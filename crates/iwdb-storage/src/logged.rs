//! [`LoggedNamespace`]: a namespace whose commits go through its log, shared
//! by one writer and many readers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};
use std::task::Waker;
use std::time::{Duration, Instant};

use iwdb_engine::catalog::AttrPath;
use iwdb_engine::metrics::{Histogram, HistogramSnapshot};
use iwdb_engine::{
    CatalogChange, Change, CommitResult, IdempotencyKey, IndexBuild, MarkUpdate, Mutation, Namespace, Prepare,
};

use crate::io::{LogFs, StdFs};
use crate::memory::{Charge, Memory, Part};
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
/// **Locks** (ADR 0014). The WAL sits behind a mutex, which a
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
/// **Idempotency keys** (ADR 0015): a commit with a key whose
/// commit is in the namespace's key table returns the original result and
/// logs nothing, even while the namespace is read-only (that commit was
/// applied, so its result stands).
///
/// **Read-only state.** If the log fails (a write, fsync or rotation
/// error), or applying a logged record fails and poisons the namespace
/// ([`iwdb_engine::Error::ApplyFailed`]), every further commit fails with
/// [`Error::ReadOnly`] until the namespace is reopened from its checkpoint
/// and log. Reads still work, and see every applied commit.
///
/// **Memory** (ADR 0054). The namespace charges its graph, payloads included, to a
/// [`Memory`] ([`with_memory`](Self::with_memory); its own unlimited one
/// otherwise). While that refuses writes, a commit that adds anything
/// fails with [`Error::MemoryLimit`] after it is prepared and before it is
/// logged, so it never reaches the WAL; commits that only remove
/// ([`Prepared::only_removes`](iwdb_engine::Prepared::only_removes)),
/// duplicates of keyed commits and the system namespace's commits go
/// through.
#[derive(Debug)]
pub struct LoggedNamespace<F: LogFs = StdFs> {
    namespace: RwLock<Namespace>,
    wal: Mutex<Wal<F>>,
    /// The seq of the last applied commit, published after each apply.
    seq: AtomicU64,
    /// The streamable seq (ADR 0031): applied and synced (applied under
    /// `off`), published after each apply and fsync.
    streamable: AtomicU64,
    /// Why the namespace is read-only (mirrors the WAL's failure and the
    /// namespace's poison, so that asking doesn't wait for an fsync).
    failure: Mutex<Option<String>>,
    /// Waiters for a seq ([`wait_for_seq`](Self::wait_for_seq)).
    progress: Mutex<()>,
    advanced: Condvar,
    /// Futures waiting for the streamable seq ([`wake_when_streamable`](Self::wake_when_streamable)).
    wakers: Mutex<Wakers>,
    stats: Stats,
    /// The namespace was dropped: commits and waits fail.
    dropped: AtomicBool,
    /// Index builds in progress.
    builds: Mutex<Vec<Arc<BuildProgress>>>,
    /// Commits waiting for the namespace's write lock: an index build's
    /// scan lets them in before it takes the read lock again, because
    /// `RwLock` doesn't promise that a waiting writer gets in between two
    /// read locks of one thread (on macOS a scan starved commits).
    applies_waiting: AtomicUsize,
    /// The WAL's fsync histogram, readable without the writer's lock.
    fsyncs: Arc<Histogram>,
    /// The graph's nodes, edges and memory use, published with each apply,
    /// so the metrics need no lock ([`sizes`](Self::sizes)).
    nodes: AtomicUsize,
    edges: AtomicUsize,
    memory: AtomicUsize,
    /// The graph's charge ([`Part::Graph`]: structure, indexes and
    /// payloads), set with each apply.
    graph_charge: Charge,
    /// Whether the memory limit applies: every namespace but the system
    /// namespace (users, grants, session tokens), so that an operator can
    /// still log in and act.
    limited: bool,
}

/// A namespace's size as of its last apply ([`LoggedNamespace::sizes`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    pub nodes: usize,
    pub edges: usize,
    /// The core's `Graph::memory_usage`: indexes and payloads included.
    pub memory_bytes: usize,
}

/// Rows scanned per read-lock hold of an online index build.
pub const BUILD_CHUNK: usize = 2048;

/// The memory an index build is charged for each node handle it holds
/// (ADR 0054), on top of the core's figure for the build itself
/// (`IndexBuild::memory_usage`).
const BUILD_HANDLE_BYTES: usize = 8;

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
    /// Every commit's time, from the call to its acknowledgement (or
    /// failure), waiting for the writer included.
    commits: Histogram,
    write_holds: Histogram,
    read_holds: Histogram,
}

/// The namespace's duration histograms (the metrics, ADR 0050), since it
/// was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NamespaceHistograms {
    /// Commits (data and catalog), from the call to the answer.
    pub commits: HistogramSnapshot,
    /// Fsyncs of the WAL's current segment.
    pub fsyncs: HistogramSnapshot,
    /// How long commits held the write lock (apply and index flush).
    pub write_holds: HistogramSnapshot,
    /// How long [`read`](LoggedNamespace::read) held the read lock.
    pub read_holds: HistogramSnapshot,
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

/// Registered wakers: id -> (the streamable seq waited for, waker).
#[derive(Debug, Default)]
struct Wakers {
    next_id: u64,
    waiting: BTreeMap<u64, (u64, Waker)>,
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
        let fsyncs = wal.fsyncs();
        let g = namespace.graph();
        let (nodes, edges, memory) = (g.node_count(), g.edge_count(), g.memory_usage());
        let accounting = Memory::unlimited();
        let graph_charge = accounting.charge(Part::Graph);
        graph_charge.set(memory as u64);
        Ok(LoggedNamespace {
            limited: !namespace.name().is_reserved(),
            graph_charge,
            fsyncs,
            nodes: AtomicUsize::new(nodes),
            edges: AtomicUsize::new(edges),
            memory: AtomicUsize::new(memory),
            seq: AtomicU64::new(namespace.seq()),
            streamable: AtomicU64::new(streamable(&wal, namespace.seq())),
            namespace: RwLock::new(namespace),
            wal: Mutex::new(wal),
            failure: Mutex::new(failure),
            progress: Mutex::new(()),
            advanced: Condvar::new(),
            wakers: Mutex::new(Wakers::default()),
            stats: Stats::default(),
            dropped: AtomicBool::new(false),
            builds: Mutex::new(Vec::new()),
            applies_waiting: AtomicUsize::new(0),
        })
    }

    /// Charge this namespace's memory to `memory` (the store's), whose
    /// limit then refuses its writes (see the type docs).
    pub fn with_memory(mut self, memory: &Arc<Memory>) -> Self {
        let graph = memory.charge(Part::Graph);
        graph.set(self.graph_charge.bytes());
        self.graph_charge = graph;
        self
    }

    /// The memory this namespace charges to.
    pub fn memory(&self) -> &Arc<Memory> {
        self.graph_charge.memory()
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

    /// [`commit_keyed`](Self::commit_keyed) that also moves a mark,
    /// compare-and-set, in the same record (ADR 0032; see
    /// [`Namespace::prepare_marked`]): the commit applies only if the mark
    /// is at `mark.expected`, otherwise
    /// [`iwdb_engine::Error::MarkConflict`] and nothing changes. With a
    /// mark, `mutations` may be empty.
    pub fn commit_marked(
        &self,
        mutations: &[Mutation],
        key: Option<&IdempotencyKey>,
        mark: Option<&MarkUpdate>,
    ) -> Result<CommitResult, Error> {
        self.log_and_apply(|ns| ns.prepare_marked(mutations, key, mark))
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
        let (build, _charge) = match self.build_index(&change, key)? {
            Some((build, charge)) => (Some(build), Some(charge)),
            None => (None, None),
        };
        // The build's charge is released once the index is in the graph
        self.log_and_apply_built(build, |ns| ns.prepare_catalog_keyed(change, key))
    }

    /// The index builds in progress.
    pub fn builds(&self) -> Vec<Arc<BuildProgress>> {
        lock(&self.builds).clone()
    }

    /// Build the index `change` needs, if any, charged to the memory's
    /// working part (an estimate per node, ADR 0054) until the returned
    /// charge is dropped. Refused while the memory refuses writes.
    fn build_index(
        &self,
        change: &CatalogChange,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<(IndexBuild, Charge)>, Error> {
        let mut build = {
            let mut ns = self.namespace.write().unwrap_or_else(PoisonError::into_inner);
            if key.is_some_and(|k| ns.keys().get(k).is_some()) {
                return Ok(None);
            }
            let Some(path) = ns.index_needed(change) else { return Ok(None) };
            if self.limited {
                self.memory().check_write()?;
            }
            // O(1). A path the core refuses is left to the commit, whose
            // validation reports it
            match ns.begin_index_build(path) {
                Ok(build) => build,
                Err(_) => return Ok(None),
            }
        };
        // Nodes added from here on are in the list and tracked as changed;
        // the install re-reads them either way
        let handles = self.namespace().node_handles();
        let progress =
            Arc::new(BuildProgress { path: build.path().clone(), total: handles.len(), scanned: AtomicU64::new(0) });
        let charge = self.memory().charge(Part::Working);
        let charged = |build: &IndexBuild| (BUILD_HANDLE_BYTES * handles.len() + build.memory_usage()) as u64;
        charge.set(charged(&build));
        lock(&self.builds).push(progress.clone());
        let mut result = Ok(());
        for chunk in handles.chunks(BUILD_CHUNK) {
            while self.applies_waiting.load(Ordering::Acquire) > 0 {
                std::thread::yield_now();
            }
            if self.is_dropped() {
                result = Err(Error::NamespaceDropped { name: self.namespace().name().to_string() });
                break;
            }
            result = self.namespace().scan_index_keys(chunk, &mut build).map_err(Error::from);
            if result.is_err() {
                break;
            }
            progress.scanned.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            charge.set(charged(&build));
        }
        lock(&self.builds).retain(|b| !Arc::ptr_eq(b, &progress));
        result.map(|()| Some((build, charge)))
    }

    /// Mark the namespace dropped: further commits fail with
    /// [`Error::NamespaceDropped`], and waiters wake with [`Wait::Dropped`].
    /// Reads in progress finish on the state they started with.
    pub fn mark_dropped(&self) {
        let guard = lock(&self.progress);
        self.dropped.store(true, Ordering::Release);
        self.advanced.notify_all();
        drop(guard);
        self.wake(u64::MAX);
    }

    pub fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
    }

    /// Fsync every logged commit (see [`Wal::sync`]).
    pub fn sync(&self) -> Result<(), Error> {
        let mut wal = lock(&self.wal);
        let result = wal.sync();
        self.note_failure(&wal, &result);
        self.publish(&wal, self.seq());
        result
    }

    /// Fsync group-committed records that are due (see [`Wal::sync_due`]).
    pub fn sync_due(&self) -> Result<bool, Error> {
        let mut wal = lock(&self.wal);
        let result = wal.sync_due();
        self.note_failure(&wal, &result);
        if matches!(result, Ok(true)) {
            self.publish(&wal, self.seq());
        }
        result
    }

    /// Run `f` on the namespace under the read lock: it sees the state
    /// after some commit, never part of one. Commits wait to apply while it
    /// runs, so keep it short (ADR 0014).
    pub fn read<R>(&self, f: impl FnOnce(&Namespace) -> R) -> R {
        let namespace = self.namespace();
        let start = Instant::now();
        let result = f(&namespace);
        drop(namespace);
        self.stats.read_holds.observe(start.elapsed());
        result
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

    /// The streamable seq (ADR 0031): every commit up to it is applied and
    /// synced, so no crash can lose it or give its seq to another commit.
    /// Under [`FsyncPolicy::Off`](crate::FsyncPolicy::Off), which promises
    /// nothing, the applied seq. Doesn't wait for any lock.
    pub fn streamable_seq(&self) -> u64 {
        self.streamable.load(Ordering::Acquire)
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
        self.wait_until(seq, &self.seq, true, deadline, cancelled)
    }

    /// Wait until the streamable seq ([`streamable_seq`](Self::streamable_seq))
    /// reaches `seq`, like [`wait_for_seq`](Self::wait_for_seq). A
    /// read-only namespace waits until the deadline: nothing more becomes
    /// streamable, but that isn't an error for a reader of changes.
    pub fn wait_for_streamable(&self, seq: u64, deadline: Option<Instant>, cancelled: &dyn Fn() -> bool) -> Wait {
        self.wait_until(seq, &self.streamable, false, deadline, cancelled)
    }

    fn wait_until(
        &self,
        seq: u64,
        current: &AtomicU64,
        read_only_ends: bool,
        deadline: Option<Instant>,
        cancelled: &dyn Fn() -> bool,
    ) -> Wait {
        let mut guard = lock(&self.progress);
        loop {
            if self.is_dropped() {
                return Wait::Dropped;
            }
            let now = current.load(Ordering::Acquire);
            if now >= seq {
                return Wait::Reached(now);
            }
            if read_only_ends && let Some(cause) = self.read_only() {
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

    /// Wake `waker` once the streamable seq reaches `seq` or the namespace
    /// is dropped, once: for futures that wait without a thread (the change
    /// stream's long poll). `id` (from an earlier call) replaces that
    /// registration. Returns the registration's id; [`forget_waker`](Self::forget_waker)
    /// removes it.
    ///
    /// Check the streamable seq (and [`is_dropped`](Self::is_dropped))
    /// **after** registering: a change published before the registration
    /// wakes nobody.
    pub fn wake_when_streamable(&self, id: Option<u64>, seq: u64, waker: &Waker) -> u64 {
        let mut wakers = lock(&self.wakers);
        let id = id.unwrap_or_else(|| {
            wakers.next_id += 1;
            wakers.next_id
        });
        wakers.waiting.insert(id, (seq, waker.clone()));
        id
    }

    /// Remove a registration of [`wake_when_streamable`](Self::wake_when_streamable)
    /// (gone already if it was woken).
    pub fn forget_waker(&self, id: u64) {
        lock(&self.wakers).waiting.remove(&id);
    }

    /// Wake (and remove) the wakers waiting for a seq up to `streamable`.
    fn wake(&self, streamable: u64) {
        let woken: Vec<Waker> = {
            let mut wakers = lock(&self.wakers);
            let ids: Vec<u64> =
                wakers.waiting.iter().filter(|(_, (seq, _))| *seq <= streamable).map(|(id, _)| *id).collect();
            ids.iter().filter_map(|id| wakers.waiting.remove(id)).map(|(_, waker)| waker).collect()
        };
        woken.into_iter().for_each(Waker::wake);
    }

    /// How long commits held the write lock so far.
    pub fn lock_stats(&self) -> LockStats {
        LockStats {
            writes: self.stats.writes.load(Ordering::Relaxed),
            write_hold_total: Duration::from_nanos(self.stats.total_ns.load(Ordering::Relaxed)),
            write_hold_max: Duration::from_nanos(self.stats.max_ns.load(Ordering::Relaxed)),
        }
    }

    /// The graph's size as of the last apply (O(1), no lock): the core's
    /// counts and memory estimate, read under the write lock then.
    pub fn sizes(&self) -> Sizes {
        Sizes {
            nodes: self.nodes.load(Ordering::Relaxed),
            edges: self.edges.load(Ordering::Relaxed),
            memory_bytes: self.memory.load(Ordering::Relaxed),
        }
    }

    /// The namespace's duration histograms so far. Doesn't wait for any
    /// lock.
    pub fn histograms(&self) -> NamespaceHistograms {
        NamespaceHistograms {
            commits: self.stats.commits.snapshot(),
            fsyncs: self.fsyncs.snapshot(),
            write_holds: self.stats.write_holds.snapshot(),
            read_holds: self.stats.read_holds.snapshot(),
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
        let span =
            crate::trace_span!("iwdb.commit", iwdb.seq = tracing::field::Empty, iwdb.ops = tracing::field::Empty);
        let _entered = span.enter();
        let start = Instant::now();
        let result = self.commit_now(&span, build, prepare);
        self.stats.commits.observe(start.elapsed());
        if let Ok(r) = &result {
            span.record("iwdb.seq", r.seq);
        }
        result
    }

    fn commit_now(
        &self,
        span: &tracing::Span,
        build: Option<IndexBuild>,
        prepare: impl FnOnce(&Namespace) -> Result<Prepare, iwdb_engine::Error>,
    ) -> Result<CommitResult, Error> {
        let mut wal = lock(&self.wal);
        if self.is_dropped() {
            return Err(Error::NamespaceDropped { name: self.namespace().name().to_string() });
        }
        // The lookup of a key comes first: a duplicate's commit was applied,
        // so its result stands even when the namespace is read-only now
        let prepared = crate::trace_span!("iwdb.prepare").in_scope(|| prepare(&self.namespace()));
        let prepared = match (prepared, self.read_only()) {
            (Ok(Prepare::Duplicate(result)), _) => return Ok(result),
            (_, Some(cause)) => return Err(Error::ReadOnly { cause }),
            (Ok(Prepare::New(prepared)), None) => prepared,
            (Err(e), None) => return Err(e.into()),
        };
        // The memory limit (ADR 0054): refused before the log, so a refused
        // commit is never in the WAL
        if self.limited && !prepared.only_removes() {
            self.memory().check_write()?;
        }
        if let Change::Data(ops) = &prepared.record().change {
            span.record("iwdb.ops", ops.len());
        }
        let appended = wal.append(prepared.record());
        self.note_failure(&wal, &appended);
        let time = appended?;

        let apply = crate::trace_span!("iwdb.apply");
        let applying = apply.enter();
        let start = Instant::now();
        self.applies_waiting.fetch_add(1, Ordering::AcqRel);
        let mut namespace = self.namespace.write().unwrap_or_else(PoisonError::into_inner);
        self.applies_waiting.fetch_sub(1, Ordering::AcqRel);
        // An error here is ApplyFailed (the namespace is now poisoned, so
        // read-only) or a bug; either way the commit is not acknowledged.
        let applied = namespace.apply_built(prepared, Some(time), build);
        let seq = namespace.seq();
        let g = namespace.graph();
        self.nodes.store(g.node_count(), Ordering::Relaxed);
        self.edges.store(g.edge_count(), Ordering::Relaxed);
        self.memory.store(g.memory_usage(), Ordering::Relaxed);
        self.graph_charge.set(g.memory_usage() as u64);
        drop(namespace);
        self.stats.record(start.elapsed());
        drop(applying);
        match applied {
            Ok(result) => {
                self.publish(&wal, seq);
                Ok(result)
            }
            Err(e) => {
                *lock(&self.failure) = Some(iwdb_engine::Error::Poisoned.to_string());
                self.advanced.notify_all();
                Err(e.into())
            }
        }
    }

    /// Make `seq` the applied seq, update the streamable seq from `wal`,
    /// and wake their waiters. Called under the writer's lock.
    fn publish(&self, wal: &Wal<F>, seq: u64) {
        let guard = lock(&self.progress);
        let streamable = streamable(wal, seq);
        self.seq.store(seq, Ordering::Release);
        self.streamable.store(streamable, Ordering::Release);
        self.advanced.notify_all();
        drop(guard);
        self.wake(streamable);
    }

    /// After a WAL operation: if the log failed, the namespace is read-only.
    fn note_failure<T>(&self, wal: &Wal<F>, result: &Result<T, Error>) {
        if result.is_err()
            && let Some(cause) = wal.failure()
        {
            lock(&self.failure).get_or_insert_with(|| cause.to_owned());
            self.advanced.notify_all();
        }
    }
}

/// The streamable seq of a namespace at `applied` logging to `wal`.
fn streamable<F: LogFs>(wal: &Wal<F>, applied: u64) -> u64 {
    match wal.options().fsync {
        crate::FsyncPolicy::Off => applied,
        _ => applied.min(wal.synced_seq()),
    }
}

impl Stats {
    fn record(&self, held: Duration) {
        let ns = u64::try_from(held.as_nanos()).unwrap_or(u64::MAX);
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        self.write_holds.observe(held);
    }
}
