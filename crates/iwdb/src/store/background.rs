//! The store's background threads (checkpoints, group commit) and the
//! abort on a panic or an inconsistent graph (ADR 0008, ADR 0028).

use std::collections::BTreeSet;
use std::panic::{self, AssertUnwindSafe};
use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ironweaver_core::GraphError;
use iwdb_engine::{CommitResult, CommitTime};
use iwdb_storage::io::LogFs;
use iwdb_storage::{CheckpointOutcome, Error, FsyncPolicy, LoggedNamespace};

use super::{NsState, Shared, Store, lock};
use crate::StoreOptions;

impl<F: LogFs + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// Stop and join the background threads.
    pub(super) fn stop(&mut self) {
        super::projections::stop_all(&self.shared);
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
pub(super) fn or_abort<R>(what: &str, f: impl FnOnce() -> R) -> R {
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

/// Abort the process if applying a commit failed with
/// `GraphError::Internal` (ADR 0028): the core's rollback failed, or it
/// found the graph inconsistent, so the graph may hold part of the
/// transaction, which readers would see. Recovery from the checkpoint and
/// the WAL (which has the record) restores a consistent state, as after a
/// panic. Other apply failures were rolled back cleanly: the namespace only
/// becomes read-only.
pub(super) fn abort_if_inconsistent(result: Result<CommitResult, Error>) -> Result<CommitResult, Error> {
    if let Err(Error::Engine(iwdb_engine::Error::ApplyFailed { seq, error: GraphError::Internal(message) })) = &result {
        log::error!("applying commit {} failed inside the core ({}), aborting the process", seq, message);
        eprintln!(
            "iwdb: applying commit {} failed inside the core ({}); the graph may be inconsistent, aborting the \
             process. The next open recovers every logged commit.",
            seq, message
        );
        std::process::abort();
    }
    result
}

pub(super) fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, Error> {
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
pub(super) fn target<F: LogFs>(live: &LoggedNamespace<F>, options: &StoreOptions) -> (u64, u64) {
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
pub(super) fn run_checkpoint<F: LogFs>(
    shared: &Shared<F>,
    state: &NsState<F>,
    target: u64,
    appended: u64,
) -> Result<CheckpointOutcome, Error> {
    let start = Instant::now();
    let result = lock(&state.checkpointer).run(target);
    if let Ok(outcome) = &result
        && outcome.written
    {
        state.checkpoints.observe(start.elapsed());
        *lock(&state.last_checkpoint) = Some(CommitTime::now());
    }
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
pub(super) fn checkpoint_loop<F: LogFs>(shared: &Shared<F>) {
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
pub(super) fn sync_loop<F: LogFs>(shared: &Shared<F>, period: Duration) {
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
