//! "Writes keep flowing while a checkpoint runs (measured)": commit
//! latency outside and during a large checkpoint. Timing depends on the
//! machine, so this is not part of the normal test suite and asserts
//! nothing about time. Run it with
//!
//! ```text
//! cargo test --release -p iwdb --test latency -- --ignored --nocapture
//! ```
//!
//! and record the numbers in `documentation/steps/step_5.md`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use iwdb::{Attrs, CheckpointOptions, FsyncPolicy, Mutation, Store, StoreOptions, Value, WalOptions};

const NODES: usize = 500_000;

fn options(fsync: FsyncPolicy) -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync, ..WalOptions::default() },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: true, keep: 2, background: false },
        create_if_missing: true,
        archive: None,
        retention: Default::default(),
        memory: Default::default(),
        backup: Default::default(),
    }
}

fn node(id: String, i: usize) -> Mutation {
    let attr: Attrs = [
        ("name".to_owned(), Value::from(format!("node number {}", i))),
        ("n".to_owned(), Value::Int(i as i64)),
        ("tags".to_owned(), Value::List(vec![Value::from("a"), Value::from("b"), Value::Int((i % 7) as i64)])),
    ]
    .into();
    Mutation::UpsertNode { id, labels: vec!["L".into()], attr, meta: Attrs::new(), expected_version: None }
}

fn edge(i: usize) -> Mutation {
    Mutation::AddEdge {
        from: format!("n{}", i),
        to: format!("n{}", (i * 7919 + 1) % NODES),
        ty: Some("T".into()),
        attr: Attrs::new(),
        meta: Attrs::new(),
    }
}

/// p50, p99, p99.9, max in microseconds.
fn summary(mut samples: Vec<Duration>) -> String {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize].as_micros();
    format!(
        "{:>6} commits  p50 {:>6} us  p99 {:>6} us  p99.9 {:>6} us  max {:>6} us",
        samples.len(),
        at(0.5),
        at(0.99),
        at(0.999),
        samples.last().unwrap().as_micros()
    )
}

fn commit_one(store: &Store, i: usize) -> Duration {
    let start = Instant::now();
    store.commit(&[node(format!("w{}", i % 1000), i)]).unwrap();
    start.elapsed()
}

#[test]
#[ignore = "measurement; run with --release --ignored --nocapture"]
fn commit_latency_during_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    // Load quickly: big transactions, no fsync
    let store = Store::open(dir.path(), options(FsyncPolicy::Off)).unwrap();
    for chunk in (0..NODES).collect::<Vec<_>>().chunks(5000) {
        store.commit(&chunk.iter().map(|&i| node(format!("n{}", i), i)).collect::<Vec<_>>()).unwrap();
    }
    for chunk in (0..NODES).collect::<Vec<_>>().chunks(5000) {
        store.commit(&chunk.iter().map(|&i| edge(i)).collect::<Vec<_>>()).unwrap();
    }
    store.close().unwrap();
    let size: u64 = std::fs::read_dir(dir.path().join("ns/00000000000000000001/checkpoints"))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    println!("graph: {} nodes, {} edges, checkpoint {} MiB", NODES, NODES, size >> 20);

    for (name, fsync) in [
        ("always", FsyncPolicy::Always),
        ("group 10ms", FsyncPolicy::Group { max_delay: Duration::from_millis(10), max_batch: 1000 }),
    ] {
        let store = Store::open(dir.path(), options(fsync)).unwrap();
        let mut i = 0;
        let baseline: Vec<Duration> = (0..2000)
            .map(|_| {
                i += 1;
                commit_one(&store, i)
            })
            .collect();

        // The checkpointer's first run loads the last checkpoint (streaming),
        // replays the WAL and saves a new one: the most work a run does
        let mut during = Vec::new();
        let started = Instant::now();
        let checkpoint_time = std::thread::scope(|scope| {
            let checkpoint = scope.spawn(|| {
                let start = Instant::now();
                store.checkpoint().unwrap();
                start.elapsed()
            });
            while !checkpoint.is_finished() {
                i += 1;
                during.push(commit_one(&store, i));
            }
            checkpoint.join().unwrap()
        });
        assert!(started.elapsed() >= checkpoint_time);
        println!("[{}] checkpoint took {} ms", name, checkpoint_time.as_millis());
        println!("[{}] outside a checkpoint: {}", name, summary(baseline));
        println!("[{}] during the checkpoint: {}", name, summary(during));
        store.close().unwrap();
    }
}

/// Writers use their own label so a constraint on `L` isn't violated by them.
fn commit_writer(store: &Store, i: usize) -> Duration {
    let mut m = node(format!("w{}", i % 1000), i);
    if let Mutation::UpsertNode { labels, .. } = &mut m {
        *labels = vec!["W".into()];
    }
    let start = Instant::now();
    store.commit(&[m]).unwrap();
    start.elapsed()
}

/// Commit and read latency while an index is built online (ADR 0019), and
/// while a unique constraint is validated (which holds the writer mutex
/// for the whole scan).
#[test]
#[ignore = "measurement; run with --release --ignored --nocapture"]
fn latency_during_an_online_index_build() {
    use iwdb::{AttrPath, CatalogChange, Constraint, ConstraintKind, IndexDef, Label};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(FsyncPolicy::Off)).unwrap();
    for chunk in (0..NODES).collect::<Vec<_>>().chunks(5000) {
        store.commit(&chunk.iter().map(|&i| node(format!("n{}", i), i)).collect::<Vec<_>>()).unwrap();
    }
    println!("graph: {} nodes", NODES);
    // For comparison: the same build inside one lock hold, on the engine alone
    let mut plain = iwdb_engine::Namespace::new(iwdb::NamespaceName::new("plain").unwrap());
    for chunk in (0..NODES).collect::<Vec<_>>().chunks(5000) {
        plain.commit(&chunk.iter().map(|&i| node(format!("n{}", i), i)).collect::<Vec<_>>()).unwrap();
    }
    let start = Instant::now();
    plain.commit_catalog(CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["n"]).unwrap() })).unwrap();
    println!("the whole build inside one lock hold: {} ms", start.elapsed().as_millis());
    drop(plain);
    let path = |p: &str| AttrPath::new([p]).unwrap();
    let changes = [
        ("create index", CatalogChange::CreateIndex(IndexDef { path: path("n") })),
        (
            "add unique constraint",
            CatalogChange::AddConstraint(Constraint {
                kind: ConstraintKind::Unique,
                label: Label::new("L").unwrap(),
                path: path("name"),
            }),
        ),
    ];
    let mut i = 0;
    let baseline: Vec<Duration> = (0..2000)
        .map(|_| {
            i += 1;
            commit_writer(&store, i)
        })
        .collect();
    println!("outside a build: {}", summary(baseline));
    for (name, change) in changes {
        let mut commits = Vec::new();
        let mut reads = Vec::new();
        let took = std::thread::scope(|scope| {
            let build = scope.spawn(|| {
                let start = Instant::now();
                store.commit_catalog(change).unwrap();
                start.elapsed()
            });
            while !build.is_finished() {
                i += 1;
                commits.push(commit_writer(&store, i));
                let start = Instant::now();
                store.read(|n| n.graph().node_count());
                reads.push(start.elapsed());
            }
            build.join().unwrap()
        });
        println!("[{}] took {} ms", name, took.as_millis());
        println!("[{}] commits during: {}", name, summary(commits));
        println!("[{}] reads during:   {}", name, summary(reads));
    }
    store.close().unwrap();
}
