//! Step 8: concurrent readers (ADR 0014), read-your-writes and deadlines
//! (ADR 0016).
//!
//! - Many reader threads against a committing writer: every read sees the
//!   reference state at the seq it reports, never part of a transaction.
//! - Lock hold times and read latency while commits run and during a long
//!   analytics job (which holds no lock while it runs). The numbers are
//!   printed (`cargo test -p iwdb --test concurrency -- --nocapture`); the
//!   assertions are loose bounds, so that a slow CI machine passes.
//! - `min_seq`: returns as soon as the seq is applied, times out, refuses
//!   another history, fails at once on a read-only store.
//! - Analytics: cancelled at the deadline (`Timeout`) or by the caller
//!   (`Cancelled`); the store stays usable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use common::{Action, Call, Rule, TestFs, When};
use ironweaver_core::algo::centrality::{pagerank, PageRank};
use iwdb::{
    CancelToken, Direction, EdgeCost, Error, FsyncPolicy, HistoryId, Mutation, ProjectionSpec, ReadOptions, Store,
    StoreOptions, Value, WalOptions,
};
use support::{options, reference, state, workload, State, Step};

fn fast_options() -> StoreOptions {
    StoreOptions { wal: WalOptions { fsync: FsyncPolicy::Off, ..options(2).wal }, ..options(2) }
}

/// Percentile `p` (0..=100) of `samples`.
fn percentile(samples: &mut [Duration], p: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * p / 100]
}

#[test]
fn readers_see_the_state_at_some_seq_never_part_of_a_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), fast_options()).unwrap());
    let steps = workload(400, 42);
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..6)
        .map(|_| {
            let (store, done) = (store.clone(), done.clone());
            thread::spawn(move || {
                let mut seen: Vec<State> = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    // The state and its seq from one read lock
                    seen.push(store.read(state));
                }
                seen
            })
        })
        .collect();

    // The writer, recording the reference state after each commit
    let mut reference = reference();
    let mut expected: HashMap<u64, State> = HashMap::from([(0, state(&reference))]);
    for step in &steps {
        let (outcome, prepared) = match step {
            Step::Tx(m) => (store.commit(m), reference.prepare(m)),
            Step::Catalog(c) => (store.commit_catalog(c.clone()), reference.prepare_catalog(c.clone())),
        };
        match (outcome, prepared) {
            (Ok(result), Ok(prepared)) => {
                reference.apply(prepared, result.time).unwrap();
                expected.insert(reference.seq(), state(&reference));
            }
            (Err(Error::Engine(_)), Err(_)) => {}
            (a, b) => panic!("store {:?}, reference {:?}", a, b.map(|p| p.result().clone())),
        }
    }
    done.store(true, Ordering::Relaxed);

    let mut reads = 0;
    let mut seqs = std::collections::BTreeSet::new();
    for reader in readers {
        for seen in reader.join().unwrap() {
            let seq = seen.2;
            assert_eq!(Some(&seen), expected.get(&seq), "a read at seq {} differs from the state at that seq", seq);
            reads += 1;
            seqs.insert(seq);
        }
    }
    let stats = store.lock_stats();
    println!(
        "{} reads at {} different seqs of {} commits; write lock held {} times, {:?} in total, at most {:?}",
        reads,
        seqs.len(),
        reference.seq(),
        stats.writes,
        stats.write_hold_total,
        stats.write_hold_max
    );
    assert!(seqs.len() > 10, "the readers ran concurrently with the writer");
    assert_eq!(stats.writes, reference.seq());
}

/// A graph of `n` nodes in a ring with chords, committed in batches.
fn big_graph(store: &Store, n: usize) {
    for chunk in (0..n).collect::<Vec<_>>().chunks(2_000) {
        let mut m: Vec<Mutation> = chunk
            .iter()
            .map(|i| Mutation::UpsertNode {
                id: format!("n{}", i),
                labels: vec![],
                attr: [("i".to_owned(), Value::Int(*i as i64))].into(),
                meta: Default::default(),
                expected_version: None,
            })
            .collect();
        store.commit(&m).unwrap();
        m.clear();
        for i in chunk {
            for to in [(i + 1) % n, (i * 7 + 3) % n] {
                m.push(Mutation::AddEdge {
                    from: format!("n{}", i),
                    to: format!("n{}", to),
                    ty: None,
                    attr: Default::default(),
                    meta: Default::default(),
                });
            }
        }
        // Edges to nodes that don't exist yet are left out
        m.retain(|e| matches!(e, Mutation::AddEdge { to, .. } if to[1..].parse::<usize>().unwrap() <= chunk[chunk.len() - 1]));
        store.commit(&m).unwrap();
    }
}

#[test]
fn reads_and_commits_go_on_during_a_long_analytics_job() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), fast_options()).unwrap());
    big_graph(&store, 20_000);
    let job_time = Duration::from_millis(1500);

    // The job: PageRank without convergence, ended by its deadline
    let started = Arc::new(Barrier::new(2));
    let job = {
        let (store, started) = (store.clone(), started.clone());
        thread::spawn(move || {
            let options = ReadOptions { timeout: Some(job_time), ..ReadOptions::default() };
            let start = Instant::now();
            let outcome = store.analyze(&ProjectionSpec::default(), &options, |p| {
                started.wait();
                pagerank(p, &PageRank { tol: 0.0, max_iter: usize::MAX, ..PageRank::default() })
            });
            (outcome.map(|a| a.seq), start.elapsed())
        })
    };
    started.wait();
    let mut reads = Vec::new();
    let mut commits = Vec::new();
    let mut i = 0;
    while !job.is_finished() {
        let t = Instant::now();
        assert!(store.node("n1").is_some());
        reads.push(t.elapsed());
        if reads.len() % 20 == 0 {
            let t = Instant::now();
            store
                .commit(&[Mutation::SetAttr {
                    target: iwdb::Target::Node("n0".into()),
                    key: "x".into(),
                    value: Value::Int(i),
                    expected_version: None,
                }])
                .unwrap();
            commits.push(t.elapsed());
            i += 1;
        }
    }
    let (outcome, took) = job.join().unwrap();
    assert!(matches!(outcome, Err(Error::Timeout { .. })), "{:?}", outcome);
    assert!(took >= job_time && took < job_time * 4, "stopped at its deadline: {:?}", took);
    let (p50, p99, max) = (percentile(&mut reads, 50), percentile(&mut reads, 99), percentile(&mut reads, 100));
    let commit_max = percentile(&mut commits, 100);
    let stats = store.lock_stats();
    println!(
        "during a {:?} analytics job: {} reads (p50 {:?}, p99 {:?}, max {:?}), {} commits (max {:?}); write lock held at most {:?}",
        took,
        reads.len(),
        p50,
        p99,
        max,
        commits.len(),
        commit_max,
        stats.write_hold_max
    );
    assert!(commits.len() >= 5, "commits went on during the job");
    // Reads never waited for the job: bounded far below its duration
    assert!(p99 < Duration::from_millis(50), "p99 read latency {:?}", p99);
    assert!(max < job_time / 3, "max read latency {:?}", max);
}

#[test]
fn min_seq_waits_until_the_seq_is_applied_and_no_longer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), fast_options()).unwrap());
    let first = store.commit(&node("a")).unwrap();
    // Applied already: no wait, even with a zero timeout
    let at_once = ReadOptions { timeout: Some(Duration::ZERO), ..ReadOptions::min_seq(first.seq) };
    assert_eq!(store.read_with(&at_once, |ns| ns.seq()).unwrap(), first.seq);

    // A reader waiting for the next seq returns once it is applied
    let waiter = {
        let store = store.clone();
        thread::spawn(move || {
            let options = ReadOptions { timeout: Some(Duration::from_secs(10)), ..ReadOptions::min_seq(first.seq + 1) };
            let t = Instant::now();
            let seen = store.read_with(&options, |ns| (ns.seq(), ns.graph().node_ix("b").is_some())).unwrap();
            (seen, Instant::now(), t.elapsed())
        })
    };
    thread::sleep(Duration::from_millis(100));
    assert!(!waiter.is_finished(), "it waits");
    store.commit(&node("b")).unwrap();
    let committed = Instant::now();
    let ((seq, has_b), returned, waited) = waiter.join().unwrap();
    assert_eq!((seq, has_b), (first.seq + 1, true));
    assert!(waited >= Duration::from_millis(100));
    // "As soon as": well before any polling interval could explain it
    let late = returned.saturating_duration_since(committed);
    println!("min_seq returned {:?} after the commit", late);
    assert!(late < Duration::from_millis(500), "{:?}", late);

    // A seq that never comes: Timeout after about the timeout
    let options = ReadOptions { timeout: Some(Duration::from_millis(50)), ..ReadOptions::min_seq(100) };
    let t = Instant::now();
    assert!(matches!(store.read_with(&options, |_| ()), Err(Error::Timeout { .. })));
    assert!(t.elapsed() >= Duration::from_millis(50) && t.elapsed() < Duration::from_secs(5));
    assert!(matches!(store.wait_for_seq(100, &options), Err(Error::Timeout { .. })));

    // Cancelled by the caller
    let token = CancelToken::new();
    let options = ReadOptions { cancel: Some(token.clone()), ..ReadOptions::min_seq(100) };
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        token.cancel();
    });
    assert!(matches!(store.read_with(&options, |_| ()), Err(Error::Cancelled)));
    canceller.join().unwrap();

    // Another history: refused, whatever the seq
    let other = HistoryId([9; 16]);
    let options = ReadOptions { history: Some(other), ..ReadOptions::min_seq(1) };
    assert!(matches!(store.read_with(&options, |_| ()), Err(Error::OtherHistory { .. })));
    let own = ReadOptions { history: Some(store.history()), ..ReadOptions::min_seq(1) };
    assert!(store.read_with(&own, |_| ()).is_ok());
}

#[test]
fn a_read_only_store_fails_a_min_seq_wait_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
    store.commit(&node("a")).unwrap();
    fs.add(Rule::new(Call::Write, When::Before, Action::Fail));
    assert!(store.commit(&node("b")).is_err());
    let options = ReadOptions { timeout: Some(Duration::from_secs(30)), ..ReadOptions::min_seq(2) };
    let t = Instant::now();
    assert!(matches!(store.read_with(&options, |_| ()), Err(Error::ReadOnly { .. })));
    assert!(t.elapsed() < Duration::from_secs(5));
    // Seqs it has are still read
    assert!(store.read_with(&ReadOptions::min_seq(1), |_| ()).is_ok());
}

#[test]
fn analytics_stop_at_their_deadline_or_when_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), fast_options()).unwrap();
    big_graph(&store, 2_000);
    let endless =
        |p: &iwdb::Projection| pagerank(p, &PageRank { tol: 0.0, max_iter: usize::MAX, ..PageRank::default() });

    let options = ReadOptions { timeout: Some(Duration::from_millis(100)), ..ReadOptions::default() };
    assert!(matches!(store.analyze(&ProjectionSpec::default(), &options, endless), Err(Error::Timeout { .. })));

    let token = CancelToken::new();
    let options = ReadOptions { cancel: Some(token.clone()), ..ReadOptions::default() };
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        token.cancel();
    });
    assert!(matches!(store.analyze(&ProjectionSpec::default(), &options, endless), Err(Error::Cancelled)));
    canceller.join().unwrap();

    // A job that finishes: its result and the seq it ran on
    let spec = ProjectionSpec { direction: Direction::Both, cost: EdgeCost::Unit };
    let analysis = store.analyze(&spec, &ReadOptions::default(), |p| pagerank(p, &PageRank::default())).unwrap();
    assert_eq!(analysis.seq, store.seq());
    assert_eq!(analysis.value.len(), 2_000);
    let ranks: BTreeMap<_, _> = analysis.value.iter().enumerate().take(3).collect();
    assert!(ranks.values().all(|r| **r > 0.0));
    // A job error passes through
    let bad = |p: &iwdb::Projection| pagerank(p, &PageRank { alpha: 2.0, ..PageRank::default() });
    assert!(matches!(store.analyze(&spec, &ReadOptions::default(), bad), Err(Error::Engine(_))));
    // The store is fine afterwards
    store.commit(&node("after")).unwrap();
}

fn node(id: &str) -> Vec<Mutation> {
    vec![Mutation::UpsertNode {
        id: id.into(),
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    }]
}
