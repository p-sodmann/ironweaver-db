//! Step 8: idempotency keys in the commit pipeline. A keyed commit applies
//! once; a retry returns the original result; another request under the
//! same key is refused; replaying the records rebuilds the key table.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ironweaver_core::{Attrs, Value};
use iwdb_engine::catalog::{AttrPath, IndexDef, NamespaceName};
use iwdb_engine::codec;
use iwdb_engine::idempotency::KEY_TABLE_CAPACITY;
use iwdb_engine::invariants;
use iwdb_engine::testutil::workload::{seeded, Step};
use iwdb_engine::{
    CatalogChange, CommitRecord, CommitResult, CommitTime, Error, IdempotencyKey, Mutation, Namespace, Prepare,
};
use proptest::prelude::*;

fn ns() -> Namespace {
    Namespace::new(NamespaceName::new("test").unwrap())
}

fn key(k: &str) -> IdempotencyKey {
    IdempotencyKey::new(k).unwrap()
}

fn upsert(id: &str, n: i64) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec![],
        attr: [("n".to_owned(), Value::Int(n))].into(),
        meta: Attrs::new(),
        expected_version: None,
    }
}

fn add_edge(from: &str, to: &str) -> Mutation {
    Mutation::AddEdge { from: from.into(), to: to.into(), ty: None, attr: Attrs::new(), meta: Attrs::new() }
}

/// Commit through the keyed pipeline, at `time`; returns the result and
/// the logged record (if a new commit).
fn commit(
    ns: &mut Namespace,
    mutations: &[Mutation],
    k: Option<&str>,
    time: i64,
) -> Result<(CommitResult, Option<CommitRecord>), Error> {
    let key = k.map(key);
    match ns.prepare_keyed(mutations, key.as_ref())? {
        Prepare::Duplicate(result) => Ok((result, None)),
        Prepare::New(prepared) => {
            let record = prepared.record().clone();
            Ok((ns.apply(prepared, Some(CommitTime(time)))?, Some(record)))
        }
    }
}

#[test]
fn a_retry_returns_the_original_result_and_applies_nothing() {
    let mut ns = ns();
    commit(&mut ns, &[upsert("a", 1), upsert("b", 1)], None, 1).unwrap();
    let (first, record) = commit(&mut ns, &[add_edge("a", "b"), upsert("a", 2)], Some("k1"), 10).unwrap();
    let record = record.unwrap();
    assert_eq!(record.keyed.as_ref().map(|k| k.key.as_str()), Some("k1"));
    assert_eq!(first.time, Some(CommitTime(10)));
    assert!(!first.deduplicated);
    let seq = ns.seq();

    let (again, record) = commit(&mut ns, &[add_edge("a", "b"), upsert("a", 2)], Some("k1"), 20).unwrap();
    assert!(record.is_none());
    assert!(again.deduplicated);
    assert_eq!(CommitResult { deduplicated: false, ..again }, first);
    assert_eq!(ns.seq(), seq);
    assert_eq!(ns.graph().edge_count(), 1);
}

#[test]
fn another_request_under_the_same_key_is_refused() {
    let mut ns = ns();
    commit(&mut ns, &[upsert("a", 1)], Some("k"), 1).unwrap();
    let error = commit(&mut ns, &[upsert("a", 2)], Some("k"), 2).unwrap_err();
    assert_eq!(error, Error::IdempotencyKeyReused { key: key("k"), seq: 1 });
    assert_eq!(ns.seq(), 1);
    // A catalog change is another request too
    let index = CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["n"]).unwrap() });
    assert!(matches!(ns.prepare_catalog_keyed(index, Some(&key("k"))), Err(Error::IdempotencyKeyReused { .. })));
}

#[test]
fn the_lookup_comes_before_validation() {
    let mut ns = ns();
    let create = [Mutation::UpsertNode {
        id: "a".into(),
        labels: vec![],
        attr: Attrs::new(),
        meta: Attrs::new(),
        expected_version: Some(0),
    }];
    commit(&mut ns, &create, Some("create"), 1).unwrap();
    // Without the key it would conflict now (the node exists)
    assert!(matches!(commit(&mut ns, &create, None, 2), Err(Error::Conflict { .. })));
    let (retry, _) = commit(&mut ns, &create, Some("create"), 2).unwrap();
    assert!(retry.deduplicated && retry.seq == 1);
}

#[test]
fn failed_commits_and_unkeyed_commits_leave_no_key() {
    let mut ns = ns();
    assert!(commit(&mut ns, &[add_edge("x", "y")], Some("k"), 1).is_err());
    assert!(ns.keys().is_empty());
    commit(&mut ns, &[upsert("x", 1)], None, 1).unwrap();
    assert!(ns.keys().is_empty());
    // The key is free: it applies
    let (result, _) = commit(&mut ns, &[upsert("x", 2)], Some("k"), 2).unwrap();
    assert!(!result.deduplicated && result.seq == 2);
}

#[test]
fn catalog_changes_take_keys_too() {
    let mut ns = ns();
    let index = CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["n"]).unwrap() });
    let Prepare::New(prepared) = ns.prepare_catalog_keyed(index.clone(), Some(&key("ix"))).unwrap() else {
        panic!("new")
    };
    ns.apply(prepared, None).unwrap();
    // Without a key the retry fails (the index exists); with it, the original result
    assert!(matches!(ns.prepare_catalog(index.clone()), Err(Error::IndexExists { .. })));
    let Prepare::Duplicate(result) = ns.prepare_catalog_keyed(index, Some(&key("ix"))).unwrap() else {
        panic!("duplicate")
    };
    assert_eq!((result.seq, result.deduplicated), (1, true));
}

#[test]
fn the_table_evicts_the_oldest_keys() {
    let mut ns = ns();
    for i in 0..KEY_TABLE_CAPACITY as i64 + 3 {
        commit(&mut ns, &[upsert("a", i)], Some(&format!("k{}", i)), i).unwrap();
    }
    assert_eq!(ns.keys().len(), KEY_TABLE_CAPACITY);
    // k0 was evicted: the "retry" applies again
    let (result, _) = commit(&mut ns, &[upsert("a", 0)], Some("k0"), 0).unwrap();
    assert!(!result.deduplicated);
    // ... and evicted the oldest left, k3; k4 is still there
    assert!(ns.keys().get(&key("k3")).is_none());
    let (result, _) = commit(&mut ns, &[upsert("a", 4)], Some("k4"), 0).unwrap();
    assert!(result.deduplicated);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Replaying the records (with their times) gives the same key table,
    /// and so does a save and load of the namespace (a checkpoint).
    #[test]
    fn replay_and_checkpoints_rebuild_the_table(seed in any::<u64>(), keyed in 1u64..4) {
        let mut live = ns();
        let mut records = Vec::new();
        for (i, step) in seeded(60, seed).into_iter().enumerate() {
            // Every `keyed`-th step has a key; some keys repeat (retries)
            let k = (i as u64 % keyed == 0).then(|| format!("k{}", i / 3));
            let key = k.as_deref().map(key);
            let prepared = match &step {
                Step::Tx(mutations) => live.prepare_keyed(mutations, key.as_ref()),
                Step::Catalog(change) => live.prepare_catalog_keyed(change.clone(), key.as_ref()),
            };
            if let Ok(Prepare::New(prepared)) = prepared {
                let time = Some(CommitTime(i as i64));
                records.push((prepared.record().clone(), time));
                live.apply(prepared, time).unwrap();
            }
        }
        let mut replica = ns();
        for (record, time) in records {
            replica.replay(record, time).unwrap();
        }
        prop_assert_eq!(invariants::compare(&live, &replica), Ok(()));
        prop_assert!(invariants::check(&live).is_empty());
        let saved = codec::to_binary(live.graph(), &live.graph_meta()).unwrap();
        let loaded = Namespace::from_loaded(codec::from_binary(&saved).unwrap());
        prop_assert_eq!(loaded.keys(), live.keys());
        prop_assert_eq!(invariants::compare(&live, &loaded), Ok(()));
    }
}
