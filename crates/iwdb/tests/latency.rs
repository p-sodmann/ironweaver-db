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
    let size: u64 =
        std::fs::read_dir(dir.path().join("checkpoints")).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
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
