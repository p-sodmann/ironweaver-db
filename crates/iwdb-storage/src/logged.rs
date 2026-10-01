//! [`LoggedNamespace`]: a namespace whose commits go through its log, shared
//! by one writer and many readers.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use iwdb_engine::catalog::AttrPath;
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, IndexBuild, Mutation, Namespace, Prepare};

use crate::io::{LogFs, StdFs};
use crate::{Error, Wal};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A [`Namespace`] and the [`Wal`] its commits are logged to: the commit
/// order of the durable write path, safe to share between threads.
///
/// A commit is resolved and validated ([`Namespace::prepare_keyed`]),
/// appended to the log and fsynced per the log's policy ([`Wal::append`]),
/// applied in memory ([`Namespace::apply`]), and only then acknowledged
/// (returned). So an acknowledged commit is in the log, durable with
/// [`FsyncPolicy::Always`](crate::FsyncPolicy::Always), and a commit whose
/// append fails is never applied.
///
/// **Locks** (step 8, ADR 0014). The WAL sits behind a mutex, which a
/// commit holds from start to end: commits are serialized (the single
/// writer). The namespace sits behind a reader/writer lock. A commit
/// prepares under its **read** side (readers go on meanwhile), appends and
/// fsyncs holding no namespace lock, and takes the **write** side only to
/// apply the record and flush the indexes. Readers ([`read`](Self::read),
/// [`namespace`](Self::namespace)) take the read side, so they never see
/// part of a transaction, and they wait only for an apply. The lock order
/// is WAL, then namespace; nothing takes the WAL's lock while holding the
/// namespace's.
///
/// **Idempotency keys** (step 8, ADR 0015): a commit with a key whose
/// commit is in the namespace's key table returns the original result and
/// logs nothing, even while the namespace is read-only (that commit was
/// applied, so its result stands).
///
/// **Read-only state.** If the log fails (a write, fsync or rotation
/// error), or applying a logged record fails and poisons the namespace
/// ([`iwdb_engine::Error::ApplyFailed`]), every further commit fails with
/// [`Error::ReadOnly`] until the namespace is reopened from its checkpoint
/// and log (step 5). Reads still work, and see every applied commit.
///
/// One namespace only (step 9 decides how namespaces share logs).
#[derive(Debug)]
pub struct LoggedNamespace<F: LogFs = StdFs> {
    namespace: RwLock<Namespace>,
    wal: Mutex<Wal<F>>,
    /// The seq of the last applied commit, published after each apply.
    seq: AtomicU64,
    /// Why the namespace is read-only (mirrors the WAL's failure and the
    /// namespace's poison, so that asking doesn't wait for an fsync).
    failure: Mutex<Option<String>>,
    /// Waiters for a seq ([`wait_for_seq`](Self::wait_for_seq)).
    progress: Mutex<()>,
    advanced: Condvar,
    stats: Stats,
    /// The namespace was dropped (step 9): commits and waits fail.
    dropped: AtomicBool,
    /// Index builds in progress (step 9).
    builds: Mutex<Vec<Arc<BuildProgress>>>,
}

/// Rows scanned per read-lock hold of an online index build.
pub const BUILD_CHUNK: usize = 8192;

/// An online index build in progress ([`LoggedNamespace::builds`]).
#[derive(Debug)]
pub struct BuildProgress {
    pub path: AttrPath,
    pub total: usize,
    scanned: AtomicU64,
}

impl BuildProgress {
    /// Nodes scanned so far.
    pub fn scanned(&self) -> usize {
        self.scanned.load(Ordering::Relaxed) as usize
    }
}

/// How long commits held the namespace's write lock (apply and index
/// flush), since the namespace was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LockStats {
    /// Commits that took the write lock.
    pub writes: u64,
    pub write_hold_total: Duration,
    pub write_hold_max: Duration,
}

#[derive(Debug, Default)]
struct Stats {
    writes: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
}

/// How a [`wait_for_seq`](LoggedNamespace::wait_for_seq) ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The seq is applied; the namespace's seq now.
    Reached(u64),
    /// The deadline passed first; the namespace's seq then.
    TimedOut(u64),
    /// The namespace was dropped.
    Dropped,
    /// `cancelled` said so.
    Cancelled,
    /// The namespace is read-only and below the seq: it will never get there.
    ReadOnly(String),
}

/// How often a waiter checks for cancellation.
const CANCEL_CHECK: Duration = Duration::from_millis(10);

impl<F: LogFs> LoggedNamespace<F> {
    /// Pair a namespace with the log its next commit goes to: the log's
    /// next seq must follow the namespace's.
    pub fn new(namespace: Namespace, wal: Wal<F>) -> Result<Self, Error> {
        let expected = namespace.seq().checked_add(1).ok_or(iwdb_engine::Error::SeqExhausted)?;
        if wal.next_seq() != expected {
            return Err(Error::OutOfOrder { expected, found: wal.next_seq() });
        }
        let poisoned = namespace.is_poisoned().then(|| iwdb_engine::Error::Poisoned.to_string());
        let failure = wal.failure().map(str::to_owned).or(poisoned);
        Ok(LoggedNamespace {
            seq: AtomicU64::new(namespace.seq()),
            namespace: RwLock::new(namespace),
            wal: Mutex::new(wal),
            failure: Mutex::new(failure),
            progress: Mutex::new(()),
            advanced: Condvar::new(),
            stats: Stats::default(),
            dropped: AtomicBool::new(false),
            builds: Mutex::new(Vec::new()),
        })
    }

    /// Commit a data transaction: all mutations or none. Returns once the
    /// commit is logged (per the fsync policy) and applied.
    ///
    /// Errors: an [`Error::Engine`] from validation (nothing changes, the
    /// namespace stays writable); [`Error::RecordTooLarge`] (likewise);
    /// [`Error::Io`] (not applied, outcome unknown, now read-only);
    /// [`Error::ReadOnly`].
    pub fn commit(&self, mutations: &[Mutation]) -> Result<CommitResult, Error> {
        self.commit_keyed(mutations, None)
    }

    /// [`commit`](Self::commit) with an idempotency key: if the key's
    /// commit is in the key table, its original result (`deduplicated`
    /// set) and nothing is logged; another request under that key fails
    /// with [`iwdb_engine::Error::IdempotencyKeyReused`].
    pub fn commit_keyed(&self, mutations: &[Mutation], key: Option<&IdempotencyKey>) -> Result<CommitResult, Error> {
        self.log_and_apply(|ns| ns.prepare_keyed(mutations, key))
    }

    /// Commit a catalog change, like [`commit`](Self::commit).
    pub fn commit_catalog(&self, change: CatalogChange) -> Result<CommitResult, Error> {
        self.commit_catalog_keyed(change, None)
    }

    /// Commit a catalog change with an idempotency key, like
    /// [`commit_keyed`](Self::commit_keyed).
    pub fn commit_catalog_keyed(
        &self,
        change: CatalogChange,
        key: Option<&IdempotencyKey>,
    ) -> Result<CommitResult, Error> {
        // An index the graph lacks is built first, reading the nodes a
        // chunk at a time under the read lock, so that neither commits nor
        // reads wait for the whole build (ADR 0019)
        let build = self.build_index(&change, key)?;
        self.log_and_apply_built(build, |ns| ns.prepare_catalog_keyed(change, key))
    }

    /// The index builds in progress.
    pub fn builds(&self) -> Vec<Arc<BuildProgress>> {
        lock(&self.builds).clone()
    }

    fn build_index(&self, change: &CatalogChange, key: Option<&IdempotencyKey>) -> Result<Option<IndexBuild>, Error> {
        let (path, handles) = {
            let ns = self.namespace();
            if key.is_some_and(|k| ns.keys().get(k).is_some()) {
                return Ok(None);
            }
            match ns.index_needed(change) {
                Some(path) => (path, ns.node_handles()),
                None => return Ok(None),
            }
        };
        let progress = Arc::new(BuildProgress { path: path.clone(), total: handles.len(), scanned: AtomicU64::new(0) });
        lock(&self.builds).push(progress.clone());
        let mut build = IndexBuild::new(path);
        let mut result = Ok(());
        for chunk in handles.chunks(BUILD_CHUNK) {
            if self.is_dropped() {
                result = Err(Error::NamespaceDropped { name: self.namespace().name().to_string() });
                break;
            }
            result = self.namespace().scan_index_keys(chunk, &mut build).map_err(Error::from);
            if result.is_err() {
                break;
            }
            progress.scanned.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        lock(&self.builds).retain(|b| !Arc::ptr_eq(b, &progress));
        result.map(|()| Some(build))
    }

    /// Mark the namespace dropped: further commits fail with
    /// [`Error::NamespaceDropped`], and waiters wake with [`Wait::Dropped`].
    /// Reads in progress finish on the state they started with.
    pub fn mark_dropped(&self) {
        let _guard = lock(&self.progress);
        self.dropped.store(true, Ordering::Release);
        self.advanced.notify_all();
    }

    pub fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
    }

    /// Fsync every logged commit (see [`Wal::sync`]).
    pub fn sync(&self) -> Result<(), Error> {
        let mut wal = lock(&self.wal);
        let result = wal.sync();
        self.note_failure(&wal, &result);
        result
    }

    /// Fsync group-committed records that are due (see [`Wal::sync_due`]).
    pub fn sync_due(&self) -> Result<bool, Error> {
        let mut wal = lock(&self.wal);
        let result = wal.sync_due();
        self.note_failure(&wal, &result);
        result
    }

    /// Run `f` on the namespace under the read lock: it sees the state
    /// after some commit, never part of one. Commits wait to apply while it
    /// runs, so keep it short (ADR 0014).
    pub fn read<R>(&self, f: impl FnOnce(&Namespace) -> R) -> R {
        f(&self.namespace())
    }

    /// The namespace, under the read lock until the guard is dropped. Don't
    /// commit on this thread while holding it: the commit would wait for it.
    pub fn namespace(&self) -> RwLockReadGuard<'_, Namespace> {
        // Only a panic while writing poisons the lock, and that aborts
        // (ADR 0008) wherever the store runs it; tests may see one
        self.namespace.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The log, under the writer's lock until the guard is dropped (a
    /// commit in progress finishes first). Don't take it while holding
    /// [`namespace`](Self::namespace).
    pub fn wal(&self) -> MutexGuard<'_, Wal<F>> {
        lock(&self.wal)
    }

    /// The seq of the last applied commit. Doesn't wait for any lock.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Why the namespace is read-only, if it is. Doesn't wait for a commit.
    pub fn read_only(&self) -> Option<String> {
        lock(&self.failure).clone()
    }

    /// Wait until commit `seq` is applied (read-your-writes), until
    /// `deadline` (`None`: no limit), checking `cancelled` every few
    /// milliseconds. Returns at once if it is applied already, and if the
    /// namespace is read-only below `seq` (it can't get there).
    pub fn wait_for_seq(&self, seq: u64, deadline: Option<Instant>, cancelled: &dyn Fn() -> bool) -> Wait {
        let mut guard = lock(&self.progress);
        loop {
            if self.is_dropped() {
                return Wait::Dropped;
            }
            let now = self.seq();
            if now >= seq {
                return Wait::Reached(now);
            }
            if let Some(cause) = self.read_only() {
                return Wait::ReadOnly(cause);
            }
            if cancelled() {
                return Wait::Cancelled;
            }
            let left = match deadline {
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return Wait::TimedOut(now),
                },
                None => CANCEL_CHECK,
            };
            guard = self.advanced.wait_timeout(guard, left.min(CANCEL_CHECK)).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// How long commits held the write lock so far.
    pub fn lock_stats(&self) -> LockStats {
        LockStats {
            writes: self.stats.writes.load(Ordering::Relaxed),
            write_hold_total: Duration::from_nanos(self.stats.total_ns.load(Ordering::Relaxed)),
            write_hold_max: Duration::from_nanos(self.stats.max_ns.load(Ordering::Relaxed)),
        }
    }

    /// Sync (per the policy) and close the log, returning the namespace.
    pub fn close(self) -> Result<Namespace, Error> {
        let (namespace, wal) = self.into_parts();
        wal.close()?;
        Ok(namespace)
    }

    /// Split into the namespace and its log.
    pub fn into_parts(self) -> (Namespace, Wal<F>) {
        let namespace = self.namespace.into_inner().unwrap_or_else(PoisonError::into_inner);
        let wal = self.wal.into_inner().unwrap_or_else(PoisonError::into_inner);
        (namespace, wal)
    }

    /// The commit pipeline: prepare (read lock), append (no namespace
    /// lock), apply (write lock), all under the writer's lock.
    fn log_and_apply(
        &self,
        prepare: impl FnOnce(&Namespace) -> Result<Prepare, iwdb_engine::Error>,
    ) -> Result<CommitResult, Error> {
        self.log_and_apply_built(None, prepare)
    }

    fn log_and_apply_built(
        &self,
        build: Option<IndexBuild>,
        prepare: impl FnOnce(&Namespace) -> Result<Prepare, iwdb_engine::Error>,
    ) -> Result<CommitResult, Error> {
        let mut wal = lock(&self.wal);
        if self.is_dropped() {
            return Err(Error::NamespaceDropped { name: self.namespace().name().to_string() });
        }
        // The lookup of a key comes first: a duplicate's commit was applied,
        // so its result stands even when the namespace is read-only now
        let prepared = match (prepare(&self.namespace()), self.read_only()) {
            (Ok(Prepare::Duplicate(result)), _) => return Ok(result),
            (_, Some(cause)) => return Err(Error::ReadOnly { cause }),
            (Ok(Prepare::New(prepared)), None) => prepared,
            (Err(e), None) => return Err(e.into()),
        };
        let appended = wal.append(prepared.record());
        self.note_failure(&wal, &appended);
        let time = appended?;

        let start = Instant::now();
        let mut namespace = self.namespace.write().unwrap_or_else(PoisonError::into_inner);
        // An error here is ApplyFailed (the namespace is now poisoned, so
        // read-only) or a bug; either way the commit is not acknowledged.
        let applied = namespace.apply_built(prepared, Some(time), build);
        let seq = namespace.seq();
        drop(namespace);
        self.stats.record(start.elapsed());
        match applied {
            Ok(result) => {
                self.publish(seq);
                Ok(result)
            }
            Err(e) => {
                *lock(&self.failure) = Some(iwdb_engine::Error::Poisoned.to_string());
                self.advanced.notify_all();
                Err(e.into())
            }
        }
    }

    /// Make `seq` the applied seq and wake its waiters.
    fn publish(&self, seq: u64) {
        let _guard = lock(&self.progress);
        self.seq.store(seq, Ordering::Release);
        self.advanced.notify_all();
    }

    /// After a WAL operation: if the log failed, the namespace is read-only.
    fn note_failure<T>(&self, wal: &Wal<F>, result: &Result<T, Error>) {
        if result.is_err() {
            if let Some(cause) = wal.failure() {
                lock(&self.failure).get_or_insert_with(|| cause.to_owned());
                self.advanced.notify_all();
            }
        }
    }
}

impl Stats {
    fn record(&self, held: Duration) {
        let ns = u64::try_from(held.as_nanos()).unwrap_or(u64::MAX);
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }
}
