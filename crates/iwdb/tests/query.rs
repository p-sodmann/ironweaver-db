//! The `Database` trait on the embedded store, beyond the conformance
//! suite: deadlines that stop a read in progress, and closing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use iwdb::{Embedded, Mutation, QueryConfig, Store};
use iwdb_query::exec::block_on;
use iwdb_query::{Code, CommitOptions, Database, MatchRequest, QueryOptions};

mod support;

fn open(dir: &std::path::Path) -> Embedded {
    Embedded::new(Store::open(dir, support::options(2)).unwrap(), QueryConfig::default()).unwrap()
}

/// A complete directed graph on `n` nodes.
fn complete(n: usize) -> Vec<Mutation> {
    let mut m: Vec<Mutation> = (0..n)
        .map(|i| Mutation::UpsertNode {
            id: format!("n{}", i),
            labels: vec![],
            attr: Default::default(),
            meta: Default::default(),
            expected_version: None,
        })
        .collect();
    for a in 0..n {
        for b in 0..n {
            if a != b {
                m.push(Mutation::AddEdge {
                    from: format!("n{}", a),
                    to: format!("n{}", b),
                    ty: None,
                    attr: Default::default(),
                    meta: Default::default(),
                });
            }
        }
    }
    m
}

#[test]
fn a_timeout_stops_a_read_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    block_on(db.commit("default", complete(30), CommitOptions::default())).unwrap();
    // Billions of trails: with every limit at its cap, the deadline stops
    // it long before the budget does
    let request = MatchRequest::parse("(a)-[*1..10]->(b)").unwrap();
    let options = QueryOptions { timeout: Some(Duration::from_millis(200)), ..QueryOptions::default() }.with_limits(
        Some(usize::MAX),
        Some(usize::MAX),
        Some(usize::MAX),
    );
    let start = Instant::now();
    let error = block_on(db.match_pattern("default", request, options)).unwrap_err();
    assert_eq!(error.code(), Code::Timeout, "{}", error);
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
    // The namespace is usable at once
    let seq = block_on(db.commit("default", complete(1), CommitOptions::default())).unwrap().seq;
    assert_eq!(seq, 2);
    db.close().unwrap();
}

#[test]
fn a_read_waiting_for_a_worker_times_out_at_its_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let config = QueryConfig { workers: 1, ..QueryConfig::default() };
    let db = Embedded::new(Store::open(dir.path(), support::options(2)).unwrap(), config).unwrap();
    block_on(db.commit("default", complete(30), CommitOptions::default())).unwrap();
    // A long read holds the only worker
    let request = MatchRequest::parse("(a)-[*1..10]->(b)").unwrap();
    let long = QueryOptions { timeout: Some(Duration::from_secs(60)), ..QueryOptions::default() }.with_limits(
        Some(usize::MAX),
        Some(usize::MAX),
        Some(usize::MAX),
    );
    let pending = db.match_pattern("default", request, long);
    std::thread::sleep(Duration::from_millis(100));
    // A read behind it ends at its deadline, not when the worker is free
    let start = Instant::now();
    let soon = QueryOptions { timeout: Some(Duration::from_millis(200)), ..QueryOptions::default() };
    let error = block_on(db.get_nodes("default", vec!["n0".into()], soon)).unwrap_err();
    assert_eq!(error.code(), Code::Timeout, "{}", error);
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
    // Dropping the long read's future cancels it, and the worker is free
    drop(pending);
    let read = block_on(db.get_nodes("default", vec!["n0".into()], QueryOptions::default())).unwrap();
    assert!(read.value[0].is_some());
    db.close().unwrap();
}
