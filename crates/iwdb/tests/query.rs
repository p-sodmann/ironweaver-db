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
    // Billions of trails: only the deadline can stop it (the matcher has
    // no budget, and every limit is at its cap)
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
fn closing_waits_for_requests_and_releases_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let pending = db.commit("default", complete(3), CommitOptions::default());
    assert_eq!(block_on(pending).unwrap().seq, 1);
    db.close().unwrap();
    let db = open(dir.path());
    assert_eq!(db.store().seq(), 1);
    db.close().unwrap();
}
