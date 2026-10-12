//! [`Store`]: the embedded, durable store, with its namespaces.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::JoinHandle;

use ironweaver_core::{EdgeId, GraphError, Projection};
use iwdb_engine::catalog::{NamespaceCatalog, NamespaceName};
use iwdb_engine::metrics::Histogram;
use iwdb_engine::{CatalogChange, CommitResult, CommitTime, Mutation, Namespace};
use iwdb_query::{CommitOptions, Edge, NamespaceStatus, Node, ProjectionSpec};
use iwdb_storage::archive::{Archive, ArchiveHandle};
use iwdb_storage::io::{LogFs, StdFs};
use iwdb_storage::layout::{DataDir, NsPaths};
use iwdb_storage::memory::{Memory, MemorySnapshot};
use iwdb_storage::namespaces::{DEFAULT_NAME, NamespaceInfo, NamespaceLog};
use iwdb_storage::{
    CheckpointOutcome, Checkpointer, Error, FsyncPolicy, HistoryId, LockStats, LoggedNamespace, OffsetIndex, Recovered,
    RecoveryReport, StoreRecovery, recover,
};

use crate::StoreOptions;
use crate::request::{ReadOptions, Timer};

mod background;
mod import;
mod lifecycle;
mod maintenance;
mod ns;
mod projections;
mod wait;

use background::{checkpoint_loop, or_abort, run_checkpoint, spawn, sync_loop};
pub use ns::{DiskUsage, NamespaceHistograms, Ns};
pub(crate) use wait::StreamableWait;

/// The name of the namespace every store has. It is created with the store
/// (or by the first open of a store restored without it), and can't be
/// dropped: the store's shorthand methods ([`Store::commit`], ...) act on
/// it.
pub const NAMESPACE: &str = DEFAULT_NAME;

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
    /// Frame offsets in the WAL, for the change stream (ADR 0031).
    offsets: OffsetIndex,
    /// The WAL's appended bytes at which the size trigger fires.
    size_trigger: AtomicU64,
    /// The last checkpoint error, cleared by a successful checkpoint.
    checkpoint_error: Mutex<Option<String>>,
    recovery: RecoveryReport,
    /// How long checkpoint runs that wrote one took (the metrics).
    checkpoints: Histogram,
    /// When the newest checkpoint was written: its file's modification
    /// time when the store opened, then the end of each written one.
    last_checkpoint: Mutex<Option<CommitTime>>,
    /// The checkpointer's newest checkpoint ([`NO_CHECKPOINT`]: none),
    /// readable while a checkpoint runs.
    checkpoint_seq: AtomicU64,
    /// The namespace's own limits (step 15d), as stored in the system
    /// namespace; the store's defaults fill what is unset.
    own_limits: Mutex<iwdb_query::NamespaceLimits>,
}

/// [`NsState::checkpoint_seq`] without a checkpoint.
const NO_CHECKPOINT: u64 = u64::MAX;

impl<F: LogFs> NsState<F> {
    fn set_checkpoint_seq(&self, seq: Option<u64>) {
        self.checkpoint_seq.store(seq.unwrap_or(NO_CHECKPOINT), std::sync::atomic::Ordering::Release);
    }

    fn checkpoint_seq(&self) -> Option<u64> {
        Some(self.checkpoint_seq.load(std::sync::atomic::Ordering::Acquire)).filter(|&s| s != NO_CHECKPOINT)
    }
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
    /// The projections started on the store (ADR 0032), stopped first
    /// when it stops.
    projections: Mutex<Vec<Arc<crate::projection::Control>>>,
    /// Serializes changes to users, grants and tokens (`crate::auth`): each
    /// reads the system namespace and commits what it read plus the change.
    auth: Mutex<()>,
    /// What the store's memory holds, and its limit (ADR 0054).
    memory: Arc<Memory>,
    /// Backups so far (the metrics, ADR 0055).
    backups: Backups,
}

/// Backups of a store since it opened: what the metrics report (ADR 0055).
#[derive(Debug, Default)]
struct Backups {
    /// Backups copying now.
    running: AtomicU64,
    /// Bytes written by backups, counted as they are written.
    bytes: AtomicU64,
    ok: AtomicU64,
    failed: AtomicU64,
    /// When the last successful one finished.
    last: Mutex<Option<CommitTime>>,
}

/// Backups of a store since it opened ([`Store::backup_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackupStats {
    /// Backups copying now (checkpoints wait for them).
    pub running: u64,
    /// Bytes written by backups, counted as they are written.
    pub bytes: u64,
    /// Backups that finished, and that failed.
    pub ok: u64,
    pub failed: u64,
    /// When the last successful backup finished.
    pub last: Option<CommitTime>,
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
/// **Namespaces** (ADR 0017). A store has named namespaces
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
/// **Threads** (ADR 0014). Reads and commits take `&self`; share
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
        options.memory.check().map_err(Error::InvalidOptions)?;
        let memory = Memory::new(&options.memory);
        if let Some(limit) = memory.limit() {
            log::info!(
                "{}: memory limit {} bytes ({}): warning at {} bytes, refusing writes at {} bytes",
                dir.display(),
                limit.bytes,
                limit.source.as_str(),
                limit.warn,
                limit.refuse_writes
            );
        }
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
            // An imported namespace's checkpoint, if the archive lacks it
            // (ADR 0033): a crash after the import's create event, or an
            // archive set up after the import
            if let Some(archive) = &archive
                && let Err(e) = iwdb_storage::import::archive_base(archive, recovered.info.id, &recovered.paths)
            {
                log::warn!("namespace '{}': its import isn't in the WAL archive: {}", recovered.info.name, e);
            }
            let state = Arc::new(new_state(
                &fs,
                &options,
                &memory,
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
            projections: Mutex::new(Vec::new()),
            auth: Mutex::new(()),
            memory,
            backups: Backups::default(),
        });
        let mut threads = Vec::new();
        let checkpoint = &shared.options.checkpoint;
        if checkpoint.background && (checkpoint.wal_size.is_some() || checkpoint.interval.is_some()) {
            let shared = shared.clone();
            threads.push(spawn("iwdb-checkpoint", move || checkpoint_loop(&shared))?);
        }
        if let FsyncPolicy::Group { max_delay, .. } = shared.options.wal.fsync
            && !max_delay.is_zero()
        {
            let shared = shared.clone();
            threads.push(spawn("iwdb-sync", move || sync_loop(&shared, max_delay))?);
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
        store.load_namespace_limits();
        Ok(store)
    }

    /// Apply the namespaces' own limits from the system namespace (step
    /// 15d). Records that can't be read leave the defaults, with an error
    /// in the log: the store still opens, as it does for users it can't
    /// read, and every auth operation reports the problem.
    fn load_namespace_limits(&self) {
        let records = match self.users().namespace_limit_records() {
            Ok(records) => records,
            Err(e) => {
                log::error!("the namespaces' limits can't be read, so only the defaults apply: {}", e);
                return;
            }
        };
        for ns in self.open_namespaces() {
            if let Some(limits) = records.get(&ns.id()) {
                ns.set_own_limits(*limits);
            }
        }
    }

    // ---- namespaces ----

    /// The namespace `name`, as a handle. Errors: [`Error::NoSuchNamespace`]
    /// (also for the reserved system namespace, which only
    /// [`users`](Self::users) reads and writes, ADR 0043).
    /// What the store's memory holds, its limit and state (ADR 0054). O(1),
    /// no lock.
    pub fn memory(&self) -> MemorySnapshot {
        self.shared.memory.snapshot()
    }

    /// The store's memory accounting, for holders of memory outside the
    /// namespaces (the managed jobs' stored results, ADR 0056).
    pub(crate) fn memory_handle(&self) -> Arc<Memory> {
        self.shared.memory.clone()
    }

    /// The limits of namespaces without their own
    /// ([`StoreOptions::namespace_limits`]).
    pub fn namespace_defaults(&self) -> iwdb_query::NamespaceLimits {
        self.shared.options.namespace_limits
    }

    /// The WALs' fsync policy.
    pub fn fsync_policy(&self) -> FsyncPolicy {
        self.shared.options.wal.fsync
    }

    /// Bytes free for an unprivileged process on the data directory's file
    /// system (`statvfs`: available blocks times the fragment size; on
    /// Windows `GetDiskFreeSpaceExW`'s bytes available to the caller, which
    /// honours quotas); `None` where that can't be read (or on a platform
    /// without either).
    pub fn disk_free(&self) -> Option<u64> {
        #[cfg(unix)]
        {
            let stat = rustix::fs::statvfs(&self.shared.root).ok()?;
            Some(stat.f_bavail.saturating_mul(stat.f_frsize))
        }
        #[cfg(windows)]
        {
            fs4::available_space(&self.shared.root).ok()
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
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
        let namespaces: Vec<NamespaceStatus> = self
            .shared
            .states()
            .into_iter()
            .filter(|state| !state.info.name.is_reserved())
            .map(|state| Ns { store: self, state }.status())
            .collect();
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

    /// The data directory.
    pub fn root(&self) -> &Path {
        self.dir.root()
    }

    /// The WAL archive the store copies segments into
    /// ([`StoreOptions::archive`]), if it has one.
    pub fn archive_dir(&self) -> Option<&Path> {
        self.shared.options.archive.as_deref()
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

/// A namespace name a caller gave: valid, and not the reserved system
/// namespace's.
pub(crate) fn public_name(name: &str) -> Result<NamespaceName, Error> {
    let name = NamespaceName::new(name).map_err(iwdb_engine::Error::from)?;
    if name.is_reserved() {
        return Err(iwdb_engine::Error::from(iwdb_engine::catalog::CatalogError::InvalidNamespaceName {
            name: name.to_string(),
            reason: "reserved for the store's users and grants",
        })
        .into());
    }
    Ok(name)
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
    memory: &Arc<Memory>,
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
    checkpointer.set_retention(options.retention);
    checkpointer.set_memory(memory);
    let live = live.with_memory(memory);
    live.set_memory_limit(options.namespace_limits.memory());
    let last_checkpoint = recovery.checkpoint.and_then(|seq| checkpoint_time(&paths, seq));
    let checkpoint_seq = AtomicU64::new(checkpointer.newest().unwrap_or(NO_CHECKPOINT));
    NsState {
        checkpoint_seq,
        checkpoints: Histogram::new(),
        last_checkpoint: Mutex::new(last_checkpoint),
        info,
        paths,
        live,
        checkpointer: Mutex::new(checkpointer),
        offsets: OffsetIndex::new(),
        size_trigger: AtomicU64::new(size_trigger),
        checkpoint_error: Mutex::new(None),
        recovery,
        own_limits: Mutex::new(iwdb_query::NamespaceLimits::default()),
    }
}

/// The modification time of checkpoint `seq` in `paths`, if it can be read.
fn checkpoint_time(paths: &NsPaths, seq: u64) -> Option<CommitTime> {
    let path = paths.checkpoints.join(iwdb_storage::checkpoint::checkpoint_name(seq));
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let micros = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_micros();
    Some(CommitTime(i64::try_from(micros).ok()?))
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
