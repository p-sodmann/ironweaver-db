//! Running blocking work behind async methods, without an async runtime
//! (ADR 0020).
//!
//! The engine is synchronous: a read holds a namespace's read lock and runs
//! the core's algorithms under a thread-local cancel token
//! (`ironweaver_core::cancel::run`), and a commit waits for its fsync.
//! Neither may run on an async executor's threads. A [`Pool`] runs such
//! work on its own threads and hands back a [`Pending`] future, which any
//! executor can poll (tokio in the server, [`block_on`] in Python and the
//! tests). Dropping a `Pending` cancels its token, so a read whose caller
//! went away stops at the core's next check. A job with a deadline
//! ([`Pool::submit_until`]) ends at it even while it waits in the queue.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::{Pin, pin};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::Instant;

use ironweaver_core::cancel::Token;

use crate::Error;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type Job = Box<dyn FnOnce() + Send>;

struct Queue {
    jobs: VecDeque<Job>,
    shutdown: bool,
}

/// What happens to a job at its deadline: its token is cancelled and its
/// future resolves with the deadline's error, unless it finished first.
type Expire = Box<dyn FnOnce() + Send>;

/// The deadlines of the jobs that have one, by time (and a counter, so
/// that equal times don't collide).
#[derive(Default)]
struct Timers {
    due: BTreeMap<(Instant, u64), Expire>,
    next: u64,
    shutdown: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    capacity: usize,
    timers: Mutex<Timers>,
    timer_changed: Condvar,
}

/// A fixed set of worker threads with a bounded queue, and a timer thread
/// for deadlines.
pub struct Pool {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool").field("capacity", &self.shared.capacity).finish_non_exhaustive()
    }
}

impl Pool {
    /// Start `workers` threads (at least 1) named `name`, taking at most
    /// `capacity` queued jobs (at least 1) beyond those running, and a timer
    /// thread.
    pub fn new(name: &str, workers: usize, capacity: usize) -> Result<Pool, Error> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue { jobs: VecDeque::new(), shutdown: false }),
            ready: Condvar::new(),
            capacity: capacity.max(1),
            timers: Mutex::new(Timers::default()),
            timer_changed: Condvar::new(),
        });
        let pool = Pool { shared, threads: Mutex::new(Vec::new()) };
        let threads =
            (0..workers.max(1)).map(|i| (format!("{}-{}", name, i), false)).chain([(format!("{}-timer", name), true)]);
        for (thread_name, timer) in threads {
            let shared = pool.shared.clone();
            let thread = thread::Builder::new()
                .name(thread_name)
                .spawn(move || if timer { time(&shared) } else { work(&shared) })
                .map_err(|e| Error::internal(format!("can't start a worker thread: {}", e)));
            match thread {
                Ok(thread) => lock(&pool.threads).push(thread),
                Err(e) => {
                    pool.shutdown();
                    return Err(e);
                }
            }
        }
        Ok(pool)
    }

    /// Run `job` on a worker. The job gets a token that is cancelled when
    /// the returned future is dropped before it completes. A panic in the
    /// job is an `internal` error. Fails at once with `unavailable` if the
    /// queue is full or the pool is shut down.
    pub fn submit<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Token) -> Result<T, Error> + Send + 'static,
    ) -> Pending<Result<T, Error>> {
        self.submit_until(None, job)
    }

    /// [`submit`](Self::submit) with a deadline: at `deadline.0`, if the job
    /// hasn't finished, the future resolves with the error `deadline.1` and
    /// the job's token is cancelled, whether the job is running (it should
    /// stop at its next check of the token) or still queued (it then runs
    /// with a cancelled token, and its result is dropped). So a request
    /// queued behind slow ones ends at its deadline, not when a worker is
    /// free.
    pub fn submit_until<T: Send + 'static>(
        &self,
        deadline: Option<(Instant, Error)>,
        job: impl FnOnce(&Token) -> Result<T, Error> + Send + 'static,
    ) -> Pending<Result<T, Error>> {
        let token = Token::new();
        let slot = Arc::new(Slot::default());
        let timer = deadline.map(|(at, error)| {
            let (token, slot) = (token.clone(), slot.clone());
            let expire: Expire = Box::new(move || {
                token.cancel();
                slot.fill(Err(error));
            });
            let mut timers = lock(&self.shared.timers);
            let key = (at, timers.next);
            timers.next += 1;
            let earliest = timers.due.keys().next().is_none_or(|first| key < *first);
            timers.due.insert(key, expire);
            drop(timers);
            if earliest {
                self.shared.timer_changed.notify_one();
            }
            key
        });
        let (job_token, job_slot, shared) = (token.clone(), slot.clone(), self.shared.clone());
        let run: Job = Box::new(move || {
            let result = catch(|| job(&job_token));
            if let Some(key) = timer {
                lock(&shared.timers).due.remove(&key);
            }
            job_slot.fill(result);
        });
        let mut queue = lock(&self.shared.queue);
        let refused = if queue.shutdown {
            Some(Error::unavailable("the store is shutting down"))
        } else if queue.jobs.len() >= self.shared.capacity {
            Some(Error::unavailable(format!("too many requests: {} are queued already", queue.jobs.len())))
        } else {
            None
        };
        if let Some(e) = refused {
            drop(queue);
            if let Some(key) = timer {
                lock(&self.shared.timers).due.remove(&key);
            }
            return Pending::ready(Err(e));
        }
        queue.jobs.push_back(run);
        drop(queue);
        self.shared.ready.notify_one();
        Pending { slot, token: Some(token) }
    }

    /// Stop taking jobs, let the workers finish the queued ones, and join
    /// them. Idempotent.
    pub fn shutdown(&self) {
        lock(&self.shared.queue).shutdown = true;
        self.shared.ready.notify_all();
        lock(&self.shared.timers).shutdown = true;
        self.shared.timer_changed.notify_all();
        let threads: Vec<JoinHandle<()>> = lock(&self.threads).drain(..).collect();
        for thread in threads {
            // A worker catches its jobs' panics; nothing is left to clean up
            let _ = thread.join();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn work(shared: &Shared) {
    loop {
        let job = {
            let mut queue = lock(&shared.queue);
            loop {
                if let Some(job) = queue.jobs.pop_front() {
                    break job;
                }
                if queue.shutdown {
                    return;
                }
                queue = shared.ready.wait(queue).unwrap_or_else(PoisonError::into_inner);
            }
        };
        job();
    }
}

/// The timer thread: expire each deadline when it comes. At shutdown the
/// workers run what is queued, so the remaining deadlines are dropped.
fn time(shared: &Shared) {
    let mut timers = lock(&shared.timers);
    loop {
        if timers.shutdown {
            return;
        }
        let now = Instant::now();
        match timers.due.keys().next().copied() {
            Some(key) if key.0 <= now => {
                if let Some(expire) = timers.due.remove(&key) {
                    drop(timers);
                    expire();
                    timers = lock(&shared.timers);
                }
            }
            Some((at, _)) => {
                timers = shared.timer_changed.wait_timeout(timers, at - now).unwrap_or_else(PoisonError::into_inner).0;
            }
            None => timers = shared.timer_changed.wait(timers).unwrap_or_else(PoisonError::into_inner),
        }
    }
}

/// Run `job`, turning a panic into an `internal` error.
pub(crate) fn catch<T>(job: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    match panic::catch_unwind(AssertUnwindSafe(job)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            Err(Error::internal(format!("internal error (a panic, please report it): {}", message)))
        }
    }
}

/// Run `job` on a new thread named `name`, not on a pool: for long jobs
/// that would otherwise hold a worker (backups, checkpoints, ADR 0055).
/// Dropping the future doesn't stop the job; its result is dropped then. A
/// panic is an `internal` error, and a thread that can't start too.
pub fn spawn<T: Send + 'static>(
    name: &str,
    job: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Pending<Result<T, Error>> {
    let slot = Arc::new(Slot::default());
    let filled = slot.clone();
    let started = std::thread::Builder::new().name(name.to_owned()).spawn(move || filled.fill(catch(job)));
    match started {
        Ok(_) => Pending { slot, token: None },
        Err(e) => Pending::ready(Err(Error::internal(format!("can't start a thread: {}", e)))),
    }
}

struct SlotState<T> {
    value: Option<T>,
    waker: Option<Waker>,
    /// A value was put in (and may have been taken since): later ones are
    /// dropped.
    filled: bool,
}

struct Slot<T> {
    state: Mutex<SlotState<T>>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot { state: Mutex::new(SlotState { value: None, waker: None, filled: false }) }
    }
}

impl<T> Slot<T> {
    /// Put in the result, unless one was put in before (a job that
    /// finishes after its deadline, a deadline after its job finished).
    fn fill(&self, value: T) {
        let waker = {
            let mut state = lock(&self.state);
            if state.filled {
                return;
            }
            state.filled = true;
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The result of a job on a [`Pool`], as a future. Dropping it before it
/// completes cancels the job's token.
#[must_use = "a Pending does nothing useful unless awaited; dropping it cancels the job"]
pub struct Pending<T> {
    slot: Arc<Slot<T>>,
    /// `None` once the result was taken (or for a ready value).
    token: Option<Token>,
}

impl<T> Pending<T> {
    /// A future that is ready with `value`.
    pub fn ready(value: T) -> Self {
        let slot = Arc::new(Slot::default());
        slot.fill(value);
        Pending { slot, token: None }
    }
}

impl<T> Future for Pending<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        let mut state = lock(&this.slot.state);
        match state.value.take() {
            Some(value) => {
                this.token = None;
                Poll::Ready(value)
            }
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        if let Some(token) = &self.token {
            token.cancel();
        }
    }
}

/// Run `future` to completion on this thread, parking it while the future
/// waits. For synchronous callers (Python, tests) of the
/// [`Database`](crate::Database) trait; don't call it on an async
/// executor's thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::Code;

    #[test]
    fn jobs_run_on_the_workers_and_their_results_arrive() {
        let pool = Pool::new("test", 2, 16).unwrap();
        let futures: Vec<_> = (0..10u64).map(|i| pool.submit(move |_| Ok(i * i))).collect();
        let results: Vec<u64> = futures.into_iter().map(|f| block_on(f).unwrap()).collect();
        assert_eq!(results, (0..10u64).map(|i| i * i).collect::<Vec<_>>());
        let name = block_on(pool.submit(|_| Ok(thread::current().name().map(str::to_owned)))).unwrap();
        assert!(name.is_some_and(|n| n.starts_with("test-")));
    }

    #[test]
    fn dropping_the_future_cancels_the_job() {
        let pool = Pool::new("test", 1, 4).unwrap();
        let (tx, rx) = mpsc::channel();
        let pending = pool.submit(move |token| {
            let start = Instant::now();
            while !token.is_cancelled() {
                assert!(start.elapsed() < Duration::from_secs(10), "never cancelled");
                thread::sleep(Duration::from_millis(1));
            }
            tx.send(()).unwrap();
            Ok(())
        });
        thread::sleep(Duration::from_millis(5));
        drop(pending);
        rx.recv_timeout(Duration::from_secs(10)).expect("the job saw the cancellation");
    }

    #[test]
    fn a_full_queue_or_a_shut_down_pool_is_unavailable_and_panics_are_internal() {
        let pool = Pool::new("test", 1, 1).unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        let running = pool.submit(move |_| {
            rx.recv().ok();
            Ok(1)
        });
        // The worker may not have taken the first job yet: fill until full
        let mut queued = Vec::new();
        let full = loop {
            let p = pool.submit(|_| Ok(2));
            let ready = p.slot.state.lock().unwrap().value.take();
            match ready {
                Some(Err(e)) => break e,
                _ => queued.push(p),
            }
            assert!(queued.len() < 3, "the queue never filled");
        };
        assert_eq!(full.code(), Code::Unavailable);
        tx.send(()).unwrap();
        assert_eq!(block_on(running).unwrap(), 1);
        let panicked = block_on(pool.submit(|_| -> Result<(), Error> { panic!("boom") })).unwrap_err();
        assert_eq!(panicked.code(), Code::Internal);
        assert!(panicked.message().contains("boom"));
        pool.shutdown();
        assert_eq!(block_on(pool.submit(|_| Ok(()))).unwrap_err().code(), Code::Unavailable);
    }

    #[test]
    fn a_queued_job_ends_at_its_deadline() {
        let pool = Pool::new("test", 1, 4).unwrap();
        let (release, held) = mpsc::channel::<()>();
        let busy = pool.submit(move |_| {
            held.recv().ok();
            Ok(0)
        });
        let (ran, saw) = mpsc::channel();
        let start = Instant::now();
        let deadline = Some((start + Duration::from_millis(50), Error::new(Code::Timeout, "too late")));
        let queued = pool.submit_until(deadline, move |token| {
            ran.send(token.is_cancelled()).unwrap();
            Ok(1)
        });
        let e = block_on(queued).unwrap_err();
        assert_eq!((e.code(), e.message()), (Code::Timeout, "too late"));
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(50) && took < Duration::from_secs(5), "{:?}", took);
        // When the worker gets to it, the job sees its cancelled token
        release.send(()).unwrap();
        assert_eq!(block_on(busy).unwrap(), 0);
        assert!(saw.recv_timeout(Duration::from_secs(10)).unwrap());
        // A job that finishes first keeps its result, and its deadline is gone
        let soon = Some((Instant::now() + Duration::from_secs(60), Error::new(Code::Timeout, "never")));
        assert_eq!(block_on(pool.submit_until(soon, |_| Ok(2))).unwrap(), 2);
        assert!(lock(&pool.shared.timers).due.is_empty());
    }
}
