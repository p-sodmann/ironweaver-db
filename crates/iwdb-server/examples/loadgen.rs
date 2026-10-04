//! The load generator (step 14b): loads the benchmark graph (`benches/
//! support`), then measures a server of it under concurrent gRPC clients.
//! It prints a report, writes the metrics as JSON (`--json`), and checks
//! them against targets (`--check`, the CI gate on the 100k set).
//!
//! ```text
//! cargo run --release -p iwdb-server --example loadgen -- \
//!     --nodes 1000000 [--degree 4] [--clients 8] [--seconds 5] \
//!     [--dir <empty dir>] [--json <file>] [--check <targets.json>] [--skip-import]
//! ```
//!
//! Phases and metrics:
//! - `load.*`: the graph through commits of 10 000 mutations (fsync
//!   `group`): nodes and edges per second; `memory.*`: the graph's own
//!   estimate per node and edge (`NamespaceStatus::memory_bytes`, indexes
//!   included, payloads not) and the process's resident memory per node and
//!   per edge (payloads, WAL buffers and allocator slack included).
//! - `import.*`: the graph exported to a core binary file and imported as a
//!   new namespace (one checkpoint, ADR 0033).
//! - `read.<op>.*`: `--clients` threads calling `<op>` over gRPC for
//!   `--seconds`: operations per second, and the median and 99th
//!   percentile latency in milliseconds.
//! - `analytics.pagerank_s`: one PageRank over gRPC.
//! - `commit.<policy>.*`: `--clients` threads committing one node each per
//!   call over gRPC, per fsync policy, on a new store.
//!
//! Targets are a JSON object `{"metric": {"min": x} | {"max": x}}`;
//! every miss is printed and the exit code is 1.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../benches/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ironweaver_core::query::Pattern;
use ironweaver_core::{CmpOp, Expr, Value};
use iwdb::import::ExportFormat;
use iwdb::{Embedded, FsyncPolicy};
use iwdb_query::exec::block_on;
use iwdb_query::{
    AnalyticsRequest, Database, FindRequest, Job, MatchRequest, NeighbourhoodRequest, PathRequest, ProjectionSpec,
    QueryOptions,
};
use iwdb_server::client::Remote;

struct Args {
    nodes: usize,
    degree: usize,
    clients: usize,
    seconds: f64,
    dir: Option<PathBuf>,
    json: Option<PathBuf>,
    check: Option<PathBuf>,
    import: bool,
}

const USAGE: &str = "usage: loadgen [--nodes N] [--degree D] [--clients C] [--seconds S] [--dir DIR] [--json FILE] [--check TARGETS] [--skip-import]";

fn args() -> Result<Args, String> {
    let mut a =
        Args { nodes: 100_000, degree: 4, clients: 8, seconds: 5.0, dir: None, json: None, check: None, import: true };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{} needs a value\n{}", flag, USAGE));
        let number = |v: String| v.replace('_', "").parse::<usize>().map_err(|_| format!("{}: not a number", v));
        match flag.as_str() {
            "--nodes" => a.nodes = number(value()?)?,
            "--degree" => a.degree = number(value()?)?,
            "--clients" => a.clients = number(value()?)?.max(1),
            "--seconds" => a.seconds = value()?.parse().map_err(|_| "--seconds: not a number".to_owned())?,
            "--dir" => a.dir = Some(value()?.into()),
            "--json" => a.json = Some(value()?.into()),
            "--check" => a.check = Some(value()?.into()),
            "--skip-import" => a.import = false,
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(a)
}

type Metrics = BTreeMap<String, f64>;

/// Run `op` on `clients` threads for `seconds`; record operations per
/// second and the p50 / p99 latency (ms) under `name`.
fn measure(metrics: &mut Metrics, name: &str, clients: usize, seconds: f64, op: impl Fn(u64) + Sync) {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let counter = AtomicU64::new(0);
    let start = Instant::now();
    let mut latencies: Vec<u32> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..clients)
            .map(|_| {
                s.spawn(|| {
                    let mut own = Vec::new();
                    while Instant::now() < deadline {
                        let i = counter.fetch_add(1, Ordering::Relaxed);
                        let t = Instant::now();
                        op(i);
                        own.push(t.elapsed().as_micros().min(u32::MAX as u128) as u32);
                    }
                    own
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let elapsed = start.elapsed().as_secs_f64();
    latencies.sort_unstable();
    let pct =
        |p: f64| latencies.get(((latencies.len() as f64 - 1.0) * p) as usize).map_or(0.0, |&us| us as f64 / 1000.0);
    metrics.insert(format!("{}.ops_per_s", name), latencies.len() as f64 / elapsed);
    metrics.insert(format!("{}.p50_ms", name), pct(0.50));
    metrics.insert(format!("{}.p99_ms", name), pct(0.99));
    eprintln!(
        "  {:<28} {:>10.0} ops/s   p50 {:>8.3} ms   p99 {:>8.3} ms",
        name,
        latencies.len() as f64 / elapsed,
        pct(0.50),
        pct(0.99)
    );
}

/// A node index for operation `i` (pseudo-random, the same per `i`).
fn pick(i: u64, n: usize, salt: u64) -> usize {
    support::Rng::new(i.wrapping_mul(31).wrapping_add(salt)).below(n)
}

fn run(a: &Args) -> Metrics {
    let mut m = Metrics::new();
    let (n, degree, clients, secs) = (a.nodes, a.degree, a.clients, a.seconds);
    let temp = tempfile::tempdir().unwrap();
    let dir = a.dir.clone().unwrap_or_else(|| temp.path().to_path_buf());
    eprintln!(
        "loadgen: {} nodes, {} edges, {} clients, {} s per measurement, in {}",
        n,
        n * degree,
        clients,
        secs,
        dir.display()
    );

    // ---- load ----
    let group = FsyncPolicy::Group { max_delay: Duration::from_millis(10), max_batch: 64 };
    let store = support::open(&dir.join("graph"), group);
    let loaded = support::load(&store, n, degree);
    let edges = (n * degree) as f64;
    m.insert("load.nodes_per_s".into(), n as f64 / loaded.nodes.as_secs_f64());
    m.insert("load.edges_per_s".into(), edges / loaded.edges.as_secs_f64());
    m.insert("load.total_s".into(), (loaded.nodes + loaded.edges).as_secs_f64());
    let status = store.default_namespace().status();
    m.insert("memory.graph_bytes_per_entity".into(), status.memory_bytes as f64 / (n as f64 + edges));
    if let [Some(r0), Some(r1), Some(r2)] = loaded.rss {
        m.insert("memory.rss_bytes_per_node".into(), r1.saturating_sub(r0) as f64 / n as f64);
        m.insert("memory.rss_bytes_per_edge".into(), r2.saturating_sub(r1) as f64 / edges);
    }
    eprintln!(
        "load: nodes {:.0}/s, edges {:.0}/s, {:.1} s; graph memory {} MiB",
        m["load.nodes_per_s"],
        m["load.edges_per_s"],
        m["load.total_s"],
        status.memory_bytes >> 20
    );

    // ---- export and import ----
    if a.import {
        let file = dir.join("graph.bin");
        let report = store.default_namespace().export_file(&file, Some(ExportFormat::Binary), None).unwrap();
        let start = Instant::now();
        store.import_file("imported", &file, None, None).unwrap();
        let s = start.elapsed().as_secs_f64();
        m.insert("import.s".into(), s);
        m.insert("import.entities_per_s".into(), (n as f64 + edges) / s);
        eprintln!(
            "import: {:.1} s for {} MiB ({:.0} nodes and edges/s)",
            s,
            report.bytes >> 20,
            (n as f64 + edges) / s
        );
        store.drop_namespace("imported", None).unwrap();
        std::fs::remove_file(&file).unwrap();
    }

    // ---- reads over gRPC ----
    let db = Embedded::new(store, support::query_config()).unwrap();
    let (runtime, endpoint) = support::serve(db);
    let remote = Remote::connect(&endpoint).unwrap();
    let r = &remote;
    let partial = || QueryOptions { partial: true, ..QueryOptions::default() };
    eprintln!("reads ({} clients over gRPC):", clients);
    measure(&mut m, "read.get_node", clients, secs, |i| {
        block_on(r.get_nodes("default", vec![support::id(pick(i, n, 1))], partial())).unwrap();
    });
    measure(&mut m, "read.neighbourhood_depth_2", clients, secs, |i| {
        let request = NeighbourhoodRequest::new([support::id(pick(i, n, 2))], 2);
        block_on(r.neighbourhood("default", request, partial())).unwrap();
    });
    measure(&mut m, "read.shortest_path", clients, secs, |i| {
        let request = PathRequest::bfs(support::id(pick(i, n, 3)), support::id(pick(i, n, 4)));
        block_on(r.shortest_path("default", request, partial())).unwrap();
    });
    measure(&mut m, "read.find_indexed_100", clients, secs, |i| {
        let filter = Expr::Compare { path: vec!["age".into()], op: CmpOp::Eq, value: Value::Int((i % 80) as i64) };
        block_on(r.find("default", FindRequest { filter }, partial().with_limits(Some(100), None, None))).unwrap();
    });
    // Bound ids can't be written in the pattern text: this one crosses the
    // wire as postcard (ADR 0023)
    measure(&mut m, "read.match_two_hops_from_id_100", clients, secs, |i| {
        let start = support::id(pick(i, n, 5));
        let mut pattern = Pattern::parse("(a)-[:KNOWS]->(b)-[:KNOWS]->(c)").unwrap();
        pattern.nodes[0].ids = Some(vec![start]);
        let request = MatchRequest { pattern, filters: Vec::new() };
        block_on(r.match_pattern("default", request, partial().with_limits(Some(100), None, None))).unwrap();
    });
    let start = Instant::now();
    let request = AnalyticsRequest {
        projection: ProjectionSpec::default(),
        job: Job::PageRank(ironweaver_core::algo::PageRank::default()),
    };
    let options = QueryOptions { timeout: Some(Duration::from_secs(3600)), ..QueryOptions::default() }.with_limits(
        Some(10),
        Some(n),
        Some(n * degree),
    );
    block_on(r.analyze("default", request, options)).unwrap();
    m.insert("analytics.pagerank_s".into(), start.elapsed().as_secs_f64());
    eprintln!("pagerank: {:.2} s", m["analytics.pagerank_s"]);
    drop(remote);
    drop(runtime);

    // ---- commits over gRPC, per fsync policy ----
    eprintln!("commits ({} clients over gRPC, one node per commit):", clients);
    for (name, policy) in [("always", FsyncPolicy::Always), ("group", group), ("off", FsyncPolicy::Off)] {
        let store = support::open(&dir.join(format!("commits-{}", name)), policy);
        let db = Embedded::new(store, support::query_config()).unwrap();
        let (runtime, endpoint) = support::serve(db);
        let remote = Remote::connect(&endpoint).unwrap();
        let r = &remote;
        measure(&mut m, &format!("commit.{}", name), clients, secs, |i| {
            block_on(r.commit("default", vec![support::node(i as usize)], Default::default())).unwrap();
        });
        drop(remote);
        drop(runtime);
    }
    m
}

/// The misses of `metrics` against the targets in `path`.
fn check(metrics: &Metrics, path: &PathBuf) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {}", path.display(), e));
    let targets: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).unwrap();
    let mut misses = Vec::new();
    for (name, target) in targets {
        if name.starts_with('_') {
            continue; // comments
        }
        let Some(&value) = metrics.get(&name) else {
            misses.push(format!("{}: not measured", name));
            continue;
        };
        if let Some(min) = target.get("min").and_then(|v| v.as_f64()).filter(|min| value < *min) {
            misses.push(format!("{}: {:.3} is below the target of at least {}", name, value, min));
        }
        if let Some(max) = target.get("max").and_then(|v| v.as_f64()).filter(|max| value > *max) {
            misses.push(format!("{}: {:.3} is above the target of at most {}", name, value, max));
        }
    }
    misses
}

fn main() -> ExitCode {
    let a = match args() {
        Ok(a) => a,
        Err(message) => {
            eprintln!("{}", message);
            return ExitCode::from(2);
        }
    };
    let metrics = run(&a);
    let json = serde_json::to_string_pretty(&metrics).unwrap();
    println!("{}", json);
    if let Some(path) = &a.json {
        std::fs::write(path, &json).unwrap();
    }
    if let Some(path) = &a.check {
        let misses = check(&metrics, path);
        for miss in &misses {
            eprintln!("loadgen: target missed: {}", miss);
        }
        if !misses.is_empty() {
            return ExitCode::from(1);
        }
        eprintln!("loadgen: every target in {} met", path.display());
    }
    ExitCode::SUCCESS
}
