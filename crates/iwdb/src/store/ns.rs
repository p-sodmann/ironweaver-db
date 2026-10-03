//! [`Ns`]: a handle on one namespace of a [`Store`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ironweaver_core::cancel::{self, Token};
use ironweaver_core::{EdgeId, GraphError, Projection};
use iwdb_engine::catalog::{AttrPath, NamespaceCatalog};
use iwdb_engine::{CatalogChange, CommitResult, Mutation, Namespace};
use iwdb_query::{CommitOptions, Edge, IndexSize, IndexState, IndexStatus, NamespaceStatus, Node, ProjectionSpec};
use iwdb_storage::io::LogFs;
use iwdb_storage::{CheckpointOutcome, Error, FsyncPolicy, LockStats, LoggedNamespace, Wait};

use super::background::{abort_if_inconsistent, or_abort, run_checkpoint, target};
use super::{Analysis, NsState, Store, lock};
use crate::request::{Deadline, ReadOptions, Scheduled};

/// A handle on one namespace of a [`Store`]: its commits, reads, catalog
/// and checkpoints. Cheap to make; holds the store borrowed, so it can't
/// outlive it. If the namespace is dropped, its methods fail with
/// [`Error::NamespaceDropped`] (commits and waits) or finish on the state
/// they had (reads).
pub struct Ns<'a, F: LogFs + Send + Sync + 'static>
where
    F::File: Send,
{
    pub(super) store: &'a Store<F>,
    pub(super) state: Arc<NsState<F>>,
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
        self.read(|ns| Node::read(ns.graph(), ns.graph().node_ix(id)?))
    }

    /// The edge `id`, if it exists.
    pub fn edge(&self, id: EdgeId) -> Option<Edge> {
        self.read(|ns| Edge::read(ns.graph(), ns.graph().edge_ix(id)?))
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
    /// applied (read-your-writes), at most until the deadline; then run `f`
    /// under a cancel token ([`cancel::run`]) that the store's timer
    /// cancels at the deadline (and the caller through `options.cancel`).
    /// The core's algorithms check it and stop; `f`'s result is then
    /// dropped and the read fails. So the deadline bounds the whole read,
    /// as long as `f` spends its time in the core. If no time is left after
    /// the wait (a zero timeout, say), the read fails without running `f`,
    /// even if `min_seq` was applied already. Errors:
    /// [`Error::OtherHistory`], [`Error::Timeout`], [`Error::Cancelled`],
    /// [`Error::ReadOnly`], [`Error::NamespaceDropped`].
    pub fn read_with<R>(&self, options: &ReadOptions, f: impl FnOnce(&Namespace) -> R) -> Result<R, Error> {
        let deadline = options.deadline();
        let (token, _scheduled) = self.start(options, &deadline, "the read")?;
        cancel::run(&token, || self.read(f)).map_err(|_| stopped(&deadline, options.cancel.as_ref(), "the read"))
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
        let (token, _scheduled) = self.start(options, &deadline, "the analytics job")?;
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
                Err(stopped(&deadline, options.cancel.as_ref(), "the analytics job"))
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

    /// The paths of the index builds in progress (ADR 0019): indexes that
    /// aren't in the catalog yet, so reads don't use them. Takes only the
    /// build list's own mutex, never the namespace's lock.
    pub fn index_builds(&self) -> Vec<AttrPath> {
        self.live().builds().iter().map(|b| b.path.clone()).collect()
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
        let result = or_abort("a commit", || abort_if_inconsistent(commit(live)))?;
        let shared = &self.store.shared;
        let trigger = self.state.size_trigger.load(Ordering::Relaxed);
        if !result.deduplicated && live.wal().appended_bytes() >= trigger {
            lock(&shared.signal).checkpoint.insert(self.state.info.id);
            shared.wake.notify_all();
        }
        Ok(result)
    }

    /// [`wait`](Self::wait), then the cancel token for `what`, which the
    /// store's timer cancels at the deadline (dropping the guard
    /// unschedules it). A deadline that has passed already fails here with
    /// [`Error::Timeout`]: otherwise the outcome would depend on whether
    /// the timer thread cancels the token before the work ends.
    fn start(
        &self,
        options: &ReadOptions,
        deadline: &Deadline,
        what: &str,
    ) -> Result<(Token, Option<Scheduled<'_>>), Error> {
        self.wait(options, deadline)?;
        if deadline.passed() {
            return Err(deadline.timeout(what));
        }
        let token = options.cancel.clone().unwrap_or_default();
        let scheduled = match deadline.at {
            Some(at) => Some(self.store.timer.schedule(at, token.clone())?),
            None => None,
        };
        Ok((token, scheduled))
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
fn stopped(deadline: &Deadline, cancel: Option<&Token>, what: &str) -> Error {
    if deadline.passed() || !cancel.is_some_and(Token::is_cancelled) {
        deadline.timeout(what)
    } else {
        Error::Cancelled
    }
}
