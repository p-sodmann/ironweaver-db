//! Criterion benchmarks (step 14b): the reads of the `Database` trait on
//! the benchmark graph (`support`), embedded and over gRPC, PageRank, and
//! the latency of one commit per fsync policy.
//!
//! `IWDB_BENCH_NODES` sets the graph's size (default 100 000, degree 4).
//! `cargo bench -p iwdb-server --bench graph`; the load generator
//! (`examples/loadgen.rs`) measures throughput under concurrency and the
//! 1M and 10M sets. Results: `documentation/benchmarks.md`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::cell::RefCell;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ironweaver_core::query::Pattern;
use ironweaver_core::{CmpOp, Expr, Value};
use iwdb::{Embedded, FsyncPolicy};
use iwdb_query::exec::block_on;
use iwdb_query::{
    AnalyticsRequest, Database, FindRequest, Job, MatchRequest, NeighbourhoodRequest, PathRequest, ProjectionSpec,
    QueryOptions,
};
use iwdb_server::client::Remote;

const DEGREE: usize = 4;

fn nodes() -> usize {
    std::env::var("IWDB_BENCH_NODES").ok().and_then(|n| n.parse().ok()).unwrap_or(100_000)
}

/// Every read of the benchmark, on any `Database`.
fn reads<D: Database>(c: &mut Criterion, kind: &str, db: &D, n: usize) {
    let mut group = c.benchmark_group(format!("read/{}", kind));
    let rng = RefCell::new(support::Rng::new(7));
    let pick = || rng.borrow_mut().below(n);
    let options = || QueryOptions { partial: true, ..QueryOptions::default() };
    group.bench_function("get_node", |b| {
        b.iter(|| black_box(block_on(db.get_nodes("default", vec![support::id(pick())], options())).unwrap()))
    });
    group.bench_function("neighbourhood_depth_2", |b| {
        b.iter(|| {
            let request = NeighbourhoodRequest::new([support::id(pick())], 2);
            black_box(block_on(db.neighbourhood("default", request, options())).unwrap())
        })
    });
    group.bench_function("shortest_path", |b| {
        b.iter(|| {
            let request = PathRequest::bfs(support::id(pick()), support::id(pick()));
            black_box(block_on(db.shortest_path("default", request, options())).unwrap())
        })
    });
    group.bench_function("find_indexed_100", |b| {
        b.iter(|| {
            let filter =
                Expr::Compare { path: vec!["age".into()], op: CmpOp::Eq, value: Value::Int((pick() % 80) as i64) };
            let options = options().with_limits(Some(100), None, None);
            black_box(block_on(db.find("default", FindRequest { filter }, options)).unwrap())
        })
    });
    group.bench_function("match_two_hops_100", |b| {
        b.iter(|| {
            let text = format!("(a:Person {{group: {}}})-[:KNOWS]->(b)-[:KNOWS]->(c)", pick() % 1000);
            let request = MatchRequest { pattern: Pattern::parse(&text).unwrap(), filters: Vec::new() };
            let options = options().with_limits(Some(100), None, None);
            black_box(block_on(db.match_pattern("default", request, options)).unwrap())
        })
    });
    group.finish();
}

fn benchmarks(c: &mut Criterion) {
    let n = nodes();
    let dir = tempfile::tempdir().unwrap();
    let store = support::open(dir.path(), FsyncPolicy::Off);
    support::load(&store, n, DEGREE);
    let db = Embedded::new(store, support::query_config()).unwrap();
    reads(c, "embedded", &db, n);

    let mut analytics = c.benchmark_group("analytics");
    analytics.sample_size(10).measurement_time(Duration::from_secs(20));
    analytics.bench_function(BenchmarkId::new("pagerank", n), |b| {
        b.iter(|| {
            let request = AnalyticsRequest {
                projection: ProjectionSpec::default(),
                job: Job::PageRank(ironweaver_core::algo::PageRank::default()),
            };
            let options = QueryOptions::default().with_limits(Some(10), Some(n), Some(n * DEGREE));
            black_box(block_on(db.analyze("default", request, options)).unwrap())
        })
    });
    analytics.finish();

    let (runtime, endpoint) = support::serve(db);
    let remote = Remote::connect(&endpoint).unwrap();
    reads(c, "grpc", &remote, n);
    drop(remote);
    drop(runtime);

    commits(c);
}

/// One single-node commit, per fsync policy (embedded, one writer).
fn commits(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit");
    let policies = [
        ("off", FsyncPolicy::Off),
        ("always", FsyncPolicy::Always),
        ("group", FsyncPolicy::Group { max_delay: Duration::from_millis(1), max_batch: 64 }),
    ];
    for (name, policy) in policies {
        let dir = tempfile::tempdir().unwrap();
        let store = support::open(dir.path(), policy);
        let mut i = 0;
        group.bench_function(name, |b| {
            b.iter(|| {
                i += 1;
                black_box(store.default_namespace().commit(&[support::node(i)]).unwrap())
            })
        });
        store.close().unwrap();
    }
    group.finish();
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
