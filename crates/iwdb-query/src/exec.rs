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
//! went away stops at the core's next check.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::{pin, Pin};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};

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

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    capacity: usize,
}

/// A fixed set of worker threads with a bounded queue.
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
    /// `capacity` queued jobs (at least 1) beyond those running.
    pub fn new(name: &str, workers: usize, capacity: usize) -> Result<Pool, Error> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue { jobs: VecDeque::new(), shutdown: false }),
            ready: Condvar::new(),
            capacity: capacity.max(1),
        });
        let pool = Pool { shared, threads: Mutex::new(Vec::new()) };
        for i in 0..workers.max(1) {
            let shared = pool.shared.clone();
            let thread = thread::Builder::new()
                .name(format!("{}-{}", name, i))
                .spawn(move || work(&shared))
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
        let token = Token::new();
        let slot = Arc::new(Slot::default());
        let (job_token, job_slot) = (token.clone(), slot.clone());
        let run: Job = Box::new(move || {
            let result = match panic::catch_unwind(AssertUnwindSafe(|| job(&job_token))) {
                Ok(result) => result,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "(no message)".into());
                    Err(Error::internal(format!("internal error (a panic, please report it): {}", message)))
                }
            };
            job_slot.fill(result);
        });
        let mut queue = lock(&self.shared.queue);
        if queue.shutdown {
            return Pending::ready(Err(Error::unavailable("the store is shutting down")));
        }
        if queue.jobs.len() >= self.shared.capacity {
            return Pending::ready(Err(Error::unavailable(format!(
                "too many requests: {} are queued already",
                queue.jobs.len()
            ))));
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

struct Slot<T> {
    state: Mutex<(Option<T>, Option<Waker>)>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot { state: Mutex::new((None, None)) }
    }
}

impl<T> Slot<T> {
    fn fill(&self, value: T) {
        let waker = {
            let mut state = lock(&self.state);
            state.0 = Some(value);
            state.1.take()
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
        match state.0.take() {
            Some(value) => {
                this.token = None;
                Poll::Ready(value)
            }
            None => {
                state.1 = Some(cx.waker().clone());
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
            let ready = p.slot.state.lock().unwrap().0.take();
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
}
