//! The benchmarks' workload (step 14b), shared by the criterion benches
//! (`benches/graph.rs`) and the load generator (`examples/loadgen.rs`):
//! a deterministic synthetic graph, a store to load it into, and a server
//! of it on an ephemeral port.
//!
//! The graph: nodes `n0` .. `n{N-1}`, label `Person`, attributes `age`
//! (`i % 80`), `group` (`i % 1000`), `x` and `y` (floats); every node has
//! `degree` outgoing `KNOWS` edges to pseudo-random nodes (seeded by the
//! node, so the same `N` and `degree` give the same graph), each with a
//! `w` between 1 and 10. `age` is indexed.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iwdb::{
    AttrPath, Attrs, CatalogChange, CheckpointOptions, Embedded, FsyncPolicy, IndexDef, Mutation, QueryConfig, Store,
    StoreOptions, Value,
};
use iwdb_query::{Bounds, LimitConfig};
use iwdb_server::Server;
use tokio::runtime::Runtime;

/// Mutations per commit while loading.
pub const BATCH: usize = 10_000;

/// A small, fast pseudo-random generator (xorshift64*): the workload
/// doesn't need a cryptographic one, and this keeps it dependency-free.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number below `n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

pub fn id(i: usize) -> String {
    format!("n{}", i)
}

fn attrs(pairs: &[(&str, Value)]) -> Attrs {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}

pub fn node(i: usize) -> Mutation {
    Mutation::UpsertNode {
        id: id(i),
        labels: vec!["Person".to_owned()],
        attr: attrs(&[
            ("age", Value::Int((i % 80) as i64)),
            ("group", Value::Int((i % 1000) as i64)),
            ("x", Value::Float((i % 1000) as f64)),
            ("y", Value::Float((i / 1000) as f64)),
        ]),
        meta: Attrs::new(),
        expected_version: None,
    }
}

/// The outgoing edges of node `i`.
pub fn edges(i: usize, nodes: usize, degree: usize) -> impl Iterator<Item = Mutation> {
    let mut rng = Rng::new(i as u64);
    (0..degree).map(move |_| Mutation::AddEdge {
        from: id(i),
        to: id(rng.below(nodes)),
        ty: Some("KNOWS".to_owned()),
        attr: attrs(&[("w", Value::Int(1 + rng.below(10) as i64))]),
        meta: Attrs::new(),
    })
}

/// A store in `dir` with `fsync`, without background checkpoints (they
/// would land at random points of a measurement).
pub fn open(dir: &Path, fsync: FsyncPolicy) -> Store {
    let options = StoreOptions {
        wal: iwdb::WalOptions { fsync, ..iwdb::WalOptions::default() },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: false, keep: 2, background: false },
        ..StoreOptions::default()
    };
    Store::open(dir, options).unwrap()
}

/// The benchmarks' read limits: high enough for analytics on 10M nodes
/// and 40M edges, and long timeouts.
pub fn query_config() -> QueryConfig {
    let big = Bounds { max_results: 1_000_000, max_visited: 100_000_000, max_edges: 1_000_000_000 };
    let limits = LimitConfig {
        default_limits: LimitConfig::DEFAULT_LIMITS,
        max_limits: big,
        default_timeout: Duration::from_secs(30),
        max_timeout: Duration::from_secs(3600),
    };
    QueryConfig { limits, ..QueryConfig::default() }
}

/// How long the phases of [`load`] took.
pub struct Loaded {
    pub nodes: Duration,
    pub edges: Duration,
    /// The process's resident memory before, after the nodes, after the
    /// edges (if `ps` could tell).
    pub rss: [Option<u64>; 3],
}

/// Load the graph into `store`'s `default` namespace in commits of
/// [`BATCH`] mutations: the index, the nodes, then the edges.
pub fn load(store: &Store, nodes: usize, degree: usize) -> Loaded {
    let ns = store.default_namespace();
    ns.commit_catalog(CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["age".to_owned()]).unwrap() }))
        .unwrap();
    let rss0 = rss_bytes();
    let start = Instant::now();
    let mut batch = Vec::with_capacity(BATCH);
    for i in 0..nodes {
        batch.push(node(i));
        if batch.len() == BATCH {
            ns.commit(&batch).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        ns.commit(&batch).unwrap();
        batch.clear();
    }
    let node_time = start.elapsed();
    let rss1 = rss_bytes();
    let start = Instant::now();
    for i in 0..nodes {
        batch.extend(edges(i, nodes, degree));
        if batch.len() >= BATCH {
            ns.commit(&batch).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        ns.commit(&batch).unwrap();
    }
    let edge_time = start.elapsed();
    Loaded { nodes: node_time, edges: edge_time, rss: [rss0, rss1, rss_bytes()] }
}

/// This process's resident set size, from `ps` (Linux and macOS).
pub fn rss_bytes() -> Option<u64> {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
    let kib: u64 = String::from_utf8(out.ok()?.stdout).ok()?.trim().parse().ok()?;
    Some(kib * 1024)
}

/// A server of `db` on 127.0.0.1 with an ephemeral port, on its own
/// runtime, until the runtime is dropped. Returns the runtime and the
/// endpoint.
pub fn serve(db: Embedded) -> (Runtime, String) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("bench-server").build().unwrap();
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = Server::new(Arc::new(db));
    runtime.spawn(async move {
        let never = std::future::pending::<()>();
        let _ = server.serve(listener, never, || async {}).await;
    });
    (runtime, endpoint)
}
