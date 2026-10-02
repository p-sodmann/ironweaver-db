//! [`Store`]: the embedded, durable store, with its namespaces.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ironweaver_core::cancel::{self, Token};
use ironweaver_core::pathfinding::EdgeCost;
use ironweaver_core::{Attrs, Direction, EdgeId, GraphError, Projection};
use iwdb_engine::catalog::{AttrPath, NamespaceCatalog, NamespaceName};
use iwdb_engine::{CatalogChange, CommitResult, CommitTime, IdempotencyKey, Mutation, Namespace};
use iwdb_storage::archive::{Archive, ArchiveHandle};
use iwdb_storage::backup::{self, NamespaceSource};
use iwdb_storage::io::{LogFs, StdFs};
use iwdb_storage::layout::{create_ns_dir, remove_ns_dir, DataDir, NsPaths};
use iwdb_storage::namespaces::{EventKind, NamespaceInfo, NamespaceLog, NamespaceResult, Plan, DEFAULT_NAME};
use iwdb_storage::{
    read_namespace, recover, start_namespace, BackupReport, CheckpointOutcome, Checkpointer, Error, FsyncPolicy,
    HistoryId, LockStats, LoggedNamespace, Recovered, RecoveryReport, StoreRecovery, Wait,
};

use crate::request::{Deadline, ReadOptions, Timer};
use crate::StoreOptions;

/// The name of the namespace every store has. It is created with the store
/// (or by the first open of a store restored without it), and can't be
/// dropped: the store's shorthand methods ([`Store::commit`], ...) act on
/// it.
pub const NAMESPACE: &str = DEFAULT_NAME;

/// A node, as read from the store.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    /// Sorted by name.
    pub labels: Vec<String>,
    pub attr: Attrs,
    /// User meta (without the database's `iwdb.*` keys).
    pub meta: Attrs,
    pub version: u64,
}

/// An edge, as read from the store.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub id: EdgeId,
    pub from: String,
    pub to: String,
    pub ty: Option<String>,
    pub attr: Attrs,
    pub meta: Attrs,
    pub version: u64,
}

/// The state of an index ([`IndexStatus`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexState {
    /// Built and kept up to date by every commit.
    Ready,
    /// An online build is reading the nodes (ADR 0019); the index isn't in
    /// the catalog yet.
    Building { scanned: usize, total: usize },
}

/// One index of a namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexStatus {
    pub path: AttrPath,
    pub state: IndexState,
    /// Declared with `CreateIndex`.
    pub declared: bool,
    /// Needed by a unique constraint.
    pub unique: bool,
    /// The index's size; `None` while it is being built.
    pub size: Option<IndexSize>,
}

/// How big an index is ([`IndexStatus::size`]), from the core's
/// `Graph::index_stats` (O(1)). The namespace's indexes are flushed after
/// every commit, so the counts are exact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexSize {
    /// Nodes with an indexable (scalar) value at the path.
    pub entries: usize,
    /// Distinct values.
    pub distinct_keys: usize,
    /// Approximate bytes the index uses (its share of
    /// [`NamespaceStatus::memory_bytes`]).
    pub memory_bytes: usize,
}

/// What an open namespace reports about itself ([`Ns::status`]).
#[derive(Clone, Debug, PartialEq)]
pub struct NamespaceStatus {
    pub id: u64,
    pub name: String,
    /// When the namespace was created (0 for one that predates layout 4).
    pub created: CommitTime,
    /// The seq of the last applied commit.
    pub seq: u64,
    /// The highest seq known to be durable; `None` under
    /// [`FsyncPolicy::Off`] until an explicit sync.
    pub synced_seq: Option<u64>,
    /// The newest checkpoint's seq.
    pub checkpoint: Option<u64>,
    /// Why the namespace is read-only, if it is.
    pub read_only: Option<String>,
    /// The last checkpoint error, if the last checkpoint failed.
    pub checkpoint_failure: Option<String>,
    pub nodes: usize,
    pub edges: usize,
    /// Approximate bytes the graph uses, indexes included (the core's
    /// `Graph::memory_usage`: O(1)); each index's share is in its
    /// [`IndexSize`]. Payloads (attribute maps) are not counted.
    pub memory_bytes: usize,
    /// Declared indexes and those unique constraints need, and builds in
    /// progress, sorted by path.
    pub indexes: Vec<IndexStatus>,
    pub constraints: usize,
    /// What recovery did to this namespace when the store opened (for a
    /// namespace created since: nothing).
    pub recovery: RecoveryReport,
}

/// What an open store reports about itself ([`Store::status`]).
///
/// `seq`, `synced_seq`, `checkpoint`, `read_only` and `checkpoint_failure`
/// are those of the `default` namespace; `namespaces` has every one.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreStatus {
    /// The seq of the last applied commit of the `default` namespace.
    pub seq: u64,
    /// The highest seq of `default` known to be durable; `None` under
    /// [`FsyncPolicy::Off`], which knows of no fsync until an explicit
    /// sync (so it never claims a durable seq it doesn't have).
    pub synced_seq: Option<u64>,
    /// The newest checkpoint's seq in `default`.
    pub checkpoint: Option<u64>,
    /// Why `default` is read-only, if it is.
    pub read_only: Option<String>,
    /// The last checkpoint error in `default`, if the last checkpoint failed.
    pub checkpoint_failure: Option<String>,
    pub history: HistoryId,
    pub fsync: FsyncPolicy,
    /// The WAL archive, if the store archives.
    pub archive: Option<PathBuf>,
    /// Why namespaces can't be created or dropped until the store is
    /// reopened, if that is so (the namespace log failed).
    pub catalog_failure: Option<String>,
    /// What recovery did when the store was opened.
    pub recovery: StoreRecovery,
    /// Every namespace, by name.
    pub namespaces: Vec<NamespaceStatus>,
}

/// Options of a commit ([`Store::commit_with`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// Commit at most once under this key (ADR 0015): if the store has a
    /// commit with this key and the same request, it returns that commit's
    /// result (with [`CommitResult::deduplicated`] set) and commits
    /// nothing; with another request, it fails with
    /// `IdempotencyKeyReused`. The store remembers the last
    /// [`KEY_TABLE_CAPACITY`](iwdb_engine::idempotency::KEY_TABLE_CAPACITY)
    /// keyed commits, across restarts, checkpoints, backups and restores.
    pub idempotency_key: Option<IdempotencyKey>,
}

/// What a projection holds ([`Store::analyze`]): every node, and the edges
/// followed in `direction`, weighted by `cost`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionSpec {
    pub direction: Direction,
    pub cost: EdgeCost,
}

impl Default for ProjectionSpec {
    /// Outgoing edges, unweighted.
    fn default() -> Self {
        ProjectionSpec { direction: Direction::Out, cost: EdgeCost::Unit }
    }
}

/// The result of an analytics job, and the seq of the state it ran on.
#[derive(Clone, Debug, PartialEq)]
pub struct Analysis<R> {
    pub seq: u64,
    pub value: R,
}

/// Coordination between the store and its background threads.
#[derive(Debug, Default)]
struct Signal {
    shutdown: bool,
    /// Namespaces whose size trigger fired.
    checkpoint: BTreeSet<u64>,
}

/// One open namespace: its live state, its checkpointer and the store's
/// bookkeeping for it.
struct NsState<F: LogFs> {
    info: NamespaceInfo,
    paths: NsPaths,
    /// The namespace and its WAL: one writer, many readers (ADR 0014).
    live: LoggedNamespace<F>,
    checkpointer: Mutex<Checkpointer<F>>,
    /// The WAL's appended bytes at which the size trigger fires.
    size_trigger: AtomicU64,
    /// The last checkpoint error, cleared by a successful checkpoint.
    checkpoint_error: Mutex<Option<String>>,
    recovery: RecoveryReport,
}

/// The namespace log and the archive: what creating and dropping a
/// namespace change. One mutex, the first in the lock order.
struct CatalogState<F: LogFs> {
    log: NamespaceLog<F>,
    archive: Option<Arc<Archive<F>>>,
}

struct Shared<F: LogFs> {
    root: PathBuf,
    fs: F,
    /// The open namespaces by name. Held only to look one up or change the
    /// set; never while taking another lock, except by create and drop,
    /// which hold the catalog mutex.
    namespaces: RwLock<BTreeMap<String, Arc<NsState<F>>>>,
    catalog: Mutex<CatalogState<F>>,
    signal: Mutex<Signal>,
    wake: Condvar,
    options: StoreOptions,
}

/// Lock a mutex whatever a panicking holder left. The store's own state
/// (signals, errors, the checkpointer, whose namespace is dropped on error)
/// stays usable. The live namespaces' locks are inside
/// [`LoggedNamespace`]: every change to it aborts the process on a panic
/// (`or_abort`), and a panicking reader doesn't poison a read lock.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<F: LogFs> Shared<F> {
    fn states(&self) -> Vec<Arc<NsState<F>>> {
        self.namespaces.read().unwrap_or_else(PoisonError::into_inner).values().cloned().collect()
    }

    fn find(&self, name: &str) -> Option<Arc<NsState<F>>> {
        self.namespaces.read().unwrap_or_else(PoisonError::into_inner).get(name).cloned()
    }
}

/// An embedded Ironweaver DB store: namespaces, each a graph with its own
/// WAL and checkpoints, in a data directory.
///
/// **Namespaces** (step 9, ADR 0017). A store has named namespaces
/// ([`create_namespace`](Self::create_namespace),
/// [`drop_namespace`](Self::drop_namespace),
/// [`namespaces`](Self::namespaces), [`namespace`](Self::namespace) for a
/// handle), always including `default`. Each has its own seq space, key
/// table, indexes and constraints; a commit changes one namespace. The
/// methods of `Store` that read and write a graph (`commit`, `node`, ...)
/// are shorthands for the `default` namespace.
///
/// **Opening** ([`open`](Self::open)) takes the directory's exclusive lock
/// and recovers every namespace: from the newest checkpoint that loads,
/// plus every record in its WAL after it. The state is then that of the
/// last commit in each log, which includes every acknowledged commit the
/// fsync policy made durable (see `documentation/guarantees.md`): with
/// `always` (the default), every acknowledged commit, after any crash.
///
/// **Commits** are all or nothing, serialized per namespace (one writer
/// at a time), logged and fsynced per the policy, applied, and then
/// acknowledged. With `group`, a background thread calls `sync_due` every
/// `max_delay`, so no acknowledged commit stays unsynced much longer than
/// `2 * max_delay`.
///
/// **Checkpoints** run in a background thread (by WAL size and time), on
/// [`Ns::checkpoint`] and on [`close`](Self::close). A checkpointer
/// replays the WAL into its own copy of the namespace, so it never locks
/// the live one, and commits keep flowing while it runs.
///
/// **After a failure.** If writing or fsyncing a namespace's WAL fails, or
/// applying a logged commit fails, that namespace is **read-only until the
/// store is reopened** ([`Ns::read_only`]): reads work, commits fail with
/// [`Error::ReadOnly`]. The others go on. If the namespace log fails,
/// namespaces can't be created or dropped until reopening
/// ([`StoreStatus::catalog_failure`]).
///
/// **A panic while the store changes a namespace or its WAL** (a commit,
/// an fsync, the group commit timer) **aborts the process** (ADR 0008).
///
/// **Threads** (step 8, ADR 0014). Reads and commits take `&self`; share
/// the store between threads. Lock order: the namespace log (create, drop,
/// backup), then a namespace's checkpointer, then its writer (WAL), then
/// its namespace lock; nothing takes two namespaces' locks except a
/// backup, which takes every checkpointer in id order.
///
/// Dropping the store without [`close`](Self::close) stops the
/// background threads and releases the lock, but doesn't sync the WALs or
/// write checkpoints: like a crash, apart from the page cache.
pub struct Store<F: LogFs = StdFs>
where
    F: Send + Sync + 'static,
    F::File: Send,
{
    shared: Arc<Shared<F>>,
    threads: Vec<JoinHandle<()>>,
    /// Cancels requests at their deadline (a thread started on first use).
    timer: Timer,
    report: StoreRecovery,
    /// Holds the lock; released when the store is dropped, after the
    /// threads have stopped.
    dir: DataDir,
}

impl<F: LogFs + Send + Sync + 'static> std::fmt::Debug for Store<F>
where
    F::File: Send,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("dir", &self.dir.root()).finish_non_exhaustive()
    }
}

impl Store<StdFs> {
    /// Open (and with `create_if_missing`, create) the store in `dir`, and
    /// recover it. See the type docs for what it recovers to.
    ///
    /// Errors: [`Error::Locked`] if another store has `dir` open (in this
    /// or another process), or its archive; [`Error::NotADataDir`],
    /// [`Error::UnsupportedLayout`], [`Error::InvalidDataDir`] for a
    /// directory that isn't one this version can open; [`Error::IsBackup`]
    /// and [`Error::InterruptedRestore`]; [`Error::ArchiveMismatch`] and
    /// [`Error::NotAnArchive`] for an archive of another history;
    /// [`Error::InvalidNamespaceLog`] and [`Error::NamespaceDamaged`]; and
    /// every recovery error ([`iwdb_storage::recover`]): corruption, a WAL
    /// that doesn't reach back to any usable checkpoint, a record that
    /// fails to replay. On error nothing valid was changed and the lock is
    /// released.
    pub fn open(dir: &Path, options: StoreOptions) -> Result<Self, Error> {
        Self::open_with(StdFs, dir, options)
    }
}

impl<F: LogFs + Clone + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// [`open`](Store::open) through the file operations `fs` (tests
    /// inject faults with it).
    pub fn open_with(fs: F, dir: &Path, options: StoreOptions) -> Result<Self, Error> {
        options.checkpoint.check()?;
        let Recovered { dir, log, namespaces, report } =
            recover(fs.clone(), dir, options.create_if_missing, options.wal.clone())?;
        log_report(dir.root(), &report);
        let archive = match &options.archive {
            Some(path) => Some(Arc::new(Archive::open(fs.clone(), path, dir.history())?)),
            None => None,
        };
        let size_trigger = options.checkpoint.wal_size.unwrap_or(u64::MAX);
        let mut states = BTreeMap::new();
        for recovered in namespaces {
            let state = Arc::new(new_state(
                &fs,
                &options,
                &archive,
                recovered.info,
                recovered.paths,
                recovered.namespace,
                recovered.report,
                size_trigger,
            ));
            states.insert(state.info.name.to_string(), state);
        }
        let shared = Arc::new(Shared {
            root: dir.root().to_path_buf(),
            fs: fs.clone(),
            namespaces: RwLock::new(states),
            catalog: Mutex::new(CatalogState { log, archive }),
            signal: Mutex::new(Signal::default()),
            wake: Condvar::new(),
            options,
        });
        let mut threads = Vec::new();
        let checkpoint = &shared.options.checkpoint;
        if checkpoint.background && (checkpoint.wal_size.is_some() || checkpoint.interval.is_some()) {
            let shared = shared.clone();
            threads.push(spawn("iwdb-checkpoint", move || checkpoint_loop(&shared))?);
        }
        if let FsyncPolicy::Group { max_delay, .. } = shared.options.wal.fsync {
            if !max_delay.is_zero() {
                let shared = shared.clone();
                threads.push(spawn("iwdb-sync", move || sync_loop(&shared, max_delay))?);
            }
        }
        let store = Store { shared, threads, timer: Timer::default(), report, dir };
        {
            // A store restored without `default` gets an empty one, and the
            // archive learns the namespace log
            let mut catalog = lock(&store.shared.catalog);
            if catalog.log.table().get(&default_name()?).is_none() {
                store.create_locked(&mut catalog, &default_name()?, None)?;
            }
            store.sync_archive_log(&catalog);
        }
        Ok(store)
    }

    // ---- namespaces ----

    /// The namespace `name`, as a handle. Errors: [`Error::NoSuchNamespace`].
    pub fn namespace(&self, name: &str) -> Result<Ns<'_, F>, Error> {
        match self.shared.find(name) {
            Some(state) => Ok(Ns { store: self, state }),
            None => Err(Error::NoSuchNamespace { name: name.to_owned() }),
        }
    }

    /// The `default` namespace.
    pub fn default_namespace(&self) -> Ns<'_, F> {
        // Invariant: `default` exists (it can't be dropped, and open
        // creates it if a restored store lacks it)
        match self.shared.find(NAMESPACE) {
            Some(state) => Ns { store: self, state },
            None => unreachable_default(),
        }
    }

    /// The live namespaces, by name. Doesn't wait for any commit.
    pub fn namespaces(&self) -> Vec<NamespaceInfo> {
        let mut list: Vec<NamespaceInfo> = lock(&self.shared.catalog).log.table().live().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Create the namespace `name`: an empty graph with its own WAL, seq
    /// space and catalog.
    ///
    /// **Crash safety** (ADR 0017): its directory is made and synced first,
    /// then the create event is appended to the namespace log and fsynced;
    /// that is the commit point. A crash before leaves a directory the log
    /// doesn't list, which the next open removes; after, the namespace
    /// exists (empty). With an idempotency key, a retry after an unknown
    /// outcome returns the original event
    /// ([`NamespaceResult::deduplicated`]); the key is remembered for good
    /// (the log is never truncated).
    ///
    /// Errors: [`Error::NamespaceExists`]; an invalid name or key
    /// ([`Error::Engine`]); [`Error::IdempotencyKeyReused`] (as
    /// [`Error::Engine`]); [`Error::ReadOnly`] (the namespace log failed);
    /// [`Error::Io`] (nothing is created, or the outcome is unknown: the
    /// namespace log is failed until reopening).
    pub fn create_namespace(&self, name: &str, key: Option<&IdempotencyKey>) -> Result<NamespaceResult, Error> {
        let name = NamespaceName::new(name).map_err(iwdb_engine::Error::from)?;
        let mut catalog = lock(&self.shared.catalog);
        let result = or_abort("creating a namespace", || self.create_locked(&mut catalog, &name, key))?;
        if !result.deduplicated {
            self.sync_archive_log(&catalog);
        }
        Ok(result)
    }

    fn create_locked(
        &self,
        catalog: &mut CatalogState<F>,
        name: &NamespaceName,
        key: Option<&IdempotencyKey>,
    ) -> Result<NamespaceResult, Error> {
        if let Some(cause) = catalog.log.failure() {
            return Err(Error::ReadOnly { cause: cause.to_owned() });
        }
        let id = match catalog.log.table().plan(EventKind::Create, name, key)? {
            Plan::Duplicate(event) => return Ok(NamespaceResult { event, deduplicated: true }),
            Plan::New { id } => id,
        };
        let shared = &self.shared;
        let paths = create_ns_dir(&shared.fs, &shared.root, id)?;
        let event = catalog.log.append(EventKind::Create, id, name, key)?;
        // The namespace exists now. If opening it fails, the namespace log
        // is failed: the next open sorts it out
        let opened = (|| {
            let read = read_namespace(&shared.fs, &paths, name)?;
            start_namespace(shared.fs.clone(), &paths, read, shared.options.wal.clone())
        })();
        let (live, report) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                catalog.log.fail(format!("namespace '{}' was created but couldn't be opened: {}", name, e));
                return Err(e);
            }
        };
        let info =
            catalog.log.table().get(name).cloned().ok_or_else(|| Error::NoSuchNamespace { name: name.to_string() })?;
        let size_trigger = shared.options.checkpoint.wal_size.unwrap_or(u64::MAX);
        let state =
            Arc::new(new_state(&shared.fs, &shared.options, &catalog.archive, info, paths, live, report, size_trigger));
        shared.namespaces.write().unwrap_or_else(PoisonError::into_inner).insert(name.to_string(), state);
        Ok(NamespaceResult { event, deduplicated: false })
    }

    /// Drop the namespace `name` and everything in it. `default` can't be
    /// dropped.
    ///
    /// **Crash safety** (ADR 0017): the namespace stops accepting commits,
    /// its WAL is fsynced, and with an archive its remaining segments are
    /// archived (so a restore to a time before the drop still has them);
    /// then the drop event is appended to the namespace log and fsynced,
    /// the commit point; then its directory is removed. A crash before the
    /// event leaves the namespace; after it, it is gone, and the next open
    /// removes what is left of the directory. With an idempotency key, a
    /// retry returns the original event.
    ///
    /// **Who waits**: commits, and waits for a `min_seq`, on the namespace
    /// fail with [`Error::NamespaceDropped`]; reads and analytics that
    /// started finish on the state they read. Handles ([`Ns`]) of it fail
    /// the same way.
    ///
    /// Errors: [`Error::NoSuchNamespace`]; [`Error::InvalidOptions`] for
    /// `default`; the archive's errors (the namespace is left as it was);
    /// [`Error::Io`] from the namespace log (outcome unknown: the namespace
    /// is unavailable and the log failed until reopening).
    pub fn drop_namespace(&self, name: &str, key: Option<&IdempotencyKey>) -> Result<NamespaceResult, Error> {
        let name = NamespaceName::new(name).map_err(iwdb_engine::Error::from)?;
        let mut catalog = lock(&self.shared.catalog);
        let result = or_abort("dropping a namespace", || self.drop_locked(&mut catalog, &name, key))?;
        if !result.deduplicated {
            self.sync_archive_log(&catalog);
        }
        Ok(result)
    }

    fn drop_locked(
        &self,
        catalog: &mut CatalogState<F>,
        name: &NamespaceName,
        key: Option<&IdempotencyKey>,
    ) -> Result<NamespaceResult, Error> {
        if let Some(cause) = catalog.log.failure() {
            return Err(Error::ReadOnly { cause: cause.to_owned() });
        }
        let id = match catalog.log.table().plan(EventKind::Drop, name, key)? {
            Plan::Duplicate(event) => return Ok(NamespaceResult { event, deduplicated: true }),
            Plan::New { id } => id,
        };
        if name.as_str() == NAMESPACE {
            return Err(Error::InvalidOptions("the 'default' namespace can't be dropped".into()));
        }
        let shared = &self.shared;
        let state = shared.find(name.as_str()).ok_or_else(|| Error::NoSuchNamespace { name: name.to_string() })?;
        let checkpointer = lock(&state.checkpointer);
        state.live.mark_dropped();
        // Waits for a commit in progress; none starts after the flag
        if state.live.read_only().is_none() {
            state.live.sync()?;
        }
        if let Some(archive) = &catalog.archive {
            let segments = iwdb_storage::list_segments(&state.paths.wal)?;
            if !segments.is_empty() {
                archive.copy(id, &segments)?;
                archive.sync(id)?;
            }
        }
        let event = catalog.log.append(EventKind::Drop, id, name, key)?;
        shared.namespaces.write().unwrap_or_else(PoisonError::into_inner).remove(name.as_str());
        drop(checkpointer);
        // The drop is durable. The directory goes now, or at the next open
        if let Err(e) = remove_ns_dir(&shared.fs, &shared.root, id) {
            log::warn!(
                "{}: namespace '{}' is dropped, but its directory wasn't removed: {}",
                shared.root.display(),
                name,
                e
            );
        }
        Ok(NamespaceResult { event, deduplicated: false })
    }

    /// Bring the archive's copy of the namespace log up to date. A failure
    /// is logged, not returned: the next namespace operation or open
    /// retries, and restores only reach as far as the copy.
    fn sync_archive_log(&self, catalog: &CatalogState<F>) {
        if let Some(archive) = &catalog.archive {
            if let Err(e) = archive.write_log(catalog.log.table().events()) {
                log::warn!("{}: the archive's namespace log is stale: {}", self.shared.root.display(), e);
            }
        }
    }

    // ---- store-level ----

    /// The history of the commits this store continues (data-dir layout 2,
    /// ADR 0009): a new id for a new or restored directory. A WAL archive
    /// belongs to one history, and so do the seqs of every namespace.
    pub fn history(&self) -> HistoryId {
        self.dir.history()
    }

    /// The store's state at a glance (what `iwctl status` shows).
    pub fn status(&self) -> StoreStatus {
        let fsync = self.shared.options.wal.fsync;
        let namespaces: Vec<NamespaceStatus> =
            self.shared.states().into_iter().map(|state| Ns { store: self, state }.status()).collect();
        let default = namespaces.iter().find(|n| n.name == NAMESPACE).cloned();
        StoreStatus {
            seq: default.as_ref().map_or(0, |n| n.seq),
            synced_seq: default.as_ref().and_then(|n| n.synced_seq),
            checkpoint: default.as_ref().and_then(|n| n.checkpoint),
            read_only: default.as_ref().and_then(|n| n.read_only.clone()),
            checkpoint_failure: default.as_ref().and_then(|n| n.checkpoint_failure.clone()),
            history: self.history(),
            fsync,
            archive: self.shared.options.archive.clone(),
            catalog_failure: lock(&self.shared.catalog).log.failure().map(str::to_owned),
            recovery: self.report.clone(),
            namespaces,
        }
    }

    /// What recovery found and did when the store was opened.
    pub fn recovery(&self) -> &StoreRecovery {
        &self.report
    }

    /// Back the store up into `dest`, a missing or empty directory, while
    /// it runs: a consistent copy of its data directory up to the last
    /// commit of every namespace, which `verify` checks and
    /// [`restore`](crate::restore) turns into a store again
    /// (`documentation/formats/backup.md`, ADR 0009, ADR 0017).
    ///
    /// - **What it reaches**: every namespace's WAL is fsynced first, and
    ///   the backup holds each up to its synced seq then, which is its last
    ///   commit (a read-only namespace: its synced seq). Commits after
    ///   that are not in it. Namespaces are copied one after another, each
    ///   consistent; there are no commits across namespaces to keep apart.
    /// - **What it holds**: per namespace, every checkpoint at or below its
    ///   seq and the WAL from the oldest of them up to the seq, so it
    ///   restores to any seq in between; and the namespace log.
    /// - **Waiting**: commits wait only for the fsync. Namespaces can't be
    ///   created or dropped, and checkpoints (background, explicit and on
    ///   close) wait, until the copy is done: the backup holds the catalog
    ///   mutex and every checkpointer's lock, so that no file it copies is
    ///   removed meanwhile, and the WALs grow until then.
    /// - **Writing**: every file is fsynced, then the manifest, then the
    ///   marker, last. A backup that fails or is interrupted leaves a
    ///   directory without a marker, which a store, `verify` and `restore`
    ///   refuse; remove it and try again.
    ///
    /// Errors: [`Error::DestinationNotEmpty`]; [`Error::InvalidOptions`] if
    /// `dest` is inside the data directory; [`Error::Io`]; a WAL read error
    /// if a segment it copies is damaged; a failed fsync of a WAL (then
    /// that namespace is read-only, as after any failed fsync).
    pub fn backup(&self, dest: &Path) -> Result<BackupReport, Error> {
        let catalog = lock(&self.shared.catalog);
        let mut states = self.shared.states();
        states.sort_by_key(|s| s.info.id);
        let guards: Vec<MutexGuard<'_, Checkpointer<F>>> = states.iter().map(|s| lock(&s.checkpointer)).collect();
        let mut sources = Vec::new();
        for (state, guard) in states.iter().zip(&guards) {
            let live = &state.live;
            if live.read_only().is_none() {
                or_abort("an fsync of the WAL", || live.sync())?;
            }
            let seq = live.wal().synced_seq().min(live.seq());
            sources.push((state.clone(), seq, guard.damaged().clone()));
        }
        let namespaces: Vec<NamespaceSource<'_>> = sources
            .iter()
            .map(|(state, seq, damaged)| NamespaceSource {
                name: state.info.name.clone(),
                paths: state.paths.clone(),
                seq: *seq,
                damaged,
            })
            .collect();
        let log = std::fs::read(catalog.log.path()).map_err(|e| Error::Io {
            op: "read",
            path: catalog.log.path().to_path_buf(),
            source: e,
        })?;
        let report = backup::write_backup(&self.shared.fs, self.dir.root(), self.history(), &namespaces, &log, dest)?;
        log::info!(
            "{}: backed up {} namespaces into '{}'",
            self.dir.root().display(),
            report.namespaces.len(),
            dest.display()
        );
        Ok(report)
    }

    /// Stop the background threads, fsync every WAL, write a checkpoint of
    /// each namespace (if `on_close`), and release the lock. The lock is
    /// released even if this fails; the first error is returned after every
    /// namespace has been tried.
    ///
    /// Errors: [`Error::ReadOnly`] if a namespace is read-only (nothing
    /// more is synced; reopen to recover); a failed fsync (commits after
    /// the last completed fsync may be lost in an OS crash, per the
    /// policy); a failed checkpoint (all commits are in the synced WAL, so
    /// nothing is lost).
    pub fn close(mut self) -> Result<(), Error> {
        self.stop();
        let mut first = None;
        for state in self.shared.states() {
            let live = &state.live;
            let result = or_abort("an fsync of the WAL", || live.sync()).and_then(|()| {
                let (target, appended) = (live.seq(), live.wal().appended_bytes());
                if self.shared.options.checkpoint.on_close {
                    run_checkpoint(&self.shared, &state, target, appended).map(drop)
                } else {
                    Ok(())
                }
            });
            if let Err(e) = result {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Checkpoint every namespace (see [`Ns::checkpoint`]); the first
    /// error is returned after all were tried.
    pub fn checkpoint_all(&self) -> Result<Vec<(String, CheckpointOutcome)>, Error> {
        let mut out = Vec::new();
        let mut first = None;
        for state in self.shared.states() {
            let ns = Ns { store: self, state };
            match ns.checkpoint() {
                Ok(outcome) => out.push((ns.name().to_owned(), outcome)),
                Err(e) => {
                    first.get_or_insert(e);
                }
            }
        }
        first.map_or(Ok(out), Err)
    }

    // ---- shorthands for the default namespace ----

    /// [`Ns::commit`] on `default`.
    pub fn commit(&self, mutations: &[Mutation]) -> Result<CommitResult, Error> {
        self.default_namespace().commit(mutations)
    }

    /// [`Ns::commit_with`] on `default`.
    pub fn commit_with(&self, mutations: &[Mutation], options: &CommitOptions) -> Result<CommitResult, Error> {
        self.default_namespace().commit_with(mutations, options)
    }

    /// [`Ns::commit_catalog`] on `default`.
    pub fn commit_catalog(&self, change: CatalogChange) -> Result<CommitResult, Error> {
        self.default_namespace().commit_catalog(change)
    }

    /// [`Ns::commit_catalog_with`] on `default`.
    pub fn commit_catalog_with(&self, change: CatalogChange, options: &CommitOptions) -> Result<CommitResult, Error> {
        self.default_namespace().commit_catalog_with(change, options)
    }

    /// [`Ns::sync`] on `default`.
    pub fn sync(&self) -> Result<(), Error> {
        self.default_namespace().sync()
    }

    /// The node `id` of `default`, if it exists.
    pub fn node(&self, id: &str) -> Option<Node> {
        self.default_namespace().node(id)
    }

    /// The edge `id` of `default`, if it exists.
    pub fn edge(&self, id: EdgeId) -> Option<Edge> {
        self.default_namespace().edge(id)
    }

    /// The catalog of `default`.
    pub fn catalog(&self) -> NamespaceCatalog {
        self.default_namespace().catalog()
    }

    /// The seq of the last applied commit of `default` (0: none). Doesn't wait.
    pub fn seq(&self) -> u64 {
        self.default_namespace().seq()
    }

    /// [`Ns::synced_seq`] on `default`.
    pub fn synced_seq(&self) -> u64 {
        self.default_namespace().synced_seq()
    }

    /// [`Ns::read`] on `default`.
    pub fn read<R>(&self, f: impl FnOnce(&Namespace) -> R) -> R {
        self.default_namespace().read(f)
    }

    /// [`Ns::read_with`] on `default`.
    pub fn read_with<R>(&self, options: &ReadOptions, f: impl FnOnce(&Namespace) -> R) -> Result<R, Error> {
        self.default_namespace().read_with(options, f)
    }

    /// [`Ns::wait_for_seq`] on `default`.
    pub fn wait_for_seq(&self, seq: u64, options: &ReadOptions) -> Result<u64, Error> {
        self.default_namespace().wait_for_seq(seq, options)
    }

    /// [`Ns::analyze`] on `default`.
    pub fn analyze<R>(
        &self,
        spec: &ProjectionSpec,
        options: &ReadOptions,
        job: impl FnOnce(&Projection) -> Result<R, GraphError>,
    ) -> Result<Analysis<R>, Error> {
        self.default_namespace().analyze(spec, options, job)
    }

    /// [`Ns::lock_stats`] on `default`.
    pub fn lock_stats(&self) -> LockStats {
        self.default_namespace().lock_stats()
    }

    /// [`Ns::read_only`] on `default`.
    pub fn read_only(&self) -> Option<String> {
        self.default_namespace().read_only()
    }

    /// [`Ns::checkpoint_failure`] on `default`.
    pub fn checkpoint_failure(&self) -> Option<String> {
        self.default_namespace().checkpoint_failure()
    }

    /// [`Ns::checkpoint_seq`] on `default`.
    pub fn checkpoint_seq(&self) -> Option<u64> {
        self.default_namespace().checkpoint_seq()
    }

    /// [`Ns::checkpoint`] on `default`.
    pub fn checkpoint(&self) -> Result<CheckpointOutcome, Error> {
        self.default_namespace().checkpoint()
    }
}

fn default_name() -> Result<NamespaceName, Error> {
    Ok(NamespaceName::new(NAMESPACE).map_err(iwdb_engine::Error::from)?)
}

#[cold]
fn unreachable_default() -> ! {
    // The store's own invariant is broken: refuse to go on rather than
    // answer from a made-up namespace
    log::error!("the default namespace is missing from an open store");
    std::process::abort()
}

#[allow(clippy::too_many_arguments)]
fn new_state<F: LogFs + Clone>(
    fs: &F,
    options: &StoreOptions,
    archive: &Option<Arc<Archive<F>>>,
    info: NamespaceInfo,
    paths: NsPaths,
    live: LoggedNamespace<F>,
    recovery: RecoveryReport,
    size_trigger: u64,
) -> NsState<F> {
    let mut checkpointer = Checkpointer::new(
        fs.clone(),
        &paths,
        info.name.clone(),
        options.checkpoint.keep,
        recovery.checkpoint,
        recovery.skipped_checkpoints.iter().map(|s| s.seq),
    );
    if let Some(archive) = archive {
        checkpointer.set_archive(ArchiveHandle::new(archive.clone(), info.id));
    }
    NsState {
        info,
        paths,
        live,
        checkpointer: Mutex::new(checkpointer),
        size_trigger: AtomicU64::new(size_trigger),
        checkpoint_error: Mutex::new(None),
        recovery,
    }
}

/// A handle on one namespace of a [`Store`]: its commits, reads, catalog
/// and checkpoints. Cheap to make; holds the store borrowed, so it can't
/// outlive it. If the namespace is dropped, its methods fail with
/// [`Error::NamespaceDropped`] (commits and waits) or finish on the state
/// they had (reads).
pub struct Ns<'a, F: LogFs + Send + Sync + 'static>
where
    F::File: Send,
{
    store: &'a Store<F>,
    state: Arc<NsState<F>>,
}

impl<F: LogFs + Clone + Send + Sync + 'static> Ns<'_, F>
where
    F::File: Send,
{
    pub fn name(&self) -> &str {
        self.state.info.name.as_str()
    }

    pub fn id(&self) -> u64 {
        self.state.info.id
    }

    fn live(&self) -> &LoggedNamespace<F> {
        &self.state.live
    }

    /// Commit a data transaction: all mutations or none, validated against
    /// the state after all of them. Returns once the commit is logged,
    /// fsynced per the policy, and applied: then it is visible to reads
    /// and, with `always`, durable.
    ///
    /// Errors: an [`Error::Engine`] for an invalid or conflicting
    /// transaction (nothing changes, the namespace stays writable);
    /// [`Error::RecordTooLarge`] (likewise); [`Error::Io`] when the WAL
    /// fails (not applied, outcome unknown, the namespace is read-only
    /// now); [`Error::ReadOnly`]; [`Error::NamespaceDropped`].
    ///
    /// The result has the commit's seq, edge ids, versions and commit
    /// time (the WAL's clock, ADR 0010). A panic inside the commit aborts
    /// the process (see the [`Store`] docs).
    pub fn commit(&self, mutations: &[Mutation]) -> Result<CommitResult, Error> {
        self.commit_with(mutations, &CommitOptions::default())
    }

    /// [`commit`](Self::commit) with options: an idempotency key (see
    /// [`CommitOptions`]). A retry after an unknown outcome (an `Io`
    /// error, a timeout, a crash) with the same key applies at most once:
    /// it returns the original result if the first attempt was applied
    /// (also if it was recovered from the log after a restart), and
    /// commits now if it wasn't. Keys are per namespace (ADR 0018).
    pub fn commit_with(&self, mutations: &[Mutation], options: &CommitOptions) -> Result<CommitResult, Error> {
        let key = options.idempotency_key.as_ref();
        self.write(|live| live.commit_keyed(mutations, key))
    }

    /// Commit a catalog change (an index or a constraint), like
    /// [`commit`](Self::commit). An index the graph lacks is built online
    /// (ADR 0019): the nodes are read a chunk at a time under the read lock,
    /// without the writer's mutex, and only the log append and the install
    /// (O(nodes changed during the build)) happen under it. A commit waits
    /// for at most one chunk. Adding a constraint validates the existing
    /// data first, under the writer's mutex (reads go on).
    pub fn commit_catalog(&self, change: CatalogChange) -> Result<CommitResult, Error> {
        self.commit_catalog_with(change, &CommitOptions::default())
    }

    /// [`commit_catalog`](Self::commit_catalog) with options, like
    /// [`commit_with`](Self::commit_with).
    pub fn commit_catalog_with(&self, change: CatalogChange, options: &CommitOptions) -> Result<CommitResult, Error> {
        let key = options.idempotency_key.as_ref();
        self.write(|live| live.commit_catalog_keyed(change, key))
    }

    /// Fsync every commit so far, whatever the policy. On error the
    /// namespace is read-only.
    pub fn sync(&self) -> Result<(), Error> {
        or_abort("an fsync of the WAL", || self.live().sync())
    }

    /// The node `id`, if it exists.
    pub fn node(&self, id: &str) -> Option<Node> {
        self.read(|ns| {
            let g = ns.graph();
            let ix = g.node_ix(id)?;
            let node = g.node(ix)?;
            let mut labels: Vec<String> = g.label_names(ix)?.into_iter().map(str::to_owned).collect();
            labels.sort_unstable();
            let data = &node.data;
            Some(Node {
                id: node.id().to_owned(),
                labels,
                attr: data.attr.clone(),
                meta: data.meta.clone(),
                version: data.version,
            })
        })
    }

    /// The edge `id`, if it exists.
    pub fn edge(&self, id: EdgeId) -> Option<Edge> {
        self.read(|ns| {
            let g = ns.graph();
            let ix = g.edge_ix(id)?;
            let edge = g.edge(ix)?;
            let data = &edge.data;
            Some(Edge {
                id,
                from: g.node(edge.source())?.id().to_owned(),
                to: g.node(edge.target())?.id().to_owned(),
                ty: g.edge_type_name(ix).map(str::to_owned),
                attr: data.attr.clone(),
                meta: data.meta.clone(),
                version: data.version,
            })
        })
    }

    /// The namespace's catalog.
    pub fn catalog(&self) -> NamespaceCatalog {
        self.read(|ns| ns.catalog().clone())
    }

    /// The seq of the last applied commit (0: none). Doesn't wait.
    pub fn seq(&self) -> u64 {
        self.live().seq()
    }

    /// The highest seq known to be durable in the WAL: every commit up to
    /// it survives an OS crash. Equal to [`seq`](Self::seq) with `always`;
    /// may lag behind it with `group` and `off`. Waits for a commit's
    /// fsync in progress.
    pub fn synced_seq(&self) -> u64 {
        self.live().wal().synced_seq()
    }

    /// Read the namespace directly, under its read lock: `f` sees the state
    /// after some commit, never part of one. Other reads run meanwhile;
    /// commits wait to apply until `f` returns, so keep it short and run
    /// long jobs with [`analyze`](Self::analyze).
    pub fn read<R>(&self, f: impl FnOnce(&Namespace) -> R) -> R {
        self.live().read(f)
    }

    /// [`read`](Self::read) with options: first wait until `min_seq` is
    /// applied (read-your-writes), at most until the deadline. Errors:
    /// [`Error::OtherHistory`], [`Error::Timeout`], [`Error::Cancelled`],
    /// [`Error::ReadOnly`], [`Error::NamespaceDropped`].
    pub fn read_with<R>(&self, options: &ReadOptions, f: impl FnOnce(&Namespace) -> R) -> Result<R, Error> {
        self.wait(options, &options.deadline())?;
        Ok(self.read(f))
    }

    /// Wait until commit `seq` is applied, at most until the deadline of
    /// `options` (its `min_seq` is ignored); returns the namespace's seq
    /// then. Returns at once if `seq` is applied already.
    pub fn wait_for_seq(&self, seq: u64, options: &ReadOptions) -> Result<u64, Error> {
        let options = ReadOptions { min_seq: Some(seq), ..options.clone() };
        self.wait(&options, &options.deadline())
    }

    /// Run an analytics job on a [`Projection`] of the graph (ADR 0014):
    /// the projection is collected under a short read lock (O(n + m)), then
    /// sorted and `job` runs without any lock, so commits and reads go on
    /// meanwhile. `job` runs on this thread under a cancel token
    /// ([`cancel::run`]), which the store's timer cancels at the deadline
    /// of `options` and the caller may cancel through `options.cancel`;
    /// the core's algorithms check it and stop. Waits for `min_seq` first.
    ///
    /// Errors: those of [`read_with`](Self::read_with); [`Error::Timeout`]
    /// or [`Error::Cancelled`] if the job was stopped; a projection error
    /// (`GraphError`, for example a negative weight) or the job's own.
    pub fn analyze<R>(
        &self,
        spec: &ProjectionSpec,
        options: &ReadOptions,
        job: impl FnOnce(&Projection) -> Result<R, GraphError>,
    ) -> Result<Analysis<R>, Error> {
        let deadline = options.deadline();
        self.wait(options, &deadline)?;
        let token = options.cancel.clone().unwrap_or_default();
        let _scheduled = match deadline.at {
            Some(at) => Some(self.store.timer.schedule(at, token.clone())?),
            None => None,
        };
        let (raw, seq) = self.read(|ns| {
            let raw = Projection::collect::<_, _, GraphError>(
                ns.graph(),
                spec.direction,
                &spec.cost,
                |_, _| Ok(true),
                |_, _| Ok(true),
            );
            (raw, ns.seq())
        });
        let raw = raw.map_err(iwdb_engine::Error::from)?;
        let outcome = cancel::run(&token, || {
            let projection = raw.finish();
            job(&projection)
        });
        match outcome {
            Ok(Ok(value)) => Ok(Analysis { seq, value }),
            Ok(Err(GraphError::Interrupted)) | Err(GraphError::Interrupted) => {
                Err(stopped(&deadline, options.cancel.as_ref()))
            }
            Ok(Err(e)) | Err(e) => Err(iwdb_engine::Error::from(e).into()),
        }
    }

    /// How long commits held the namespace's write lock (apply and index
    /// flush) since the store opened.
    pub fn lock_stats(&self) -> LockStats {
        self.live().lock_stats()
    }

    /// Why the namespace is read-only, if it is: its WAL failed, or a
    /// logged commit failed to apply. Reopen the store to recover.
    pub fn read_only(&self) -> Option<String> {
        self.live().read_only()
    }

    /// The last checkpoint error, if the last checkpoint failed (cleared by
    /// the next successful one). Includes [`Error::CheckpointsDisabled`]
    /// once checkpoints are disabled until reopening.
    pub fn checkpoint_failure(&self) -> Option<String> {
        lock(&self.state.checkpoint_error).clone()
    }

    /// The seq of the newest checkpoint, if any.
    pub fn checkpoint_seq(&self) -> Option<u64> {
        lock(&self.state.checkpointer).newest()
    }

    /// Fsync the WAL, then checkpoint every commit so far and cut the WAL.
    /// Commits keep running meanwhile (they wait only for the fsync).
    /// Writes nothing if the newest checkpoint is current.
    ///
    /// If the namespace is read-only, the synced part of the log is
    /// checkpointed. Errors: a failed fsync (the namespace becomes
    /// read-only); a failed checkpoint write (nothing is deleted, the
    /// previous checkpoints are intact; the next checkpoint retries);
    /// [`Error::CheckpointsDisabled`].
    pub fn checkpoint(&self) -> Result<CheckpointOutcome, Error> {
        let live = self.live();
        if live.read_only().is_none() {
            or_abort("an fsync of the WAL", || live.sync())?;
        }
        let (target, appended) = target(live, &self.store.shared.options);
        run_checkpoint(&self.store.shared, &self.state, target, appended)
    }

    /// The namespace's state at a glance: counts, indexes, memory.
    /// O(number of indexes) and a read lock for an instant.
    pub fn status(&self) -> NamespaceStatus {
        let fsync = self.store.shared.options.wal.fsync;
        let synced = self.synced_seq();
        let builds = self.live().builds();
        let (nodes, edges, memory_bytes, catalog, sizes) = self.read(|ns| {
            let g = ns.graph();
            let sizes: BTreeMap<AttrPath, IndexSize> = ns
                .catalog()
                .index_paths()
                .into_iter()
                .filter_map(|path| {
                    let stats = g.index_stats(path.keys())?;
                    let size = IndexSize {
                        entries: stats.entries,
                        distinct_keys: stats.distinct_keys,
                        memory_bytes: stats.memory_bytes,
                    };
                    Some((path.clone(), size))
                })
                .collect();
            (g.node_count(), g.edge_count(), g.memory_usage(), ns.catalog().clone(), sizes)
        });
        let ready = |path: &AttrPath, declared: bool, unique: bool| IndexStatus {
            path: path.clone(),
            state: IndexState::Ready,
            declared,
            unique,
            size: sizes.get(path).copied(),
        };
        let mut indexes: BTreeMap<AttrPath, IndexStatus> = BTreeMap::new();
        for index in catalog.indexes() {
            indexes.insert(index.path.clone(), ready(&index.path, true, false));
        }
        for constraint in catalog.constraints() {
            if constraint.kind == iwdb_engine::catalog::ConstraintKind::Unique {
                indexes
                    .entry(constraint.path.clone())
                    .and_modify(|i| i.unique = true)
                    .or_insert_with(|| ready(&constraint.path, false, true));
            }
        }
        for build in builds {
            indexes.entry(build.path.clone()).or_insert(IndexStatus {
                path: build.path.clone(),
                state: IndexState::Building { scanned: build.scanned(), total: build.total },
                declared: true,
                unique: false,
                size: None,
            });
        }
        NamespaceStatus {
            id: self.state.info.id,
            name: self.name().to_owned(),
            created: self.state.info.created,
            seq: self.seq(),
            synced_seq: (fsync != FsyncPolicy::Off || synced > 0).then_some(synced),
            checkpoint: self.checkpoint_seq(),
            read_only: self.read_only(),
            checkpoint_failure: self.checkpoint_failure(),
            nodes,
            edges,
            memory_bytes,
            indexes: indexes.into_values().collect(),
            constraints: catalog.constraints().count(),
            recovery: self.state.recovery.clone(),
        }
    }

    /// The number of nodes with a value the index on `path` holds (the
    /// core's `Graph::index_stats`: O(1), a read lock for an instant).
    /// `None` if the graph has no index on `path`.
    pub fn index_entries(&self, path: &AttrPath) -> Option<usize> {
        self.read(|ns| ns.graph().index_stats(path.keys()).map(|stats| stats.entries))
    }

    fn write(
        &self,
        commit: impl FnOnce(&LoggedNamespace<F>) -> Result<CommitResult, Error>,
    ) -> Result<CommitResult, Error> {
        let live = self.live();
        let result = or_abort("a commit", || commit(live))?;
        let shared = &self.store.shared;
        let trigger = self.state.size_trigger.load(Ordering::Relaxed);
        if !result.deduplicated && live.wal().appended_bytes() >= trigger {
            lock(&shared.signal).checkpoint.insert(self.state.info.id);
            shared.wake.notify_all();
        }
        Ok(result)
    }

    /// Check `options.history` and wait for `options.min_seq` until the
    /// deadline. Returns the seq reached.
    fn wait(&self, options: &ReadOptions, deadline: &Deadline) -> Result<u64, Error> {
        let history = self.store.history();
        if let Some(given) = options.history.filter(|h| *h != history) {
            return Err(Error::OtherHistory { given, store: history });
        }
        let Some(min_seq) = options.min_seq else { return Ok(self.seq()) };
        let cancelled = || options.cancel.as_ref().is_some_and(Token::is_cancelled);
        match self.live().wait_for_seq(min_seq, deadline.at, &cancelled) {
            Wait::Reached(seq) => Ok(seq),
            Wait::TimedOut(seq) => {
                Err(deadline.timeout(&format!("waiting for seq {} (the namespace is at {})", min_seq, seq)))
            }
            Wait::Cancelled => Err(Error::Cancelled),
            Wait::ReadOnly(cause) => Err(Error::ReadOnly { cause }),
            Wait::Dropped => Err(Error::NamespaceDropped { name: self.name().to_owned() }),
        }
    }
}

/// Why a job stopped: its deadline passed, or the caller cancelled it.
fn stopped(deadline: &Deadline, cancel: Option<&Token>) -> Error {
    if deadline.passed() || !cancel.is_some_and(Token::is_cancelled) {
        deadline.timeout("the analytics job")
    } else {
        Error::Cancelled
    }
}

impl<F: LogFs + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// Stop and join the background threads.
    fn stop(&mut self) {
        self.timer.stop();
        lock(&self.shared.signal).shutdown = true;
        self.shared.wake.notify_all();
        for thread in self.threads.drain(..) {
            // A panicked thread has nothing left to clean up
            let _ = thread.join();
        }
    }
}

impl<F: LogFs + Send + Sync + 'static> Drop for Store<F>
where
    F::File: Send,
{
    /// Stops the background threads (waiting for a running checkpoint) and
    /// releases the lock. Doesn't sync or checkpoint; see
    /// [`close`](Store::close).
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run `f`, which changes the live namespace or its WAL, and abort the
/// process if it panics (ADR 0008). After such a panic the namespace may
/// hold part of a transaction, or the WAL writer's position may disagree
/// with its file; only recovery from the checkpoint and the WAL restores a
/// consistent state, so the panic is turned into a crash. The panic
/// message has been printed by the panic hook already.
fn or_abort<R>(what: &str, f: impl FnOnce() -> R) -> R {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(no message)");
            log::error!("{} panicked, aborting the process: {}", what, message);
            eprintln!(
                "iwdb: {} panicked ({}); aborting the process. The next open recovers every logged commit.",
                what, message
            );
            std::process::abort()
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, Error> {
    thread::Builder::new().name(name.to_owned()).spawn(f).map_err(|e| Error::Io {
        op: "spawn thread",
        path: name.into(),
        source: e,
    })
}

/// The highest seq a checkpoint may cover: a synced one, so that no OS
/// crash can leave a checkpoint newer than the log. With `off` nothing is
/// ever synced, and the policy already accepts that an OS crash can make
/// the store unrecoverable, so it is the last applied seq.
/// Also returns the WAL's appended bytes, for the size trigger.
fn target<F: LogFs>(live: &LoggedNamespace<F>, options: &StoreOptions) -> (u64, u64) {
    // The seq first: the WAL may be ahead of it (appended, not applied yet)
    let seq = live.seq();
    let wal = live.wal();
    let target = match options.wal.fsync {
        FsyncPolicy::Off => seq,
        _ => wal.synced_seq().min(seq),
    };
    (target, wal.appended_bytes())
}

/// Run the namespace's checkpointer to `target`; remember the outcome, and
/// move the size trigger `wal_size` bytes past `appended` (also after a
/// failure, so that a failing checkpoint isn't retried on every commit).
fn run_checkpoint<F: LogFs>(
    shared: &Shared<F>,
    state: &NsState<F>,
    target: u64,
    appended: u64,
) -> Result<CheckpointOutcome, Error> {
    let result = lock(&state.checkpointer).run(target);
    if let Some(size) = shared.options.checkpoint.wal_size {
        state.size_trigger.store(appended.saturating_add(size), Ordering::Relaxed);
    }
    let mut last_error = lock(&state.checkpoint_error);
    match &result {
        Ok(outcome) => {
            *last_error = None;
            log::debug!("{}: checkpoint at seq {} ({:?})", state.info.name, outcome.seq, outcome);
        }
        Err(e) => {
            let message = e.to_string();
            if last_error.as_deref() != Some(message.as_str()) {
                log::warn!("{}: checkpoint to seq {} failed: {}", state.info.name, target, message);
            }
            *last_error = Some(message);
        }
    }
    result
}

/// The background checkpointer: waits for a size trigger or the interval,
/// then checkpoints the synced part of the logs of the namespaces that
/// triggered (all of them, at the interval).
fn checkpoint_loop<F: LogFs>(shared: &Shared<F>) {
    let interval = shared.options.checkpoint.interval;
    let mut last = Instant::now();
    loop {
        let requested: Option<BTreeSet<u64>>;
        {
            let mut signal = lock(&shared.signal);
            loop {
                if signal.shutdown {
                    return;
                }
                if !signal.checkpoint.is_empty() {
                    requested = Some(std::mem::take(&mut signal.checkpoint));
                    break;
                }
                match interval {
                    Some(interval) if last.elapsed() >= interval => {
                        requested = None;
                        break;
                    }
                    Some(interval) => {
                        let wait = interval.saturating_sub(last.elapsed());
                        signal = shared.wake.wait_timeout(signal, wait).unwrap_or_else(PoisonError::into_inner).0;
                    }
                    None => signal = shared.wake.wait(signal).unwrap_or_else(PoisonError::into_inner),
                }
            }
        }
        if requested.is_none() {
            last = Instant::now();
        }
        for state in shared.states() {
            if requested.as_ref().is_some_and(|ids| !ids.contains(&state.info.id)) || state.live.is_dropped() {
                continue;
            }
            let (target, appended) = target(&state.live, &shared.options);
            // Errors are kept in `checkpoint_error` and logged
            let _ = run_checkpoint(shared, &state, target, appended);
        }
    }
}

/// The group commit timer: every `period`, fsync the records that have
/// waited `max_delay` (the period itself), in every namespace.
fn sync_loop<F: LogFs>(shared: &Shared<F>, period: Duration) {
    let period = period.max(Duration::from_millis(1));
    loop {
        {
            let signal = lock(&shared.signal);
            if signal.shutdown {
                return;
            }
            let (signal, _) = shared.wake.wait_timeout(signal, period).unwrap_or_else(PoisonError::into_inner);
            if signal.shutdown {
                return;
            }
        }
        for state in shared.states() {
            let live = &state.live;
            if live.read_only().is_some() || live.is_dropped() {
                continue;
            }
            if let Err(e) = or_abort("the group commit fsync", || live.sync_due()) {
                log::error!(
                    "group commit fsync failed, namespace '{}' is read-only until reopened: {}",
                    state.info.name,
                    e
                );
            }
        }
    }
}

/// Log what recovery found that is worth attention.
fn log_report(root: &Path, report: &StoreRecovery) {
    let dir = root.display();
    if let Some(cut) = &report.cut_log {
        log::info!(
            "{}: cut a torn tail ({}) off the namespace log at {} of {} bytes",
            dir,
            cut.reason,
            cut.valid_len,
            cut.file_len
        );
    }
    for id in &report.removed_orphans {
        log::info!("{}: removed the directory of namespace {}, which the log doesn't list", dir, id);
    }
    for (name, ns) in &report.namespaces {
        for skipped in &ns.skipped_checkpoints {
            log::warn!("{}: namespace '{}': skipped damaged checkpoint {}: {}", dir, name, skipped.seq, skipped.reason);
        }
        let changes = &ns.index_changes;
        if !changes.created.is_empty() || !changes.dropped.is_empty() {
            log::warn!(
                "{}: namespace '{}': the checkpoint's indexes differed from its catalog: {:?}",
                dir,
                name,
                changes
            );
        }
        if let Some(tail) = &ns.torn_tail {
            let level = if tail.discarded_frames > 0 { log::Level::Warn } else { log::Level::Info };
            log::log!(
                level,
                "{}: namespace '{}': cut a torn WAL tail ({:?}) off '{}' at {} of {} bytes, discarding {} later frames",
                dir,
                name,
                tail.damage,
                tail.path.display(),
                tail.valid_len,
                tail.file_len,
                tail.discarded_frames
            );
        }
        log::info!(
            "{}: namespace '{}' recovered to seq {} (checkpoint {:?}, {} WAL records replayed)",
            dir,
            name,
            ns.seq,
            ns.checkpoint,
            ns.replayed
        );
    }
    for path in &report.removed_temp_files {
        log::info!("{}: removed stale temporary file '{}'", dir, path.display());
    }
}
