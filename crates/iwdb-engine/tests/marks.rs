//! Marks in the commit pipeline (ADR 0032): a commit moves a mark
//! compare-and-set, with or without mutations; a failed commit moves
//! nothing; replaying the records and loading a checkpoint rebuild the
//! marks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ironweaver_core::{Attrs, Value};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::{
    CommitRecord, CommitResult, Error, IdempotencyKey, MarkName, MarkUpdate, Mutation, Namespace, Prepare, codec,
    invariants,
};
use std::assert_matches;

fn ns() -> Namespace {
    Namespace::new(NamespaceName::new("test").unwrap())
}

fn name(n: &str) -> MarkName {
    MarkName::new(n).unwrap()
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

/// Commit with a mark update; returns the result and the logged record.
fn commit(
    ns: &mut Namespace,
    mutations: &[Mutation],
    key: Option<&str>,
    mark: (&str, Option<u64>, u64),
) -> Result<(CommitResult, Option<CommitRecord>), Error> {
    let key = key.map(|k| IdempotencyKey::new(k).unwrap());
    let update = MarkUpdate { name: name(mark.0), expected: mark.1, position: mark.2 };
    match ns.prepare_marked(mutations, key.as_ref(), Some(&update))? {
        Prepare::Duplicate(result) => Ok((result, None)),
        Prepare::New(prepared) => {
            let record = prepared.record().clone();
            Ok((ns.apply(prepared, None)?, Some(record)))
        }
    }
}

#[test]
fn a_commit_moves_its_mark_with_its_changes() {
    let mut ns = ns();
    let (_, record) = commit(&mut ns, &[upsert("a", 1)], None, ("orders", None, 10)).unwrap();
    assert_eq!(record.unwrap().mark.map(|m| (m.name, m.position)), Some((name("orders"), 10)));
    assert_eq!(ns.mark(&name("orders")), Some(10));
    assert_eq!(ns.mark(&name("other")), None);

    // Only the mark: skipped events
    let (result, record) = commit(&mut ns, &[], None, ("orders", Some(10), 12)).unwrap();
    assert_eq!((result.seq, result.versions.len()), (2, 0));
    assert!(matches!(record.unwrap().change, iwdb_engine::Change::Data(ops) if ops.is_empty()));
    assert_eq!(ns.marks().get(&name("orders")).map(|e| (e.position, e.seq)), Some((12, 2)));

    // Marks are independent of each other
    commit(&mut ns, &[upsert("b", 1)], None, ("customers", None, 3)).unwrap();
    assert_eq!(ns.marks().len(), 2);
    assert!(invariants::check(&ns).is_empty());
}

#[test]
fn a_commit_that_fails_moves_no_mark() {
    let mut ns = ns();
    commit(&mut ns, &[upsert("a", 1)], None, ("p", None, 5)).unwrap();
    // Another writer moved it, or a projector resumed from a stale mark
    assert_matches!(
        commit(&mut ns, &[upsert("a", 2)], None, ("p", None, 6)),
        Err(Error::MarkConflict { expected: None, found: Some(5), .. })
    );
    assert_matches!(
        commit(&mut ns, &[upsert("a", 2)], None, ("p", Some(4), 6)),
        Err(Error::MarkConflict { expected: Some(4), found: Some(5), .. })
    );
    // Backwards or in place
    assert_matches!(commit(&mut ns, &[upsert("a", 2)], None, ("p", Some(5), 5)), Err(Error::InvalidMark { .. }));
    // The mark is right, the mutations aren't
    let missing = Mutation::DeleteNode { id: "nobody".into(), expected_version: None };
    assert_matches!(commit(&mut ns, &[missing], None, ("p", Some(5), 6)), Err(Error::NotFound { .. }));
    assert_eq!((ns.seq(), ns.mark(&name("p"))), (1, Some(5)));
    // Without a mark, a transaction still needs mutations
    assert_matches!(ns.prepare_keyed(&[], None), Err(Error::EmptyTransaction));
}

#[test]
fn a_keyed_retry_is_found_before_the_mark_is_checked() {
    let mut ns = ns();
    let (first, _) = commit(&mut ns, &[upsert("a", 1)], Some("k"), ("p", None, 5)).unwrap();
    // The retry's mark is stale by now, but the key answers it
    let (again, record) = commit(&mut ns, &[upsert("a", 1)], Some("k"), ("p", None, 5)).unwrap();
    assert!(record.is_none() && again.deduplicated);
    assert_eq!(again.seq, first.seq);
    assert_eq!((ns.seq(), ns.mark(&name("p"))), (1, Some(5)));
}

#[test]
fn replay_and_checkpoints_rebuild_the_marks() {
    let mut ns = ns();
    let mut records = Vec::new();
    let mut position = None;
    for i in 1..=20u64 {
        let mutations = if i % 3 == 0 { vec![] } else { vec![upsert(&format!("n{}", i % 4), i as i64)] };
        let mark = if i % 2 == 0 { "even" } else { "odd" };
        let expected = if i <= 2 { None } else { position.map(|_| i - 2) };
        records.push(commit(&mut ns, &mutations, None, (mark, expected, i)).unwrap().1.unwrap());
        position = Some(i);
    }
    // A commit without a mark leaves them
    let plain = ns.prepare(&[upsert("x", 0)]).unwrap();
    records.push(plain.record().clone());
    ns.apply(plain, None).unwrap();

    let mut replayed = self::ns();
    for record in records {
        replayed.replay(record, None).unwrap();
    }
    invariants::compare(&ns, &replayed).unwrap();
    assert_eq!(replayed.mark(&name("even")), Some(20));
    assert_eq!(replayed.marks().get(&name("odd")).map(|e| (e.position, e.seq)), Some((19, 19)));

    let bytes = codec::to_binary(ns.graph(), &ns.graph_meta()).unwrap();
    let loaded = Namespace::from_loaded(codec::from_binary(&bytes).unwrap());
    invariants::compare(&ns, &loaded).unwrap();

    // A checkpoint without marks (layout 4 and older) has none
    let mut meta = ns.graph_meta();
    meta.marks = Default::default();
    let bytes = codec::to_binary(ns.graph(), &meta).unwrap();
    assert!(Namespace::from_loaded(codec::from_binary(&bytes).unwrap()).marks().is_empty());
}
