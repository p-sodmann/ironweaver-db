//! The request registry (step 16c, ADR 0052): every call that passes the
//! authorisation point ([`Authorized`](crate::Authorized)) is registered
//! while it runs, so it can be listed ([`Requests::list`]) and cancelled
//! ([`Requests::cancel`]), and counted when it ends (the request metrics).
//! It also remembers the change-stream readers ([`Requests::consumers`]).
//!
//! **Ids** are a per-process counter from 1: unique while the process
//! runs, not across restarts. Guessing one gains nothing: only its owner
//! and server admins may see or cancel a request.
//!
//! **Cancelling** a call sets its flag and wakes its future
//! ([`Call::run`]), which then answers `cancelled` and drops the inner
//! future: that drops the worker pool's job, whose token stops the core's
//! algorithm, as when a client goes away. A call whose answer is ready
//! when its future is next polled answers that instead: the cancel came
//! too late. Commits and other changes can't be cancelled (their outcome
//! would only become unknown); they are listed with `cancellable: false`.
//!
//! **Managed jobs** (step 16f, ADR 0056) take their ids from the same
//! counter ([`Requests::next_id`]) and are listed here while they are
//! queued or running ([`Requests::register`]); cancelling one calls the job
//! registry's cancel.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use iwdb_engine::CommitTime;
use iwdb_engine::metrics::{Histogram, HistogramSnapshot};

use crate::auth::Operation;
use crate::{Code, Error};

/// The message a cancelled call answers with.
pub const CANCELLED: &str = "the request was cancelled (CancelRequest)";

/// Change-stream readers the registry remembers; the one polled longest
/// ago makes room for a new one.
pub const MAX_CONSUMERS: usize = 1024;

/// A change-stream reader is forgotten this long after its last poll.
pub const CONSUMER_EXPIRY: Duration = Duration::from_secs(60);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running request, as [`Requests::list`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestInfo {
    pub id: u64,
    pub operation: Operation,
    /// The namespace it is on, if it is a namespace operation.
    pub namespace: Option<String>,
    /// The principal's user.
    pub user: String,
    /// The client's address, if the call came over the network.
    pub client: Option<IpAddr>,
    /// When it started.
    pub started: CommitTime,
    /// How long it has run.
    pub elapsed: Duration,
    /// Whether [`Requests::cancel`] can cancel it.
    pub cancellable: bool,
}

/// A change-stream reader ([`Requests::consumers`]): a user, from a
/// client, reading a namespace's changes, as of its last successful poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerInfo {
    pub user: String,
    pub client: Option<IpAddr>,
    pub namespace: String,
    /// The seq it reads from next (the last batch's `next_seq`).
    pub next_seq: u64,
    /// Commits it hasn't read yet: the namespace's streamable seq when
    /// listed, minus `next_seq - 1` (0 if it is up to date). Filled in by
    /// the database, which knows the seq.
    pub lag: u64,
    pub last_poll: CommitTime,
    /// Successful polls so far.
    pub polls: u64,
}

/// What cancelling registered work does ([`Requests::register`]).
pub type CancelHook = Box<dyn Fn() + Send + Sync>;

/// A request's cancel flag and the waker of its future; or, for work
/// registered without a future (a job), its hook.
#[derive(Default)]
struct Cancel {
    set: AtomicBool,
    waker: Mutex<Option<Waker>>,
    hook: Option<CancelHook>,
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancel").field("set", &self.set).finish_non_exhaustive()
    }
}

impl Cancel {
    /// Fire it. Called without the registry's lock: a hook may take other
    /// locks, and unregister work.
    fn fire(&self) {
        self.set.store(true, Ordering::Release);
        if let Some(waker) = lock(&self.waker).take() {
            waker.wake();
        }
        if let Some(hook) = &self.hook {
            hook();
        }
    }

    /// Whether it fired; if not, `waker` is woken when it does.
    fn fired_or_register(&self, waker: &Waker) -> bool {
        if self.set.load(Ordering::Acquire) {
            return true;
        }
        *lock(&self.waker) = Some(waker.clone());
        // A fire between the check and the registration found no waker
        self.set.load(Ordering::Acquire)
    }
}

struct Entry {
    info: RequestInfo,
    started: Instant,
    cancel: Arc<Cancel>,
}

/// Per operation: calls by outcome, and their durations.
struct OperationStats {
    /// Index 0: succeeded; `1 + i`: failed with `Code::ALL[i]`.
    outcomes: [AtomicU64; Code::ALL.len() + 1],
    durations: Histogram,
}

impl Default for OperationStats {
    fn default() -> Self {
        OperationStats { outcomes: std::array::from_fn(|_| AtomicU64::new(0)), durations: Histogram::new() }
    }
}

/// An operation's calls so far ([`Requests::stats`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationTotals {
    pub operation: Operation,
    /// Calls by outcome (`None`: succeeded), only those with any.
    pub outcomes: Vec<(Option<Code>, u64)>,
    pub durations: HistogramSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ConsumerKey {
    namespace: String,
    user: String,
    client: Option<IpAddr>,
}

struct Consumer {
    next_seq: u64,
    last_poll: Instant,
    last_poll_time: CommitTime,
    polls: u64,
}

/// The running requests, the totals of those that ended, and the
/// change-stream readers. One per database; shared through an `Arc`.
pub struct Requests {
    running: Mutex<BTreeMap<u64, Entry>>,
    next_id: AtomicU64,
    stats: Vec<OperationStats>,
    consumers: Mutex<BTreeMap<ConsumerKey, Consumer>>,
}

impl Default for Requests {
    fn default() -> Self {
        Requests {
            running: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            stats: Operation::ALL.iter().map(|_| OperationStats::default()).collect(),
            consumers: Mutex::new(BTreeMap::new()),
        }
    }
}

impl std::fmt::Debug for Requests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Requests").field("running", &lock(&self.running).len()).finish_non_exhaustive()
    }
}

fn index_of(op: Operation) -> usize {
    Operation::ALL.iter().position(|&o| o == op).unwrap_or(0)
}

impl Requests {
    pub fn new() -> Arc<Self> {
        Arc::new(Requests::default())
    }

    /// A new id from the counter that numbers requests: also for managed
    /// jobs, so that requests and jobs share one id space (ADR 0056).
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register work that runs outside a call (a managed job, ADR 0056) as
    /// request `id` of `operation`: listed, cancellable, until the returned
    /// [`Registration`] is dropped. Cancelling it calls `on_cancel` (without
    /// the registry's lock). It isn't counted in the request metrics when
    /// it ends. O(log running).
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        self: &Arc<Self>,
        id: u64,
        operation: Operation,
        namespace: Option<&str>,
        user: &str,
        client: Option<IpAddr>,
        started: CommitTime,
        on_cancel: CancelHook,
    ) -> Registration {
        let cancel = Arc::new(Cancel { hook: Some(on_cancel), ..Cancel::default() });
        let info = RequestInfo {
            id,
            operation,
            namespace: namespace.map(str::to_owned),
            user: user.to_owned(),
            client,
            started,
            elapsed: Duration::ZERO,
            cancellable: true,
        };
        lock(&self.running).insert(id, Entry { info, started: Instant::now(), cancel });
        Registration { requests: self.clone(), id }
    }

    /// Register a call of `operation` by `user` from `client`: running until
    /// the returned [`Call`] is dropped. O(log running).
    pub fn begin(
        self: &Arc<Self>,
        operation: Operation,
        namespace: Option<&str>,
        user: &str,
        client: Option<IpAddr>,
    ) -> Call {
        let id = self.next_id();
        let cancel = Arc::new(Cancel::default());
        let info = RequestInfo {
            id,
            operation,
            namespace: namespace.map(str::to_owned),
            user: user.to_owned(),
            client,
            started: CommitTime::now(),
            elapsed: Duration::ZERO,
            cancellable: operation.cancellable(),
        };
        let started = Instant::now();
        lock(&self.running).insert(id, Entry { info, started, cancel: cancel.clone() });
        Call { requests: self.clone(), id, operation, started, cancel, outcome: None }
    }

    /// Count a call that was refused before it ran (it never was
    /// registered): `unauthenticated` or `permission_denied`.
    pub fn refused(&self, operation: Operation, code: Code) {
        self.count(operation, Some(code), Duration::ZERO);
    }

    fn count(&self, operation: Operation, code: Option<Code>, took: Duration) {
        let stats = &self.stats[index_of(operation)];
        let slot = code.and_then(|c| Code::ALL.iter().position(|&k| k == c)).map_or(0, |i| i + 1);
        stats.outcomes[slot].fetch_add(1, Ordering::Relaxed);
        stats.durations.observe(took);
    }

    /// The running requests (only `user`'s, if given), oldest first, at
    /// most `limit`; and whether there were more. O(running).
    pub fn list(&self, user: Option<&str>, limit: usize) -> (Vec<RequestInfo>, bool) {
        let running = lock(&self.running);
        let mut matching = running.values().filter(|e| user.is_none_or(|u| e.info.user == u));
        let list: Vec<RequestInfo> = matching
            .by_ref()
            .take(limit)
            .map(|e| RequestInfo { elapsed: e.started.elapsed(), ..e.info.clone() })
            .collect();
        let more = matching.next().is_some();
        (list, more)
    }

    /// How many requests are running.
    pub fn active(&self) -> usize {
        lock(&self.running).len()
    }

    /// Cancel request `id` (only if it is `user`'s, if given). Returns it
    /// as it was. Errors: `not_found` if no such request is running (or it
    /// is someone else's); `invalid_argument` if it can't be cancelled.
    pub fn cancel(&self, id: u64, user: Option<&str>) -> Result<RequestInfo, Error> {
        let running = lock(&self.running);
        let entry = running
            .get(&id)
            .filter(|e| user.is_none_or(|u| e.info.user == u))
            .ok_or_else(|| Error::new(Code::NotFound, format!("no request {} is running", id)))?;
        if !entry.info.cancellable {
            return Err(Error::invalid(format!(
                "request {} is a {}, which can't be cancelled: its outcome would only become unknown",
                id,
                entry.info.operation.name()
            )));
        }
        let info = RequestInfo { elapsed: entry.started.elapsed(), ..entry.info.clone() };
        let cancel = entry.cancel.clone();
        drop(running);
        cancel.fire();
        Ok(info)
    }

    /// Every operation's calls that ended (and refusals), in the order of
    /// [`Operation::ALL`].
    pub fn stats(&self) -> Vec<OperationTotals> {
        Operation::ALL
            .iter()
            .zip(&self.stats)
            .map(|(&operation, stats)| {
                let count = |i: usize| stats.outcomes[i].load(Ordering::Relaxed);
                let outcomes = std::iter::once((None, count(0)))
                    .chain(Code::ALL.iter().enumerate().map(|(i, &c)| (Some(c), count(i + 1))))
                    .filter(|(_, n)| *n > 0)
                    .collect();
                OperationTotals { operation, outcomes, durations: stats.durations.snapshot() }
            })
            .collect()
    }

    /// Remember a successful poll of `namespace`'s changes, after which
    /// the reader reads from `next_seq`.
    pub fn polled(&self, namespace: &str, user: &str, client: Option<IpAddr>, next_seq: u64) {
        let key = ConsumerKey { namespace: namespace.to_owned(), user: user.to_owned(), client };
        let mut consumers = lock(&self.consumers);
        let now = Instant::now();
        consumers.retain(|_, c| now.duration_since(c.last_poll) < CONSUMER_EXPIRY);
        if !consumers.contains_key(&key)
            && consumers.len() >= MAX_CONSUMERS
            && let Some(oldest) = consumers.iter().min_by_key(|(_, c)| c.last_poll).map(|(k, _)| k.clone())
        {
            consumers.remove(&oldest);
        }
        let consumer = consumers.entry(key).or_insert(Consumer {
            next_seq,
            last_poll: now,
            last_poll_time: CommitTime::now(),
            polls: 0,
        });
        consumer.next_seq = next_seq;
        consumer.last_poll = now;
        consumer.last_poll_time = CommitTime::now();
        consumer.polls += 1;
    }

    /// The change-stream readers that polled in the last
    /// [`CONSUMER_EXPIRY`], by namespace, user and client; `lag` is 0
    /// (the database fills it in). At most [`MAX_CONSUMERS`].
    pub fn consumers(&self) -> Vec<ConsumerInfo> {
        let now = Instant::now();
        lock(&self.consumers)
            .iter()
            .filter(|(_, c)| now.duration_since(c.last_poll) < CONSUMER_EXPIRY)
            .map(|(k, c)| ConsumerInfo {
                user: k.user.clone(),
                client: k.client,
                namespace: k.namespace.clone(),
                next_seq: c.next_seq,
                lag: 0,
                last_poll: c.last_poll_time,
                polls: c.polls,
            })
            .collect()
    }
}

/// A registered call ([`Requests::begin`]): run its future with
/// [`run`](Self::run). When it is dropped the call is no longer running,
/// and it is counted with its outcome (`cancelled` if its future was
/// dropped before it finished: the client went away).
pub struct Call {
    requests: Arc<Requests>,
    id: u64,
    operation: Operation,
    started: Instant,
    cancel: Arc<Cancel>,
    /// The outcome, once known: `Some(None)` succeeded.
    outcome: Option<Option<Code>>,
}

impl Call {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Run the call's future until it ends or the call is cancelled
    /// ([`Requests::cancel`]): then it answers `cancelled`, unless its
    /// answer is ready when it is polled, and drops `future`.
    pub async fn run<T>(mut self, future: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        let cancel = self.cancel.clone();
        let cancellable = self.operation.cancellable();
        let mut future = std::pin::pin!(future);
        let result = std::future::poll_fn(|cx| {
            if let Poll::Ready(result) = future.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            if cancellable && cancel.fired_or_register(cx.waker()) {
                return Poll::Ready(Err(Error::new(Code::Cancelled, CANCELLED)));
            }
            Poll::Pending
        })
        .await;
        self.outcome = Some(result.as_ref().err().map(Error::code));
        result
    }
}

/// Registered work ([`Requests::register`]): listed until dropped.
pub struct Registration {
    requests: Arc<Requests>,
    id: u64,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration").field("id", &self.id).finish()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock(&self.requests.running).remove(&self.id);
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        lock(&self.requests.running).remove(&self.id);
        let code = self.outcome.unwrap_or(Some(Code::Cancelled));
        self.requests.count(self.operation, code, self.started.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::block_on;

    fn ann(requests: &Arc<Requests>, op: Operation) -> Call {
        requests.begin(op, Some("social"), "ann", None)
    }

    #[test]
    fn calls_are_listed_while_they_run_and_counted_when_they_end() {
        let requests = Requests::new();
        let call = ann(&requests, Operation::Find);
        let other = requests.begin(Operation::Commit, None, "bob", None);
        let (list, more) = requests.list(None, 10);
        assert_eq!(list.iter().map(|r| r.id).collect::<Vec<_>>(), vec![call.id(), other.id()]);
        assert!(!more);
        assert!(list[0].cancellable && !list[1].cancellable);
        assert_eq!(list[0].namespace.as_deref(), Some("social"));
        assert_eq!(requests.list(Some("bob"), 10).0.len(), 1);
        let (first, more) = requests.list(None, 1);
        assert_eq!((first.len(), first[0].id, more), (1, call.id(), true));
        assert_eq!(block_on(call.run(async { Ok::<_, Error>(7) })), Ok(7));
        drop(other);
        assert_eq!(requests.active(), 0);
        let stats = requests.stats();
        let find = stats.iter().find(|s| s.operation == Operation::Find).unwrap();
        assert_eq!(find.outcomes, vec![(None, 1)]);
        assert_eq!(find.durations.count(), 1);
        // Dropped before it ended: the client went away
        let commit = stats.iter().find(|s| s.operation == Operation::Commit).unwrap();
        assert_eq!(commit.outcomes, vec![(Some(Code::Cancelled), 1)]);
        requests.refused(Operation::Find, Code::PermissionDenied);
        let find = requests.stats().into_iter().find(|s| s.operation == Operation::Find).unwrap();
        assert_eq!(find.outcomes, vec![(None, 1), (Some(Code::PermissionDenied), 1)]);
    }

    #[test]
    fn a_cancelled_call_answers_cancelled_and_drops_its_future() {
        let requests = Requests::new();
        let call = ann(&requests, Operation::Find);
        let id = call.id();
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel::<()>();
        struct OnDrop(std::sync::mpsc::Sender<()>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let thread = {
            let requests = requests.clone();
            std::thread::spawn(move || {
                // Until the call waits
                while requests.active() == 0 {
                    std::thread::yield_now();
                }
                std::thread::sleep(Duration::from_millis(20));
                assert_eq!(requests.cancel(id, Some("bob")).unwrap_err().code(), Code::NotFound);
                requests.cancel(id, Some("ann")).unwrap()
            })
        };
        let result = block_on(call.run(async move {
            let _guard = OnDrop(dropped_tx);
            std::future::pending::<Result<(), Error>>().await
        }));
        let error = result.unwrap_err();
        assert_eq!((error.code(), error.message()), (Code::Cancelled, CANCELLED));
        assert!(dropped_rx.try_recv().is_ok(), "the inner future was dropped");
        assert_eq!(thread.join().unwrap().id, id);
        assert_eq!(requests.cancel(id, None).unwrap_err().code(), Code::NotFound, "it ended");
    }

    #[test]
    fn an_answer_that_is_ready_wins_over_a_cancel() {
        let requests = Requests::new();
        let call = ann(&requests, Operation::Find);
        requests.cancel(call.id(), None).unwrap();
        assert_eq!(block_on(call.run(async { Ok::<_, Error>(1) })), Ok(1));
    }

    #[test]
    fn changes_can_not_be_cancelled() {
        let requests = Requests::new();
        let call = ann(&requests, Operation::Commit);
        let e = requests.cancel(call.id(), None).unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument);
        assert!(e.message().contains("Commit"), "{}", e);
    }

    #[test]
    fn registered_work_shares_the_ids_and_is_cancelled_through_its_hook() {
        let requests = Requests::new();
        let call = ann(&requests, Operation::Find);
        let id = requests.next_id();
        assert!(id > call.id());
        let fired = Arc::new(AtomicU64::new(0));
        let hook = {
            let (fired, requests) = (fired.clone(), Arc::downgrade(&requests));
            // A hook may use the registry: it runs without its lock
            Box::new(move || {
                fired.fetch_add(1, Ordering::Relaxed);
                assert!(requests.upgrade().is_some_and(|r| r.active() > 0));
            })
        };
        let job = requests.register(id, Operation::StartJob, Some("social"), "ann", None, CommitTime::now(), hook);
        let listed = requests.list(None, 10).0;
        assert_eq!(
            listed.iter().map(|r| (r.id, r.cancellable)).collect::<Vec<_>>(),
            vec![(call.id(), true), (id, true)]
        );
        assert_eq!(requests.cancel(id, Some("bob")).unwrap_err().code(), Code::NotFound);
        assert_eq!(requests.cancel(id, Some("ann")).unwrap().operation, Operation::StartJob);
        assert_eq!(fired.load(Ordering::Relaxed), 1);
        drop(job);
        drop(call);
        assert_eq!(requests.active(), 0);
        // Registered work isn't counted when it ends
        let start = requests.stats().into_iter().find(|s| s.operation == Operation::StartJob).unwrap();
        assert!(start.outcomes.is_empty(), "{:?}", start.outcomes);
    }

    #[test]
    fn consumers_are_remembered_by_namespace_user_and_client() {
        let requests = Requests::new();
        requests.polled("social", "ann", None, 5);
        requests.polled("social", "ann", None, 9);
        requests.polled("social", "bob", None, 1);
        let list = requests.consumers();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].user.as_str(), list[0].next_seq, list[0].polls), ("ann", 9, 2));
        for i in 0..MAX_CONSUMERS + 5 {
            requests.polled("orders", &format!("u{}", i), None, 1);
        }
        assert_eq!(requests.consumers().len(), MAX_CONSUMERS);
    }
}
