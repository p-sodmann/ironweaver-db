//! Managed analytics jobs over the wire (step 16f, ADR 0056).
//!
//! The step's acceptance criterion, through the gRPC and the REST client: a
//! job longer than the server's maximum request timeout runs to the end,
//! reports progress, can be cancelled, and its result can be fetched until
//! it expires. Also the caps, and what a drain does to jobs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::{Duration, Instant};

use ironweaver_core::algo::PageRank;
use iwdb::{Embedded, QueryConfig, Store};
use iwdb_engine::Mutation;
use iwdb_query::exec::block_on;
use iwdb_query::jobs::{JobsConfig, SHUTTING_DOWN};
use iwdb_query::{
    Admin, AnalyticsRequest, Code, Database, Job, JobInfo, JobResult, JobState, LimitConfig, ProjectionSpec,
    QueryOptions,
};
use support::{Running, options};

const NS: &str = "default";

/// The server's maximum request timeout: far shorter than the job.
const MAX_TIMEOUT: Duration = Duration::from_millis(50);

/// How long an ended job is kept.
const RETENTION: Duration = Duration::from_secs(2);

fn node(id: String) -> Mutation {
    Mutation::UpsertNode {
        id,
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    }
}

fn edge(from: usize, to: usize) -> Mutation {
    Mutation::AddEdge {
        from: format!("n{}", from),
        to: format!("n{}", to),
        ty: None,
        attr: Default::default(),
        meta: Default::default(),
    }
}

/// A server whose requests may take at most [`MAX_TIMEOUT`], with jobs
/// kept for [`RETENTION`], on a ring of 2000 nodes with a chord each.
fn server(jobs: JobsConfig) -> (Running<Embedded>, tempfile::TempDir) {
    const N: usize = 2000;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    let mut m: Vec<Mutation> = (0..N).map(|i| node(format!("n{}", i))).collect();
    m.extend((0..N).flat_map(|i| [edge(i, (i + 1) % N), edge(i, (i * 7 + 3) % N)]));
    store.commit(&m).unwrap();
    let limits = LimitConfig { default_timeout: MAX_TIMEOUT, max_timeout: MAX_TIMEOUT, ..LimitConfig::default() };
    let config = QueryConfig { limits, jobs: JobsConfig { retention: RETENTION, ..jobs }, ..QueryConfig::default() };
    (Running::start(Embedded::new(store, config).unwrap()), dir)
}

/// PageRank for exactly `iterations` iterations.
fn pagerank(iterations: usize) -> AnalyticsRequest {
    let options = PageRank { tol: 0.0, max_iter: iterations, ..PageRank::default() };
    AnalyticsRequest { projection: ProjectionSpec::default(), job: Job::PageRank(options) }
}

/// Wide enough for the ring.
fn limits() -> QueryOptions {
    QueryOptions::default().with_limits(Some(10), Some(10_000), Some(10_000))
}

/// Poll job `id` until it has ended, recording every state seen.
fn until_ended<D: Admin>(db: &D, id: u64) -> (JobInfo, Vec<JobInfo>) {
    let start = Instant::now();
    let mut seen = Vec::new();
    loop {
        let job = block_on(db.job(id, None)).unwrap();
        if job.state.ended() {
            return (job, seen);
        }
        seen.push(job);
        assert!(start.elapsed() < Duration::from_secs(60), "job {} never ended", id);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn a_job_outlives_the_request_timeout<D: Database + Admin>(db: &D) {
    // As a request, it times out
    let e = block_on(db.analyze(NS, pagerank(20_000), limits())).unwrap_err();
    assert_eq!(e.code(), Code::Timeout, "{}", e);

    // As a job, it runs to the end, reporting its phase on the way
    let job = block_on(db.start_job(NS.into(), pagerank(20_000), limits(), None)).unwrap();
    let (done, seen) = until_ended(db, job.id);
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert!(done.elapsed > MAX_TIMEOUT, "the job took {:?}: not longer than a request may", done.elapsed);
    let running = seen.iter().find(|j| j.state == JobState::Running).expect("seen running");
    assert_eq!((running.nodes, running.edges), (Some(2000), Some(4000)));
    assert!(running.elapsed > Duration::ZERO && running.ended.is_none());
    assert_eq!((done.rows, done.truncated), (Some(10), true));

    // Its result can be fetched, in pages, until it expires
    let page = block_on(db.job_result(job.id, None, 0, Some(4))).unwrap();
    assert_eq!(page.next_offset, Some(4));
    let JobResult::Scores(top) = page.rows else { panic!("scores") };
    assert!(top.len() == 4 && top.windows(2).all(|w| w[0].1 >= w[1].1), "{:?}", top);
    let expires = done.expires.expect("an expiry").to_system_time();
    std::thread::sleep(expires.duration_since(std::time::SystemTime::now()).unwrap_or_default());
    std::thread::sleep(Duration::from_millis(50));
    let e = block_on(db.job_result(job.id, None, 0, None)).unwrap_err();
    assert_eq!(e.code(), Code::NotFound, "{}", e);
    assert_eq!(block_on(db.job(job.id, None)).unwrap_err().code(), Code::NotFound);

    // And a long job can be cancelled while it runs
    let long = block_on(db.start_job(NS.into(), pagerank(1 << 40), limits(), None)).unwrap();
    let start = Instant::now();
    while block_on(db.job(long.id, None)).unwrap().state != JobState::Running {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(MAX_TIMEOUT * 2);
    let cancelled = block_on(db.cancel_job(long.id, None)).unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert!(cancelled.elapsed > MAX_TIMEOUT, "{:?}", cancelled.elapsed);
    // Its thread is free again: a new job runs
    let next = block_on(db.start_job(NS.into(), pagerank(10), limits(), None)).unwrap();
    assert_eq!(until_ended(db, next.id).0.state, JobState::Done);
}

#[test]
fn a_job_outlives_the_request_timeout_over_grpc() {
    let (server, _dir) = server(JobsConfig { running: 1, ..JobsConfig::default() });
    a_job_outlives_the_request_timeout(&server.client());
}

#[cfg(feature = "rest")]
#[test]
fn a_job_outlives_the_request_timeout_over_rest() {
    let (server, _dir) = server(JobsConfig { running: 1, ..JobsConfig::default() });
    a_job_outlives_the_request_timeout(&server.rest_client());
}

#[test]
fn too_many_jobs_are_refused_with_unavailable() {
    let (server, _dir) = server(JobsConfig { running: 1, queued: 2, per_user: 2, ..JobsConfig::default() });
    let db = server.client();
    let start = |iterations| block_on(db.start_job(NS.into(), pagerank(iterations), limits(), None));
    let first = start(1 << 40).unwrap();
    let second = start(1 << 40).unwrap();
    // The caller has two: refused, whatever the queue
    let e = start(10).unwrap_err();
    assert!(e.code() == Code::Unavailable && e.message().contains("per_user"), "{}", e);
    block_on(db.cancel_job(first.id, None)).unwrap();
    block_on(db.cancel_job(second.id, None)).unwrap();
    // A job that is too large for the limits is refused at once
    let small = QueryOptions::default().with_limits(None, Some(10), None);
    let e = block_on(db.start_job(NS.into(), pagerank(10), small, None)).unwrap_err();
    assert_eq!(e.code(), Code::BudgetExceeded, "{}", e);
}

/// A drain cancels the running jobs and refuses new ones, the jobs stay
/// readable while it lasts, and it still ends in time.
#[test]
fn a_drain_cancels_jobs_and_still_ends_in_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    store.commit(&(0..100).map(|i| node(format!("n{}", i))).collect::<Vec<_>>()).unwrap();
    let db = Embedded::new(store, QueryConfig::default()).unwrap();
    let server = Running::start_built(db, |s| s.unready_delay(Duration::from_millis(500)));
    let client = server.client();
    let job = block_on(client.start_job(NS.into(), pagerank(1 << 40), QueryOptions::default(), None)).unwrap();
    while block_on(client.job(job.id, None)).unwrap().state != JobState::Running {
        std::thread::sleep(Duration::from_millis(2));
    }
    let start = Instant::now();
    std::thread::scope(|s| {
        let shutdown = s.spawn(|| server.shutdown(Duration::from_millis(200)));
        // During the unready delay: cancelled, readable, no new jobs
        let seen = loop {
            let seen = block_on(client.job(job.id, None)).unwrap();
            if seen.state.ended() {
                break seen;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "never cancelled");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(seen.state, JobState::Cancelled);
        assert_eq!(seen.error.as_ref().map(|e| e.message()), Some(SHUTTING_DOWN));
        let e = block_on(client.start_job(NS.into(), pagerank(10), QueryOptions::default(), None)).unwrap_err();
        assert_eq!(e.code(), Code::Unavailable, "{}", e);
        let (report, db) = shutdown.join().unwrap();
        assert!(report.complete, "no call was left running: {:?}", report);
        // Closing joins the job threads, which have stopped
        db.close().unwrap();
    });
    assert!(start.elapsed() < Duration::from_secs(10), "shutdown took {:?}", start.elapsed());
}
