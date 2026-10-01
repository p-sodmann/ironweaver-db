//! Reads with deadlines, cancellation and read-your-writes (step 8): what
//! the `Database` trait of step 10 builds on.
//!
//! A request runs on the caller's thread (in a server, a blocking thread).
//! Its deadline is enforced by the store's timer thread, which cancels the
//! request's [`cancel::Token`] when the deadline passes; the core's
//! algorithms check the token and stop ([`cancel::run`]). Waiting for a
//! `min_seq` ends at the deadline too.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ironweaver_core::cancel::Token;
use iwdb_storage::{Error, HistoryId};

/// How long a request may take when [`ReadOptions::timeout`] is `None`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Options of a read ([`Store::read_with`](crate::Store::read_with),
/// [`Store::analyze`](crate::Store::analyze), ...).
#[derive(Clone, Debug, Default)]
pub struct ReadOptions {
    /// Read-your-writes: wait until the store has applied this seq (for
    /// example the seq a commit returned), then read. The read sees that
    /// commit and every one before it.
    pub min_seq: Option<u64>,
    /// The history `min_seq` belongs to ([`Store::history`](crate::Store::history)).
    /// If given and not the store's, the read fails with
    /// [`Error::OtherHistory`]: a seq of another history (a store restored
    /// since, or another store) says nothing about this one. Without it,
    /// the caller vouches that `min_seq` is of this store's history.
    pub history: Option<HistoryId>,
    /// How long the request may take, the `min_seq` wait included
    /// (`None`: [`DEFAULT_TIMEOUT`]; `Duration::MAX`: no limit). After it,
    /// the request fails with [`Error::Timeout`].
    pub timeout: Option<Duration>,
    /// Cancels the request when the caller cancels it (it fails with
    /// [`Error::Cancelled`]). The store also cancels it at the deadline.
    pub cancel: Option<Token>,
}

impl ReadOptions {
    /// Options that wait for `seq` (read-your-writes).
    pub fn min_seq(seq: u64) -> Self {
        ReadOptions { min_seq: Some(seq), ..ReadOptions::default() }
    }

    pub(crate) fn deadline(&self) -> Deadline {
        let timeout = self.timeout.unwrap_or(DEFAULT_TIMEOUT);
        Deadline { at: Instant::now().checked_add(timeout), timeout }
    }
}

/// When a request must be done (`at: None`: never).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    pub at: Option<Instant>,
    pub timeout: Duration,
}

impl Deadline {
    pub fn passed(&self) -> bool {
        self.at.is_some_and(|at| Instant::now() >= at)
    }

    pub fn timeout(&self, what: &str) -> Error {
        Error::Timeout { what: what.to_owned(), after: self.timeout }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct TimerState {
    deadlines: BTreeMap<(Instant, u64), Token>,
    next_id: u64,
    shutdown: bool,
}

#[derive(Default)]
struct TimerShared {
    state: Mutex<TimerState>,
    wake: Condvar,
}

/// Cancels tokens at their deadlines, in one background thread started on
/// first use. Each scheduled deadline is removed when its guard drops, so
/// finished requests leave nothing behind.
#[derive(Default)]
pub(crate) struct Timer {
    shared: Arc<TimerShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// A scheduled deadline; dropping it unschedules it.
pub(crate) struct Scheduled<'a> {
    timer: &'a Timer,
    entry: (Instant, u64),
}

impl Drop for Scheduled<'_> {
    fn drop(&mut self) {
        lock(&self.timer.shared.state).deadlines.remove(&self.entry);
    }
}

impl Timer {
    /// Cancel `token` at `at`, unless the guard is dropped first.
    pub fn schedule(&self, at: Instant, token: Token) -> Result<Scheduled<'_>, Error> {
        self.start()?;
        let mut state = lock(&self.shared.state);
        let entry = (at, state.next_id);
        state.next_id += 1;
        state.deadlines.insert(entry, token);
        drop(state);
        self.shared.wake.notify_all();
        Ok(Scheduled { timer: self, entry })
    }

    fn start(&self) -> Result<(), Error> {
        let mut thread = lock(&self.thread);
        if thread.is_some() {
            return Ok(());
        }
        let shared = self.shared.clone();
        let handle = std::thread::Builder::new()
            .name("iwdb-timer".into())
            .spawn(move || run_timer(&shared))
            .map_err(|e| Error::Io { op: "spawn thread", path: "iwdb-timer".into(), source: e })?;
        *thread = Some(handle);
        Ok(())
    }

    /// Stop the thread (pending deadlines are dropped).
    pub fn stop(&self) {
        lock(&self.shared.state).shutdown = true;
        self.shared.wake.notify_all();
        if let Some(thread) = lock(&self.thread).take() {
            // A panicked timer has nothing to clean up
            let _ = thread.join();
        }
    }
}

fn run_timer(shared: &TimerShared) {
    let mut state = lock(&shared.state);
    loop {
        if state.shutdown {
            return;
        }
        let now = Instant::now();
        while let Some(entry) = state.deadlines.first_entry() {
            if entry.key().0 > now {
                break;
            }
            entry.remove().cancel();
        }
        state = match state.deadlines.keys().next().map(|(at, _)| *at) {
            Some(at) => {
                let wait = at.saturating_duration_since(Instant::now());
                shared.wake.wait_timeout(state, wait).unwrap_or_else(PoisonError::into_inner).0
            }
            None => shared.wake.wait(state).unwrap_or_else(PoisonError::into_inner),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timer_cancels_at_the_deadline_and_not_after_the_guard_drops() {
        let timer = Timer::default();
        let early = Token::new();
        let dropped = Token::new();
        let start = Instant::now();
        let _a = timer.schedule(start + Duration::from_millis(20), early.clone()).expect("schedule");
        drop(timer.schedule(start + Duration::from_millis(10), dropped.clone()).expect("schedule"));
        while !early.is_cancelled() {
            assert!(start.elapsed() < Duration::from_secs(5), "never cancelled");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(start.elapsed() >= Duration::from_millis(20));
        assert!(!dropped.is_cancelled());
        timer.stop();
    }
}
