//! [`Store`]: the embedded, durable store.

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ironweaver_core::{Attrs, EdgeId};
use iwdb_engine::catalog::{NamespaceCatalog, NamespaceName};
use iwdb_engine::{CatalogChange, CommitResult, Mutation, Namespace};
use iwdb_storage::archive::Archive;
use iwdb_storage::io::{LogFs, StdFs};
use iwdb_storage::layout::DataDir;
use iwdb_storage::{
    backup, recover, BackupReport, CheckpointOutcome, Checkpointer, Error, FsyncPolicy, HistoryId, LoggedNamespace,
    Recovered, RecoveryReport,
};

use crate::StoreOptions;

/// The name of the store's one namespace. Step 9 adds more.
pub const NAMESPACE: &str = "default";

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

/// Coordination between the store and its background threads.
#[derive(Debug, Default)]
struct Signal {
    shutdown: bool,
    /// The size trigger fired.
    checkpoint: bool,
}

struct Shared<F: LogFs> {
    live: Mutex<LoggedNamespace<F>>,
    checkpointer: Mutex<Checkpointer<F>>,
    signal: Mutex<Signal>,
    wake: Condvar,
    /// The WAL's appended bytes at which the size trigger fires.
    size_trigger: AtomicU64,
    /// The last checkpoint error, cleared by a successful checkpoint.
    checkpoint_error: Mutex<Option<String>>,
    options: StoreOptions,
}

/// Lock a mutex whatever a panicking holder left. The store's own state
/// (signals, errors, the checkpointer, whose namespace is dropped on error)
/// stays usable, and so does the live namespace: every change to it aborts
/// the process on a panic (`or_abort`), so only a read can have poisoned it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An embedded Ironweaver DB store: one namespace in a data directory,
/// durable through a write-ahead log and checkpoints.
///
/// **Opening** ([`open`](Self::open)) takes the directory's exclusive lock
/// and recovers: from the newest checkpoint that loads, plus every record
/// in the WAL after it. The state is then that of the last commit in the
/// log, which includes every acknowledged commit the fsync policy made
/// durable (see `documentation/guarantees.md`): with `always` (the
/// default), every acknowledged commit, after any crash.
///
/// **Commits** ([`commit`](Self::commit),
/// [`commit_catalog`](Self::commit_catalog)) are all or nothing, serialized
/// (one writer at a time), logged and fsynced per the policy, applied, and
/// then acknowledged. With `group`, a background thread calls
/// `sync_due` every `max_delay`, so no acknowledged commit stays unsynced
/// much longer than `2 * max_delay`.
///
/// **Checkpoints** run in a background thread (by WAL size and time), on
/// [`checkpoint`](Self::checkpoint) and on [`close`](Self::close). The
/// checkpointer replays the WAL into its own copy of the namespace, so it
/// never locks the live one, and commits keep flowing while it runs. It
/// only checkpoints synced commits, then deletes the WAL segments that the
/// oldest kept checkpoint covers.
///
/// **After a failure.** If writing or fsyncing the WAL fails, or applying
/// a logged commit fails, the store is **read-only until reopened**
/// ([`read_only`](Self::read_only)): reads work, commits fail with
/// [`Error::ReadOnly`]. Reopening runs recovery. A failed checkpoint
/// doesn't affect commits ([`checkpoint_failure`](Self::checkpoint_failure)).
///
/// **A panic while the store changes its namespace or WAL** (a commit, an
/// fsync, the group commit timer) **aborts the process** (ADR 0008): the
/// namespace may hold part of a transaction, or the WAL writer's state may
/// disagree with its file, and no reader may see either. It is a crash;
/// the next open recovers every logged commit. Such a panic is a bug (for
/// example upstream #28). A panic in a [`read`](Self::read) closure
/// changes nothing and only unwinds the caller.
///
/// Reads and commits take `&self`; the store can be shared between
/// threads, but a read waits while a commit runs (concurrent readers come
/// in step 8). Dropping the store without [`close`](Self::close) stops the
/// background threads and releases the lock, but doesn't sync the WAL or
/// write a checkpoint: like a crash, apart from the page cache.
pub struct Store<F: LogFs = StdFs>
where
    F: Send + Sync + 'static,
    F::File: Send,
{
    shared: Arc<Shared<F>>,
    threads: Vec<JoinHandle<()>>,
    report: RecoveryReport,
    /// The file operations, for backups.
    fs: F,
    /// Holds the lock; released when the store is dropped, after the
    /// threads have stopped.
    dir: DataDir,
}

impl<F: LogFs + Send + Sync + 'static> std::fmt::Debug for Store<F>
where
    F::File: Send,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir.root())
            .field("seq", &lock(&self.shared.live).namespace().seq())
            .finish_non_exhaustive()
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
    /// [`Error::NotAnArchive`] for an archive of another history; and every recovery
    /// error ([`iwdb_storage::recover`]): corruption, a WAL that doesn't
    /// reach back to any usable checkpoint, a record that fails to replay.
    /// On error nothing valid was changed and the lock is released.
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
        let name = NamespaceName::new(NAMESPACE).map_err(iwdb_engine::Error::from)?;
        let Recovered { dir, namespace, report } =
            recover(fs.clone(), dir, options.create_if_missing, &name, options.wal.clone())?;
        log_report(dir.root(), &report);
        let mut checkpointer = Checkpointer::new(
            fs.clone(),
            &dir,
            name,
            options.checkpoint.keep,
            report.checkpoint,
            report.skipped_checkpoints.iter().map(|s| s.seq),
        );
        if let Some(path) = &options.archive {
            checkpointer.set_archive(Archive::open(fs.clone(), path, dir.history())?);
        }
        let size_trigger = options.checkpoint.wal_size.unwrap_or(u64::MAX);
        let shared = Arc::new(Shared {
            live: Mutex::new(namespace),
            checkpointer: Mutex::new(checkpointer),
            signal: Mutex::new(Signal::default()),
            wake: Condvar::new(),
            size_trigger: AtomicU64::new(size_trigger),
            checkpoint_error: Mutex::new(None),
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
        Ok(Store { shared, threads, report, fs, dir })
    }

    /// Commit a data transaction: all mutations or none, validated against
    /// the state after all of them. Returns once the commit is logged,
    /// fsynced per the policy, and applied: then it is visible to reads
    /// and, with `always`, durable.
    ///
    /// Errors: an [`Error::Engine`] for an invalid or conflicting
    /// transaction (nothing changes, the store stays writable);
    /// [`Error::RecordTooLarge`] (likewise); [`Error::Io`] when the WAL
    /// fails (not applied, outcome unknown, the store is read-only now);
    /// [`Error::ReadOnly`].
    ///
    /// A panic inside the commit aborts the process (see the type docs).
    pub fn commit(&self, mutations: &[Mutation]) -> Result<CommitResult, Error> {
        self.write(|live| live.commit(mutations))
    }

    /// Commit a catalog change (an index or a constraint), like
    /// [`commit`](Self::commit).
    pub fn commit_catalog(&self, change: CatalogChange) -> Result<CommitResult, Error> {
        self.write(|live| live.commit_catalog(change))
    }

    /// Fsync every commit so far, whatever the policy. On error the store
    /// is read-only.
    pub fn sync(&self) -> Result<(), Error> {
        let mut live = self.live()?;
        or_abort("an fsync of the WAL", || live.sync())
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

    /// The seq of the last applied commit (0: none).
    pub fn seq(&self) -> u64 {
        self.read(Namespace::seq)
    }

    /// The highest seq known to be durable in the WAL: every commit up to
    /// it survives an OS crash. Equal to [`seq`](Self::seq) with `always`;
    /// may lag behind it with `group` and `off`.
    pub fn synced_seq(&self) -> u64 {
        lock(&self.shared.live).wal().synced_seq()
    }

    /// Read the namespace directly, under the store's lock (commits wait
    /// meanwhile). Until the query layer (step 10), this is how to run
    /// graph queries on the store.
    pub fn read<R>(&self, f: impl FnOnce(&Namespace) -> R) -> R {
        f(lock(&self.shared.live).namespace())
    }

    /// Why the store is read-only, if it is: the WAL failed, or a logged
    /// commit failed to apply. Reopen it to recover.
    pub fn read_only(&self) -> Option<String> {
        lock(&self.shared.live).read_only()
    }

    /// The history of commits this store continues (data-dir layout 2,
    /// ADR 0009): a new id for a new or restored directory. A WAL archive
    /// belongs to one history.
    pub fn history(&self) -> HistoryId {
        self.dir.history()
    }

    /// What recovery found and did when the store was opened.
    pub fn recovery(&self) -> &RecoveryReport {
        &self.report
    }

    /// The last checkpoint error, if the last checkpoint failed (cleared by
    /// the next successful one). Includes [`Error::CheckpointsDisabled`]
    /// once checkpoints are disabled until reopening.
    pub fn checkpoint_failure(&self) -> Option<String> {
        lock(&self.shared.checkpoint_error).clone()
    }

    /// The seq of the newest checkpoint, if any.
    pub fn checkpoint_seq(&self) -> Option<u64> {
        lock(&self.shared.checkpointer).newest()
    }

    /// Fsync the WAL, then checkpoint every commit so far and cut the WAL.
    /// Commits keep running meanwhile (they wait only for the fsync).
    /// Writes nothing if the newest checkpoint is current.
    ///
    /// If the store is read-only, the synced part of the log is
    /// checkpointed. Errors: a failed fsync (the store becomes read-only);
    /// a failed checkpoint write (nothing is deleted, the previous
    /// checkpoints are intact; the next checkpoint retries);
    /// [`Error::CheckpointsDisabled`].
    pub fn checkpoint(&self) -> Result<CheckpointOutcome, Error> {
        let (target, appended) = {
            let mut live = lock(&self.shared.live);
            if live.read_only().is_none() {
                or_abort("an fsync of the WAL", || live.sync())?;
            }
            (target(&live, &self.shared.options), live.wal().appended_bytes())
        };
        run_checkpoint(&self.shared, target, appended)
    }

    /// Back the store up into `dest`, a missing or empty directory, while
    /// it runs: a consistent copy of its data directory up to the last
    /// commit, which `verify` checks and [`restore`](crate::restore) turns
    /// into a store again (`documentation/formats/backup.md`, ADR 0009).
    ///
    /// - **What it reaches**: the WAL is fsynced first, and the backup holds
    ///   every commit up to the synced seq then, which is the last commit
    ///   (if the store is read-only, nothing more is synced: the commits up
    ///   to its synced seq). Commits after that are not in it.
    /// - **What it holds**: every checkpoint at or below that seq, and the
    ///   WAL from the oldest of them up to the seq, so it restores to any
    ///   seq in between.
    /// - **Waiting**: commits wait only for the fsync, as in
    ///   [`checkpoint`](Self::checkpoint). Checkpoints (background,
    ///   explicit and on close) wait until the copy is done: the backup
    ///   holds the checkpointer's lock, so that no file it copies is
    ///   removed meanwhile, and the WAL grows until then.
    /// - **Writing**: every file is fsynced, then the manifest, then the
    ///   marker, last. A backup that fails or is interrupted leaves a
    ///   directory without a marker, which a store, `verify` and `restore`
    ///   refuse; remove it and try again.
    ///
    /// Errors: [`Error::DestinationNotEmpty`]; [`Error::InvalidOptions`] if
    /// `dest` is inside the data directory; [`Error::Io`]; a WAL read error
    /// if a segment it copies is damaged; a failed fsync of the WAL (then
    /// the store is read-only, as after any failed fsync).
    pub fn backup(&self, dest: &Path) -> Result<BackupReport, Error> {
        let checkpointer = lock(&self.shared.checkpointer);
        let seq = {
            let mut live = self.live()?;
            if live.read_only().is_none() {
                or_abort("an fsync of the WAL", || live.sync())?;
            }
            live.wal().synced_seq().min(live.namespace().seq())
        };
        let damaged = checkpointer.damaged();
        let report = backup::write_backup(&self.fs, self.dir.root(), self.history(), seq, damaged, dest)?;
        log::info!("{}: backed up to seq {} into '{}'", self.dir.root().display(), report.seq, dest.display());
        Ok(report)
    }

    /// Stop the background threads, fsync the WAL, write a checkpoint (if
    /// `on_close`), and release the lock. The lock is released even if
    /// this fails.
    ///
    /// Errors: [`Error::ReadOnly`] if the store is read-only (nothing more
    /// is synced; reopen to recover); a failed fsync (commits after the last
    /// completed fsync may be lost in an OS crash, per the policy); a failed
    /// checkpoint (all commits are in the synced WAL, so nothing is lost).
    pub fn close(mut self) -> Result<(), Error> {
        self.stop();
        let (target, appended) = {
            let mut live = self.live()?;
            or_abort("an fsync of the WAL", || live.sync())?;
            (live.namespace().seq(), live.wal().appended_bytes())
        };
        if self.shared.options.checkpoint.on_close {
            run_checkpoint(&self.shared, target, appended)?;
        }
        Ok(())
    }

    fn write(
        &self,
        commit: impl FnOnce(&mut LoggedNamespace<F>) -> Result<CommitResult, Error>,
    ) -> Result<CommitResult, Error> {
        let mut live = self.live()?;
        let result = or_abort("a commit", || commit(&mut live))?;
        if live.wal().appended_bytes() >= self.shared.size_trigger.load(Ordering::Relaxed) {
            lock(&self.shared.signal).checkpoint = true;
            self.shared.wake.notify_all();
        }
        Ok(result)
    }

    /// The live namespace for writing. A read that panicked may have
    /// poisoned the mutex; it changed nothing (see [`lock`]).
    fn live(&self) -> Result<MutexGuard<'_, LoggedNamespace<F>>, Error> {
        Ok(lock(&self.shared.live))
    }
}

impl<F: LogFs + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// Stop and join the background threads.
    fn stop(&mut self) {
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
fn target<F: LogFs>(live: &LoggedNamespace<F>, options: &StoreOptions) -> u64 {
    let seq = live.namespace().seq();
    match options.wal.fsync {
        FsyncPolicy::Off => seq,
        _ => live.wal().synced_seq().min(seq),
    }
}

/// Run the checkpointer to `target`; remember the outcome, and move the
/// size trigger `wal_size` bytes past `appended` (also after a failure, so
/// that a failing checkpoint isn't retried on every commit).
fn run_checkpoint<F: LogFs>(shared: &Shared<F>, target: u64, appended: u64) -> Result<CheckpointOutcome, Error> {
    let result = lock(&shared.checkpointer).run(target);
    if let Some(size) = shared.options.checkpoint.wal_size {
        shared.size_trigger.store(appended.saturating_add(size), Ordering::Relaxed);
    }
    let mut last_error = lock(&shared.checkpoint_error);
    match &result {
        Ok(outcome) => {
            *last_error = None;
            log::debug!("checkpoint at seq {} ({:?})", outcome.seq, outcome);
        }
        Err(e) => {
            let message = e.to_string();
            if last_error.as_deref() != Some(message.as_str()) {
                log::warn!("checkpoint to seq {} failed: {}", target, message);
            }
            *last_error = Some(message);
        }
    }
    result
}

/// The background checkpointer: waits for the size trigger or the
/// interval, then checkpoints the synced part of the log.
fn checkpoint_loop<F: LogFs>(shared: &Shared<F>) {
    let interval = shared.options.checkpoint.interval;
    let mut last = Instant::now();
    loop {
        {
            let mut signal = lock(&shared.signal);
            loop {
                if signal.shutdown {
                    return;
                }
                if signal.checkpoint {
                    signal.checkpoint = false;
                    break;
                }
                match interval {
                    Some(interval) if last.elapsed() >= interval => break,
                    Some(interval) => {
                        let wait = interval.saturating_sub(last.elapsed());
                        signal = shared.wake.wait_timeout(signal, wait).unwrap_or_else(PoisonError::into_inner).0;
                    }
                    None => signal = shared.wake.wait(signal).unwrap_or_else(PoisonError::into_inner),
                }
            }
        }
        last = Instant::now();
        let (target, appended) = {
            let live = lock(&shared.live);
            (target(&live, &shared.options), live.wal().appended_bytes())
        };
        // Errors are kept in `checkpoint_error` and logged
        let _ = run_checkpoint(shared, target, appended);
    }
}

/// The group commit timer: every `period`, fsync the records that have
/// waited `max_delay` (the period itself).
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
        let mut live = lock(&shared.live);
        if live.read_only().is_some() {
            continue;
        }
        if let Err(e) = or_abort("the group commit fsync", || live.sync_due()) {
            log::error!("group commit fsync failed, the store is read-only until reopened: {}", e);
        }
    }
}

/// Log what recovery found that is worth attention.
fn log_report(root: &Path, report: &RecoveryReport) {
    let dir = root.display();
    for skipped in &report.skipped_checkpoints {
        log::warn!("{}: skipped damaged checkpoint {}: {}", dir, skipped.seq, skipped.reason);
    }
    let changes = &report.index_changes;
    if !changes.created.is_empty() || !changes.dropped.is_empty() {
        log::warn!("{}: the checkpoint's indexes differed from its catalog: {:?}", dir, changes);
    }
    if let Some(tail) = &report.torn_tail {
        let level = if tail.discarded_frames > 0 { log::Level::Warn } else { log::Level::Info };
        log::log!(
            level,
            "{}: cut a torn WAL tail ({:?}) off '{}' at {} of {} bytes, discarding {} later frames",
            dir,
            tail.damage,
            tail.path.display(),
            tail.valid_len,
            tail.file_len,
            tail.discarded_frames
        );
    }
    for path in &report.removed_temp_files {
        log::info!("{}: removed stale temporary file '{}'", dir, path.display());
    }
    log::info!(
        "{}: recovered to seq {} (checkpoint {:?}, {} WAL records replayed)",
        dir,
        report.seq,
        report.checkpoint,
        report.replayed
    );
}
