//! Managed analytics jobs (step 16f, ADR 0056): the registry behind the
//! job methods of [`Admin`](crate::Admin), and the threads that run them.
//!
//! A job is queued by [`Jobs::start`] with its work (a closure the
//! database gives: `iwdb::Embedded` runs `Ns::analyze` and
//! [`read::run_job`](crate::read::run_job) in it), run by one of
//! [`JobsConfig::running`] threads of its own (never the query workers),
//! and kept with its result once it ends, until it expires.
//!
//! **Bounds** (design rule 5): at most [`JobsConfig::queued`] jobs wait and
//! [`JobsConfig::per_user`] are a user's at once (`unavailable` beyond); a
//! job runs at most [`JobsConfig::timeout`]; ended jobs are kept for
//! [`JobsConfig::retention`] and at most [`JobsConfig::max_finished`] of
//! them (the oldest go first: `not_found`); stored results hold at most
//! [`JobsConfig::result_bytes`] together (the oldest are dropped first,
//! their jobs [`JobState::Expired`]). Expiry is checked whenever the
//! registry is used, not by a timer.
//!
//! **Ids** come from the request registry's counter ([`Requests::next_id`]),
//! and a queued or running job is listed there as a `StartJob` request, so
//! `CancelRequest` cancels it too.
//!
//! **Memory** (ADR 0054): stored results are charged to the `working` part
//! by their estimate ([`result_bytes`]) until they are dropped. The work
//! charges its own projection.

use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ironweaver_core::cancel::{Progress, Token};
use iwdb_engine::CommitTime;
use iwdb_storage::memory::{Charge, Memory, Part};

use crate::auth::Operation;
use crate::requests::{Registration, Requests};
use crate::{Code, Error, JobResult};

/// The most rows a page of a result holds (and its default).
pub const MAX_PAGE_ROWS: usize = 10_000;

/// A page of a result stops before about this many bytes (estimated), after
/// at least one row.
pub const PAGE_BYTES: u64 = 4 << 20;

/// The message of a job cancelled by `CancelJob` or `CancelRequest`.
pub const CANCELLED: &str = "the job was cancelled (CancelJob)";

/// The message of a job cancelled because the server drains.
pub const SHUTTING_DOWN: &str = "the server is shutting down";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The job registry's bounds (`[jobs]`, ADR 0056).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobsConfig {
    /// Jobs running at once, each on a thread of its own. Default 2.
    pub running: usize,
    /// Jobs that may wait for a thread; more fail with `unavailable`.
    /// Default 16.
    pub queued: usize,
    /// A user's queued and running jobs at once; more fail with
    /// `unavailable`. Default 4.
    pub per_user: usize,
    /// The longest a job runs, from when it leaves the queue; it fails with
    /// `timeout` then. A request's `timeout` can only lower it. Default 1 h.
    pub timeout: Duration,
    /// How long an ended job is kept. Default 1 h.
    pub retention: Duration,
    /// Ended jobs kept at most. Default 100.
    pub max_finished: usize,
    /// Stored results' estimated bytes, together, at most. Default 64 MiB.
    pub result_bytes: u64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        JobsConfig {
            running: 2,
            queued: 16,
            per_user: 4,
            timeout: Duration::from_secs(3600),
            retention: Duration::from_secs(3600),
            max_finished: 100,
            result_bytes: 64 << 20,
        }
    }
}

impl JobsConfig {
    /// Every count and the timeout must be at least 1. Errors:
    /// `invalid_argument`.
    pub fn check(&self) -> Result<(), Error> {
        let counts = [
            ("running", self.running),
            ("queued", self.queued),
            ("per_user", self.per_user),
            ("max_finished", self.max_finished),
        ];
        for (name, n) in counts {
            if n == 0 {
                return Err(Error::invalid(format!("[jobs] {} must be at least 1", name)));
            }
        }
        if self.timeout.is_zero() {
            return Err(Error::invalid("[jobs] timeout_secs must be at least 1"));
        }
        Ok(())
    }
}

/// Where a job is (ADR 0056).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Waiting for a job thread.
    Queued,
    /// Waiting for `min_seq`, then collecting the projection.
    Collecting,
    /// The algorithm runs, then the rows are ranked.
    Running,
    /// Ended; its result can be fetched.
    Done,
    /// Ended with an error ([`JobInfo::error`]).
    Failed,
    /// Cancelled (`CancelJob`, `CancelRequest`, or the server drains).
    Cancelled,
    /// Was done; its result was dropped to make room for newer ones.
    Expired,
}

impl JobState {
    pub const ALL: [JobState; 7] = [
        JobState::Queued,
        JobState::Collecting,
        JobState::Running,
        JobState::Done,
        JobState::Failed,
        JobState::Cancelled,
        JobState::Expired,
    ];

    /// `queued`, `collecting`, `running`, `done`, `failed`, `cancelled`,
    /// `expired`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Collecting => "collecting",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Expired => "expired",
        }
    }

    pub fn parse(name: &str) -> Option<JobState> {
        JobState::ALL.into_iter().find(|s| s.as_str() == name)
    }

    /// Whether the job has ended.
    pub fn ended(self) -> bool {
        !matches!(self, JobState::Queued | JobState::Collecting | JobState::Running)
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A job, as the registry reports it: its state and progress.
#[derive(Clone, Debug, PartialEq)]
pub struct JobInfo {
    /// From the request ids' counter: unique while the server runs.
    pub id: u64,
    pub namespace: String,
    /// Its owner: the user who started it.
    pub user: String,
    pub client: Option<IpAddr>,
    /// The job's name ([`Job::name`](crate::Job::name)).
    pub kind: String,
    pub state: JobState,
    pub created: CommitTime,
    /// When it left the queue.
    pub started: Option<CommitTime>,
    /// When it ended.
    pub ended: Option<CommitTime>,
    /// How long it has run (or ran): from leaving the queue to now or its
    /// end; zero while queued.
    pub elapsed: Duration,
    /// How far the algorithm has got, once it reports (the core's
    /// [`Progress`], upstream #62); the last report once ended.
    pub progress: Option<JobProgress>,
    /// The projection's size, once collecting started.
    pub nodes: Option<u64>,
    pub edges: Option<u64>,
    /// The seq of the state the projection saw, once done.
    pub seq: Option<u64>,
    /// The result's rows, once done.
    pub rows: Option<u64>,
    /// Rows were cut by `max_results`.
    pub truncated: bool,
    /// The stored result's estimated size (0 without one).
    pub result_bytes: u64,
    /// Why it failed or was cancelled.
    pub error: Option<Error>,
    /// When it will be removed, once ended.
    pub expires: Option<CommitTime>,
}

/// How far a job's algorithm has got: the core's report (upstream #62).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobProgress {
    /// The algorithm's phase, as the core names it (`pagerank`, `leiden`,
    /// `label propagation`, ...).
    pub phase: String,
    /// Units done in the phase: iterations (PageRank, label propagation),
    /// runs (Leiden), nodes or sources (the others).
    pub done: u64,
    /// The phase's units, if known. An algorithm that converges early ends
    /// below it.
    pub total: Option<u64>,
}

impl JobProgress {
    /// What `progress` reports, if it has reported anything.
    pub fn of(progress: &Progress) -> Option<JobProgress> {
        let s = progress.snapshot();
        (!s.phase.is_empty()).then(|| JobProgress { phase: s.phase.to_owned(), done: s.done, total: s.total })
    }
}

/// A page of a done job's result ([`Jobs::page`]).
#[derive(Clone, Debug, PartialEq)]
pub struct JobPage {
    pub job: JobInfo,
    /// Rows `offset..`, of the result's kind.
    pub rows: JobResult,
    /// The offset of the next page; `None`: this was the last.
    pub next_offset: Option<u64>,
}

/// The registry at a glance (the server status and the metrics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobCounts {
    pub queued: u64,
    pub running: u64,
    /// Ended and still kept.
    pub finished: u64,
    /// The stored results' estimated bytes.
    pub result_bytes: u64,
    /// Jobs that ended since the registry started, by outcome.
    pub done_total: u64,
    pub failed_total: u64,
    pub cancelled_total: u64,
}

/// What a job's work answers: its ranked rows, whether `max_results` cut
/// them, and the seq its projection saw.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub result: JobResult,
    pub truncated: bool,
    pub seq: u64,
}

/// A job's work, run on a job thread.
pub type JobWork = Box<dyn FnOnce(&JobHandle) -> Result<Finished, Error> + Send>;

/// What a job's work reports through, and its limits.
pub struct JobHandle {
    shared: Arc<Shared>,
    id: u64,
    token: Token,
    progress: Progress,
    timeout: Duration,
}

impl JobHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The job's cancel token: cancelled by a cancel or a drain.
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// Where the algorithm reports how far it has got: run it with
    /// `cancel::run_with_progress`.
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// How long the job may run, from now.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The projection to collect has `nodes` nodes and `edges` edges.
    pub fn collecting(&self, nodes: u64, edges: u64) {
        self.update(|e| {
            if let Some(span) = &e.span {
                span.record("iwdb.job.nodes", nodes);
                span.record("iwdb.job.edges", edges);
            }
            e.info.nodes = Some(nodes);
            e.info.edges = Some(edges);
        });
    }

    /// The projection is collected; the algorithm runs.
    pub fn running(&self) {
        self.update(|e| e.info.state = JobState::Running);
    }

    fn update(&self, f: impl FnOnce(&mut Entry)) {
        let mut state = lock(&self.shared.state);
        if let Some(entry) = state.entries.get_mut(&self.id).filter(|e| !e.info.state.ended()) {
            f(entry);
        }
    }
}

struct Entry {
    info: JobInfo,
    token: Token,
    progress: Progress,
    timeout: Duration,
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
    work: Option<JobWork>,
    registration: Option<Registration>,
    result: Option<Arc<JobResult>>,
    charge: Option<Charge>,
    /// The job's trace span (`iwdb.job`, ADR 0057), until it ends; and its
    /// wait for a job thread (`iwdb.queue`), until it leaves the queue.
    span: Option<tracing::Span>,
    queued: Option<tracing::Span>,
}

impl Entry {
    fn info(&self, retention: Duration) -> JobInfo {
        let elapsed = match (self.started_at, self.ended_at) {
            (Some(start), Some(end)) => end.saturating_duration_since(start),
            (Some(start), None) => start.elapsed(),
            _ => Duration::ZERO,
        };
        let retention = i64::try_from(retention.as_micros()).unwrap_or(i64::MAX);
        let expires = self.info.ended.map(|t| CommitTime(t.0.saturating_add(retention)));
        // Read live while it runs; `end` keeps the last report
        let progress =
            if self.info.state.ended() { self.info.progress.clone() } else { JobProgress::of(&self.progress) };
        JobInfo { elapsed, expires, progress, ..self.info.clone() }
    }

    fn drop_result(&mut self, bytes: &mut u64) {
        *bytes = bytes.saturating_sub(self.info.result_bytes);
        self.info.result_bytes = 0;
        self.result = None;
        self.charge = None;
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<u64, Entry>,
    queue: VecDeque<u64>,
    /// Draining: starts are refused.
    closed: bool,
    /// The threads stop.
    shutdown: bool,
    result_bytes: u64,
}

struct Shared {
    config: JobsConfig,
    requests: Arc<Requests>,
    memory: Option<Arc<Memory>>,
    state: Mutex<State>,
    ready: Condvar,
    done: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
}

/// The job registry and its threads. One per database.
pub struct Jobs {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs").field("config", &self.shared.config).finish_non_exhaustive()
    }
}

impl Jobs {
    /// A registry with `config`'s bounds, taking ids from `requests` and
    /// charging stored results to `memory`'s working part. Starts
    /// `config.running` threads. Errors: `invalid_argument` for invalid
    /// bounds; `internal` if a thread can't start.
    pub fn new(config: JobsConfig, requests: Arc<Requests>, memory: Option<Arc<Memory>>) -> Result<Jobs, Error> {
        config.check()?;
        let shared = Arc::new(Shared {
            config,
            requests,
            memory,
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            done: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
        });
        let jobs = Jobs { shared, threads: Mutex::new(Vec::new()) };
        for i in 0..jobs.shared.config.running {
            let shared = jobs.shared.clone();
            let thread = std::thread::Builder::new().name(format!("iwdb-job-{}", i)).spawn(move || work(&shared));
            match thread {
                Ok(thread) => lock(&jobs.threads).push(thread),
                Err(e) => {
                    jobs.close();
                    return Err(Error::internal(format!("can't start a job thread: {}", e)));
                }
            }
        }
        Ok(jobs)
    }

    pub fn config(&self) -> &JobsConfig {
        &self.shared.config
    }

    /// Queue a job of `kind` on `namespace` for `user`, from `client`, that
    /// may run `timeout` (at most the configured timeout; `None`: that).
    /// Answers at once with the queued job. Errors: `unavailable` while
    /// draining, when the queue is full, or when `user` has
    /// [`JobsConfig::per_user`] jobs queued or running.
    pub fn start(
        &self,
        namespace: &str,
        user: &str,
        client: Option<IpAddr>,
        kind: &str,
        timeout: Option<Duration>,
        work: JobWork,
    ) -> Result<JobInfo, Error> {
        let shared = &self.shared;
        let config = &shared.config;
        let mut state = lock(&shared.state);
        sweep(shared, &mut state);
        if state.closed {
            return Err(Error::unavailable(SHUTTING_DOWN));
        }
        if state.queue.len() >= config.queued {
            return Err(Error::unavailable(format!(
                "too many jobs: {} are queued already ([jobs] queued); retry later",
                state.queue.len()
            )));
        }
        let users = state.entries.values().filter(|e| e.info.user == user && !e.info.state.ended()).count();
        if users >= config.per_user {
            return Err(Error::unavailable(format!(
                "user '{}' has {} jobs queued or running, the most a user may ([jobs] per_user); retry once one ends",
                user, users
            )));
        }
        let id = shared.requests.next_id();
        let created = CommitTime::now();
        let token = Token::new();
        let weak: Weak<Shared> = Arc::downgrade(shared);
        let hook = Box::new(move || {
            if let Some(shared) = weak.upgrade() {
                let _ = cancel(&shared, id, None, CANCELLED);
            }
        });
        let registration =
            shared.requests.register(id, Operation::StartJob, Some(namespace), user, client, created, hook);
        let info = JobInfo {
            id,
            namespace: namespace.to_owned(),
            user: user.to_owned(),
            client,
            kind: kind.to_owned(),
            state: JobState::Queued,
            created,
            started: None,
            ended: None,
            elapsed: Duration::ZERO,
            progress: None,
            nodes: None,
            edges: None,
            seq: None,
            rows: None,
            truncated: false,
            result_bytes: 0,
            error: None,
            expires: None,
        };
        let entry = Entry {
            info,
            token,
            progress: Progress::new(),
            timeout: timeout.map_or(config.timeout, |t| t.min(config.timeout)),
            started_at: None,
            ended_at: None,
            work: Some(work),
            registration: Some(registration),
            result: None,
            charge: None,
            span: None,
            queued: None,
        };
        // A trace of its own, linked to the StartJob request that queued it
        let span = iwdb_storage::trace_span!(
            parent: None,
            "iwdb.job",
            otel.status_code = tracing::field::Empty,
            db.namespace = namespace,
            iwdb.request_id = id,
            iwdb.job.kind = kind,
            iwdb.job.nodes = tracing::field::Empty,
            iwdb.job.edges = tracing::field::Empty,
            iwdb.outcome = tracing::field::Empty,
        );
        if !span.is_disabled() {
            span.follows_from(tracing::Span::current());
        }
        let entry =
            Entry { queued: Some(iwdb_storage::trace_span!(parent: &span, "iwdb.queue")), span: Some(span), ..entry };
        let info = entry.info(config.retention);
        state.entries.insert(id, entry);
        state.queue.push_back(id);
        drop(state);
        shared.ready.notify_one();
        Ok(info)
    }

    /// The jobs kept (only `user`'s, if given), newest first, at most
    /// `limit`; and whether there were more.
    pub fn list(&self, user: Option<&str>, limit: usize) -> (Vec<JobInfo>, bool) {
        let shared = &self.shared;
        let mut state = lock(&shared.state);
        sweep(shared, &mut state);
        let mut matching = state.entries.values().rev().filter(|e| user.is_none_or(|u| e.info.user == u));
        let list = matching.by_ref().take(limit).map(|e| e.info(shared.config.retention)).collect();
        (list, matching.next().is_some())
    }

    /// Job `id` (only if it is `user`'s, if given). Errors: `not_found`.
    pub fn get(&self, id: u64, user: Option<&str>) -> Result<JobInfo, Error> {
        let shared = &self.shared;
        let mut state = lock(&shared.state);
        sweep(shared, &mut state);
        Ok(find(&state, id, user)?.info(shared.config.retention))
    }

    /// Cancel job `id` (only if it is `user`'s, if given): it is
    /// `cancelled` at once, and its thread stops at the core's next check
    /// (a queued job never starts). A job that has ended keeps its outcome
    /// and is answered as it is. Errors: `not_found`.
    pub fn cancel(&self, id: u64, user: Option<&str>) -> Result<JobInfo, Error> {
        cancel(&self.shared, id, user, CANCELLED)
    }

    /// Rows `offset..` of done job `id`'s result (only if it is `user`'s,
    /// if given): at most `limit` (default and maximum [`MAX_PAGE_ROWS`]),
    /// stopping before about [`PAGE_BYTES`] after at least one row.
    /// Errors: `not_found` (no such job, or its result expired);
    /// `invalid_argument` while it is queued or running, or for a limit of
    /// 0; the job's own error if it failed; `cancelled` if it was.
    pub fn page(&self, id: u64, user: Option<&str>, offset: u64, limit: Option<usize>) -> Result<JobPage, Error> {
        let limit = match limit {
            Some(0) => return Err(Error::invalid("limit must be at least 1")),
            Some(n) => n.min(MAX_PAGE_ROWS),
            None => MAX_PAGE_ROWS,
        };
        let shared = &self.shared;
        let mut state = lock(&shared.state);
        sweep(shared, &mut state);
        let entry = find(&state, id, user)?;
        let job = entry.info(shared.config.retention);
        let result = match (job.state, &entry.result) {
            (JobState::Done, Some(result)) => result.clone(),
            (JobState::Failed | JobState::Cancelled, _) => {
                return Err(job.error.unwrap_or_else(|| Error::internal("a job ended without its error")));
            }
            (JobState::Expired, _) | (JobState::Done, None) => {
                return Err(Error::new(Code::NotFound, format!("job {}'s result has expired", id)));
            }
            (running, _) => {
                return Err(Error::invalid(format!("job {} is {}: fetch its result once it is done", id, running)));
            }
        };
        drop(state);
        let (rows, next) = slice(&result, offset, limit);
        Ok(JobPage { job, rows, next_offset: next })
    }

    /// Queued, running and kept jobs, stored bytes, and the outcomes so far.
    pub fn counts(&self) -> JobCounts {
        let shared = &self.shared;
        let mut state = lock(&shared.state);
        sweep(shared, &mut state);
        let mut counts = JobCounts {
            result_bytes: state.result_bytes,
            done_total: shared.done.load(Ordering::Relaxed),
            failed_total: shared.failed.load(Ordering::Relaxed),
            cancelled_total: shared.cancelled.load(Ordering::Relaxed),
            ..JobCounts::default()
        };
        for entry in state.entries.values() {
            match entry.info.state {
                JobState::Queued => counts.queued += 1,
                JobState::Collecting | JobState::Running => counts.running += 1,
                _ => counts.finished += 1,
            }
        }
        counts
    }

    /// Start draining: cancel every queued and running job ("the server is
    /// shutting down") and refuse new ones with `unavailable`. The jobs
    /// stay readable.
    pub fn drain(&self) {
        let ids: Vec<u64> = {
            let mut state = lock(&self.shared.state);
            state.closed = true;
            state.entries.values().filter(|e| !e.info.state.ended()).map(|e| e.info.id).collect()
        };
        for id in ids {
            let _ = cancel(&self.shared, id, None, SHUTTING_DOWN);
        }
    }

    /// Take jobs again after [`drain`](Self::drain).
    pub fn resume(&self) {
        lock(&self.shared.state).closed = false;
    }

    /// Drain, stop the threads and join them (they stop at the core's next
    /// check of their jobs' tokens). Idempotent.
    pub fn close(&self) {
        self.drain();
        lock(&self.shared.state).shutdown = true;
        self.shared.ready.notify_all();
        let threads: Vec<JoinHandle<()>> = lock(&self.threads).drain(..).collect();
        for thread in threads {
            // A job's panic is caught; nothing is left to clean up
            let _ = thread.join();
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.close();
    }
}

/// Job `id`, if it is `user`'s (when given). Errors: `not_found`.
fn find<'a>(state: &'a State, id: u64, user: Option<&str>) -> Result<&'a Entry, Error> {
    state
        .entries
        .get(&id)
        .filter(|e| user.is_none_or(|u| e.info.user == u))
        .ok_or_else(|| Error::new(Code::NotFound, format!("no job {}", id)))
}

fn cancel(shared: &Shared, id: u64, user: Option<&str>, message: &str) -> Result<JobInfo, Error> {
    let mut state = lock(&shared.state);
    sweep(shared, &mut state);
    find(&state, id, user)?;
    let State { entries, queue, .. } = &mut *state;
    let Some(entry) = entries.get_mut(&id) else {
        return Err(Error::new(Code::NotFound, format!("no job {}", id)));
    };
    if !entry.info.state.ended() {
        entry.token.cancel();
        queue.retain(|q| *q != id);
        entry.work = None;
        end(entry, JobState::Cancelled, Some(Error::new(Code::Cancelled, message)));
        shared.cancelled.fetch_add(1, Ordering::Relaxed);
    }
    let info = entry.info(shared.config.retention);
    trim(shared, &mut state);
    Ok(info)
}

/// Mark `entry` ended in `outcome`, and take it out of the request list.
fn end(entry: &mut Entry, outcome: JobState, error: Option<Error>) {
    entry.info.progress = JobProgress::of(&entry.progress);
    entry.info.state = outcome;
    entry.info.error = error;
    entry.info.ended = Some(CommitTime::now());
    entry.ended_at = Some(Instant::now());
    entry.registration = None;
    entry.queued = None;
    if let Some(span) = entry.span.take() {
        let code = entry.info.error.as_ref().map(Error::code);
        span.record("iwdb.outcome", code.map_or("ok", Code::as_str));
        if code.is_some() {
            span.record("otel.status_code", "error");
        }
    }
}

/// Remove the jobs that ended more than the retention ago.
fn sweep(shared: &Shared, state: &mut State) {
    let retention = shared.config.retention;
    let State { entries, result_bytes, .. } = state;
    entries.retain(|_, e| {
        let keep = e.ended_at.is_none_or(|t| t.elapsed() < retention);
        if !keep {
            e.drop_result(result_bytes);
        }
        keep
    });
}

/// Remove the oldest ended jobs beyond [`JobsConfig::max_finished`].
fn trim(shared: &Shared, state: &mut State) {
    loop {
        let ended = state.entries.values().filter(|e| e.ended_at.is_some());
        if ended.clone().count() <= shared.config.max_finished {
            return;
        }
        let Some(oldest) = ended.min_by_key(|e| (e.ended_at, e.info.id)).map(|e| e.info.id) else { return };
        if let Some(mut entry) = state.entries.remove(&oldest) {
            entry.drop_result(&mut state.result_bytes);
        }
    }
}

/// Store the outcome of job `id`'s work, unless it ended meanwhile (it
/// was cancelled).
fn finish(shared: &Shared, id: u64, outcome: Result<Finished, Error>) {
    let config = &shared.config;
    let mut state = lock(&shared.state);
    let State { entries, result_bytes, .. } = &mut *state;
    let Some(entry) = entries.get_mut(&id).filter(|e| !e.info.state.ended()) else { return };
    let outcome = outcome.and_then(|f| {
        let bytes = result_bytes_of(&f.result);
        if bytes > config.result_bytes {
            return Err(Error::budget(format!(
                "the result holds about {} bytes, more than jobs may keep ([jobs] result_bytes = {}): ask for fewer max_results",
                bytes, config.result_bytes
            )));
        }
        Ok((f, bytes))
    });
    match outcome {
        Ok((f, bytes)) => {
            entry.info.seq = Some(f.seq);
            entry.info.rows = Some(rows_of(&f.result) as u64);
            entry.info.truncated = f.truncated;
            entry.info.result_bytes = bytes;
            entry.result = Some(Arc::new(f.result));
            end(entry, JobState::Done, None);
            shared.done.fetch_add(1, Ordering::Relaxed);
            // Room for it: the oldest other results go first
            *result_bytes += bytes;
            while *result_bytes > config.result_bytes {
                let oldest = entries
                    .values()
                    .filter(|e| e.info.id != id && e.result.is_some())
                    .min_by_key(|e| (e.ended_at, e.info.id))
                    .map(|e| e.info.id);
                let Some(entry) = oldest.and_then(|o| entries.get_mut(&o)) else { break };
                entry.drop_result(result_bytes);
                entry.info.state = JobState::Expired;
            }
            if let Some(entry) = entries.get_mut(&id) {
                entry.charge = shared.memory.as_ref().map(|m| {
                    let charge = m.charge(Part::Working);
                    charge.set(bytes);
                    charge
                });
            }
        }
        Err(e) => {
            let cancelled = e.code() == Code::Cancelled && entry.token.is_cancelled();
            if cancelled {
                end(entry, JobState::Cancelled, Some(e));
                shared.cancelled.fetch_add(1, Ordering::Relaxed);
            } else {
                end(entry, JobState::Failed, Some(e));
                shared.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    trim(shared, &mut state);
}

/// A job thread: take the oldest queued job, run it, store its outcome.
fn work(shared: &Arc<Shared>) {
    loop {
        let (handle, job, span) = {
            let mut state = lock(&shared.state);
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(id) = state.queue.pop_front() {
                    if let Some(entry) = state.entries.get_mut(&id)
                        && let Some(job) = entry.work.take()
                    {
                        entry.info.state = JobState::Collecting;
                        entry.info.started = Some(CommitTime::now());
                        entry.started_at = Some(Instant::now());
                        entry.queued = None;
                        let span = entry.span.clone().unwrap_or_else(tracing::Span::none);
                        let handle = JobHandle {
                            shared: shared.clone(),
                            id,
                            token: entry.token.clone(),
                            progress: entry.progress.clone(),
                            timeout: entry.timeout,
                        };
                        break (handle, job, span);
                    }
                    continue;
                }
                state = shared.ready.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let outcome = span.in_scope(|| crate::exec::catch(|| job(&handle)));
        drop(span);
        finish(shared, handle.id, outcome);
    }
}

fn rows_of(result: &JobResult) -> usize {
    match result {
        JobResult::Scores(rows) => rows.len(),
        JobResult::Groups(rows) => rows.len(),
        JobResult::Counts(rows) => rows.len(),
    }
}

const STRING: u64 = std::mem::size_of::<String>() as u64;

/// A score's or a count's estimated bytes: the pair, and the id's text.
fn pair_bytes(id: &str) -> u64 {
    STRING + 8 + id.len() as u64
}

/// A group's estimated bytes: its `Vec`, and each id as a `String`.
fn group_bytes(ids: &[String]) -> u64 {
    std::mem::size_of::<Vec<String>>() as u64 + ids.iter().map(|id| STRING + id.len() as u64).sum::<u64>()
}

/// A result's estimated size in memory: the ids' lengths plus a fixed
/// overhead per row and per id. What [`JobsConfig::result_bytes`] counts.
pub fn result_bytes_of(result: &JobResult) -> u64 {
    match result {
        JobResult::Scores(rows) => rows.iter().map(|(id, _)| pair_bytes(id)).sum(),
        JobResult::Counts(rows) => rows.iter().map(|(id, _)| pair_bytes(id)).sum(),
        JobResult::Groups(rows) => rows.iter().map(|g| group_bytes(g)).sum(),
    }
}

/// Rows `offset..` of `result`, at most `limit` and about [`PAGE_BYTES`]
/// (at least one row), and the next offset if more are left.
fn slice(result: &JobResult, offset: u64, limit: usize) -> (JobResult, Option<u64>) {
    fn take<T: Clone>(rows: &[T], offset: u64, limit: usize, bytes: impl Fn(&T) -> u64) -> (Vec<T>, Option<u64>) {
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(rows.len());
        let mut page = Vec::new();
        let mut used = 0u64;
        for row in rows[start..].iter().take(limit) {
            let size = bytes(row);
            if !page.is_empty() && used + size > PAGE_BYTES {
                break;
            }
            used += size;
            page.push(row.clone());
        }
        let end = start + page.len();
        let next = (end < rows.len()).then_some(end as u64);
        (page, next)
    }
    match result {
        JobResult::Scores(rows) => {
            let (page, next) = take(rows, offset, limit, |(id, _)| pair_bytes(id));
            (JobResult::Scores(page), next)
        }
        JobResult::Counts(rows) => {
            let (page, next) = take(rows, offset, limit, |(id, _)| pair_bytes(id));
            (JobResult::Counts(page), next)
        }
        JobResult::Groups(rows) => {
            let (page, next) = take(rows, offset, limit, |g| group_bytes(g));
            (JobResult::Groups(page), next)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn scores(n: usize) -> JobResult {
        JobResult::Scores((0..n).map(|i| (format!("n{:04}", i), i as f64)).collect())
    }

    fn quick(result: JobResult) -> JobWork {
        Box::new(move |_| Ok(Finished { result, truncated: false, seq: 7 }))
    }

    /// Work that runs until its token is cancelled or `release` gets a
    /// message, and reports that it started, and 4 of 10 units done as
    /// the core's algorithms do.
    fn blocking(started: mpsc::Sender<u64>, release: mpsc::Receiver<()>) -> JobWork {
        Box::new(move |h| {
            h.collecting(10, 20);
            h.running();
            ironweaver_core::cancel::run_with_progress(h.token(), h.progress(), || {
                let report = ironweaver_core::cancel::progress();
                report.start("count", Some(10));
                report.add(4);
            })
            .ok();
            let _ = started.send(h.id());
            loop {
                if h.token().is_cancelled() {
                    return Err(Error::new(Code::Cancelled, "stopped"));
                }
                if release.recv_timeout(Duration::from_millis(2)).is_ok() {
                    return Ok(Finished { result: scores(3), truncated: true, seq: 9 });
                }
            }
        })
    }

    fn wait_for(jobs: &Jobs, id: u64, state: JobState) -> JobInfo {
        let start = Instant::now();
        loop {
            let info = jobs.get(id, None).unwrap();
            if info.state == state {
                return info;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "job {} is {:?}, not {:?}", id, info.state, state);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn jobs(config: JobsConfig) -> (Jobs, Arc<Requests>) {
        let requests = Requests::new();
        (Jobs::new(config, requests.clone(), None).unwrap(), requests)
    }

    #[test]
    fn a_job_runs_reports_its_phases_and_keeps_its_result() {
        let (jobs, requests) = jobs(JobsConfig::default());
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let job = jobs.start("social", "ann", None, "page_rank", None, blocking(started_tx, release_rx)).unwrap();
        assert_eq!((job.state, job.kind.as_str(), job.namespace.as_str()), (JobState::Queued, "page_rank", "social"));
        assert_eq!(started.recv().unwrap(), job.id);
        let running = jobs.get(job.id, None).unwrap();
        assert_eq!((running.state, running.nodes, running.edges), (JobState::Running, Some(10), Some(20)));
        assert!(running.started.is_some() && running.ended.is_none());
        let reported = JobProgress { phase: "count".into(), done: 4, total: Some(10) };
        assert_eq!(running.progress.as_ref(), Some(&reported));
        // Listed as a request while it runs, with the job's id
        let listed = requests.list(None, 10).0;
        assert_eq!(listed.iter().map(|r| (r.id, r.operation)).collect::<Vec<_>>(), vec![(job.id, Operation::StartJob)]);
        release.send(()).unwrap();
        let done = wait_for(&jobs, job.id, JobState::Done);
        assert_eq!((done.seq, done.rows, done.truncated), (Some(9), Some(3), true));
        assert!(done.result_bytes > 0 && done.expires.is_some());
        assert_eq!(done.progress, Some(reported), "the last report is kept");
        assert_eq!(requests.active(), 0, "no longer a running request");
        let page = jobs.page(job.id, None, 0, Some(2)).unwrap();
        assert_eq!((page.rows, page.next_offset), (scores(2), Some(2)));
        let rest = jobs.page(job.id, None, 2, None).unwrap();
        assert_eq!(rest.next_offset, None);
        assert_eq!(rest.rows, JobResult::Scores(vec![("n0002".into(), 2.0)]));
        assert_eq!(jobs.page(job.id, Some("bob"), 0, None).unwrap_err().code(), Code::NotFound);
        assert_eq!(jobs.page(job.id, None, 0, Some(0)).unwrap_err().code(), Code::InvalidArgument);
        let counts = jobs.counts();
        assert_eq!((counts.finished, counts.done_total, counts.result_bytes), (1, 1, done.result_bytes));
    }

    #[test]
    fn a_cancel_takes_effect_at_once_and_reaches_the_work() {
        let (jobs, requests) = jobs(JobsConfig { running: 1, ..JobsConfig::default() });
        let (started_tx, started) = mpsc::channel();
        let (_release, release_rx) = mpsc::channel();
        let running = jobs.start("social", "ann", None, "triangles", None, blocking(started_tx, release_rx)).unwrap();
        started.recv().unwrap();
        // Queued behind it: cancelled through the request registry
        let queued = jobs.start("social", "ann", None, "triangles", None, quick(scores(1))).unwrap();
        assert_eq!(requests.cancel(queued.id, Some("ann")).unwrap().id, queued.id);
        let info = jobs.get(queued.id, None).unwrap();
        assert_eq!((info.state, info.error.map(|e| e.code())), (JobState::Cancelled, Some(Code::Cancelled)));
        assert_eq!(jobs.page(queued.id, None, 0, None).unwrap_err().code(), Code::Cancelled);
        assert_eq!(jobs.cancel(running.id, Some("bob")).unwrap_err().code(), Code::NotFound);
        assert_eq!(jobs.page(running.id, None, 0, None).unwrap_err().code(), Code::InvalidArgument);
        let cancelled = jobs.cancel(running.id, Some("ann")).unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
        // Answered as it is from now on
        assert_eq!(jobs.cancel(running.id, None).unwrap().state, JobState::Cancelled);
        // The thread is free again
        let next = jobs.start("social", "ann", None, "triangles", None, quick(scores(1))).unwrap();
        wait_for(&jobs, next.id, JobState::Done);
        assert_eq!(jobs.counts().cancelled_total, 2);
        assert_eq!(requests.active(), 0);
    }

    #[test]
    fn queues_and_users_are_capped() {
        let config = JobsConfig { running: 1, queued: 2, per_user: 2, ..JobsConfig::default() };
        let (jobs, _) = jobs(config);
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        jobs.start("ns", "ann", None, "degree", None, blocking(started_tx, release_rx)).unwrap();
        started.recv().unwrap();
        jobs.start("ns", "ann", None, "degree", None, quick(scores(1))).unwrap();
        // Ann has two (one running, one queued): refused for her alone
        let e = jobs.start("ns", "ann", None, "degree", None, quick(scores(1))).unwrap_err();
        assert!(e.code() == Code::Unavailable && e.message().contains("per_user"), "{}", e);
        jobs.start("ns", "bob", None, "degree", None, quick(scores(1))).unwrap();
        // Two queued: refused for everyone
        let e = jobs.start("ns", "carol", None, "degree", None, quick(scores(1))).unwrap_err();
        assert!(e.code() == Code::Unavailable && e.message().contains("queued"), "{}", e);
        release.send(()).unwrap();
        let (list, more) = jobs.list(None, 10);
        assert!(!more && list.len() == 3 && list.windows(2).all(|w| w[0].id > w[1].id), "newest first");
        assert_eq!(jobs.list(Some("bob"), 10).0.len(), 1);
        assert!(jobs.list(None, 1).1);
    }

    #[test]
    fn results_are_kept_by_time_count_and_bytes() {
        // Count (one thread, so they end in order: the job that ended first
        // goes first)
        let (jobs_, _) = jobs(JobsConfig { max_finished: 2, running: 1, ..JobsConfig::default() });
        let ids: Vec<u64> =
            (0..3).map(|_| jobs_.start("ns", "ann", None, "degree", None, quick(scores(1))).unwrap().id).collect();
        for &id in &ids {
            let start = Instant::now();
            while jobs_.get(id, None).map(|j| !j.state.ended()).unwrap_or(false) {
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(jobs_.get(ids[0], None).unwrap_err().code(), Code::NotFound, "the oldest went first");
        assert_eq!(jobs_.counts().finished, 2);
        drop(jobs_);

        // Time
        let (jobs_, _) = jobs(JobsConfig { retention: Duration::from_millis(50), ..JobsConfig::default() });
        let id = jobs_.start("ns", "ann", None, "degree", None, quick(scores(1))).unwrap().id;
        wait_for(&jobs_, id, JobState::Done);
        assert!(jobs_.page(id, None, 0, None).is_ok());
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(jobs_.page(id, None, 0, None).unwrap_err().code(), Code::NotFound);
        assert_eq!(jobs_.get(id, None).unwrap_err().code(), Code::NotFound);
        drop(jobs_);

        // Bytes: three results fit two at a time; one alone too large fails
        let one = result_bytes_of(&scores(100));
        let memory = Memory::unlimited();
        let config = JobsConfig { result_bytes: one * 2, ..JobsConfig::default() };
        let jobs_ = Jobs::new(config, Requests::new(), Some(memory.clone())).unwrap();
        let ids: Vec<u64> = (0..3)
            .map(|_| {
                let id = jobs_.start("ns", "ann", None, "degree", None, quick(scores(100))).unwrap().id;
                wait_for(&jobs_, id, JobState::Done);
                id
            })
            .collect();
        let expired = jobs_.get(ids[0], None).unwrap();
        assert_eq!((expired.state, expired.result_bytes), (JobState::Expired, 0));
        assert_eq!(jobs_.page(ids[0], None, 0, None).unwrap_err().code(), Code::NotFound);
        assert!(jobs_.page(ids[2], None, 0, None).is_ok());
        assert_eq!(jobs_.counts().result_bytes, one * 2);
        assert_eq!(memory.part(Part::Working), one * 2, "stored results count as working memory");
        let big = jobs_.start("ns", "ann", None, "degree", None, quick(scores(300))).unwrap().id;
        let failed = wait_for(&jobs_, big, JobState::Failed);
        assert_eq!(failed.error.map(|e| e.code()), Some(Code::BudgetExceeded));
        drop(jobs_);
        assert_eq!(memory.part(Part::Working), 0, "and leave it with the registry");
    }

    #[test]
    fn a_page_stops_before_its_byte_budget() {
        let big = "x".repeat(1 << 20);
        let result = JobResult::Groups(vec![vec![big.clone()]; 6]);
        let (page, next) = slice(&result, 0, MAX_PAGE_ROWS);
        assert!(matches!(&page, JobResult::Groups(g) if g.len() == 3), "about 4 MiB");
        assert_eq!(next, Some(3));
        // One row always, however large
        let huge = JobResult::Groups(vec![vec!["y".repeat(5 << 20)], vec![big]]);
        assert_eq!(slice(&huge, 0, MAX_PAGE_ROWS).1, Some(1));
        assert_eq!(slice(&huge, 9, 10), (JobResult::Groups(vec![]), None));
    }

    #[test]
    fn a_drain_cancels_every_job_and_refuses_new_ones() {
        let (jobs, requests) = jobs(JobsConfig { running: 1, ..JobsConfig::default() });
        let (started_tx, started) = mpsc::channel();
        let (_release, release_rx) = mpsc::channel();
        let running = jobs.start("ns", "ann", None, "degree", None, blocking(started_tx, release_rx)).unwrap();
        started.recv().unwrap();
        let queued = jobs.start("ns", "bob", None, "degree", None, quick(scores(1))).unwrap();
        jobs.drain();
        for id in [running.id, queued.id] {
            let info = jobs.get(id, None).unwrap();
            assert_eq!(info.state, JobState::Cancelled);
            assert_eq!(info.error.unwrap().message(), SHUTTING_DOWN);
        }
        let e = jobs.start("ns", "ann", None, "degree", None, quick(scores(1))).unwrap_err();
        assert_eq!((e.code(), e.message()), (Code::Unavailable, SHUTTING_DOWN));
        assert_eq!(requests.active(), 0);
        jobs.close();
        jobs.resume();
    }

    #[test]
    fn a_panicking_job_fails_with_internal() {
        let (jobs, _) = jobs(JobsConfig::default());
        let id = jobs.start("ns", "ann", None, "degree", None, Box::new(|_| panic!("boom"))).unwrap().id;
        let failed = wait_for(&jobs, id, JobState::Failed);
        assert_eq!(failed.error.map(|e| e.code()), Some(Code::Internal));
    }

    #[test]
    fn bounds_are_checked() {
        assert!(JobsConfig::default().check().is_ok());
        assert!(JobsConfig { running: 0, ..JobsConfig::default() }.check().is_err());
        assert!(JobsConfig { timeout: Duration::ZERO, ..JobsConfig::default() }.check().is_err());
        for state in JobState::ALL {
            assert_eq!(JobState::parse(state.as_str()), Some(state));
        }
    }
}
