//! Step 8 acceptance: a retried commit with the same idempotency key
//! returns the original result and applies once, across restarts,
//! checkpoints, a backup and restore, and a commit whose outcome is
//! unknown (a failed WAL write or fsync). The kill -9 harness checks the
//! same across process crashes (`tests/crash`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{CommitOptions, CommitResult, Error, IdempotencyKey, Mutation, RestoreSources, RestoreTarget, Store, Value};
use support::{options, reference, run_keyed, state, store_state, workload};

fn key(k: &str) -> CommitOptions {
    CommitOptions { idempotency_key: Some(IdempotencyKey::new(k).unwrap()) }
}

fn bump(n: i64) -> Vec<Mutation> {
    vec![Mutation::SetAttr {
        target: iwdb::Target::Node("a".into()),
        key: "n".into(),
        value: Value::Int(n),
        expected_version: None,
    }]
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

/// The retry returns `original` (marked deduplicated) and changes nothing.
fn assert_retry<F: iwdb::LogFs + Clone + Send + Sync + 'static>(
    store: &Store<F>,
    mutations: &[Mutation],
    k: &str,
    original: &CommitResult,
) where
    F::File: Send,
{
    let seq = store.seq();
    let retry = store.commit_with(mutations, &key(k)).unwrap();
    assert!(retry.deduplicated, "{} applied again", k);
    assert_eq!(&CommitResult { deduplicated: false, ..retry }, original);
    assert_eq!(store.seq(), seq);
}

#[test]
fn a_retry_applies_once_across_restarts_and_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    store.commit(&node("a")).unwrap();
    let first = store.commit_with(&bump(1), &key("one")).unwrap();
    assert!(!first.deduplicated && first.time.is_some());
    assert_retry(&store, &bump(1), "one", &first);

    // A restart without a checkpoint: the key comes from the WAL
    drop(store);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store.recovery().checkpoint, None);
    assert_retry(&store, &bump(1), "one", &first);

    // A checkpoint cuts the WAL: the key comes from the checkpoint
    let second = store.commit_with(&bump(2), &key("two")).unwrap();
    store.close().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert!(store.recovery().checkpoint.is_some() && store.recovery().replayed == 0);
    assert_retry(&store, &bump(1), "one", &first);
    assert_retry(&store, &bump(2), "two", &second);

    // Another request under a known key changes nothing
    let error = store.commit_with(&bump(3), &key("one")).unwrap_err();
    assert!(matches!(error, Error::Engine(iwdb_engine::Error::IdempotencyKeyReused { seq: 2, .. })), "{}", error);
    assert_eq!(store.node("a").unwrap().attr.get("n"), Some(&Value::Int(2)));
}

#[test]
fn keys_survive_a_backup_and_restores_keep_those_of_their_history() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let store = Store::open(&data, options(2)).unwrap();
    let mut reference = reference();
    run_keyed(&store, &mut reference, &workload(30, 3), "w");
    let before = store.commit_with(&node("x"), &key("before")).unwrap();
    store.backup(&root.path().join("backup")).unwrap();
    let after = store.commit_with(&node("y"), &key("after")).unwrap();
    drop(store);

    // The whole backup: the key before it is known, the one after isn't
    let restored = root.path().join("restored");
    let sources = RestoreSources { backup: Some(root.path().join("backup")), archive: None };
    iwdb::restore(&restored, &sources, RestoreTarget::Latest).unwrap();
    let store = Store::open(&restored, options(2)).unwrap();
    assert_retry(&store, &node("x"), "before", &before);
    let again = store.commit_with(&node("y"), &key("after")).unwrap();
    assert!(!again.deduplicated && again.seq == after.seq);
    drop(store);

    // To a seq before the commit: its key isn't in that history, it applies
    let early = root.path().join("early");
    iwdb::restore(&early, &sources, RestoreTarget::Seq(before.seq - 1)).unwrap();
    let store = Store::open(&early, options(2)).unwrap();
    assert!(!store.commit_with(&node("x"), &key("before")).unwrap().deduplicated);
    // Keys of the restored commits are kept
    let table = store.read(|ns| ns.keys().len());
    assert!(table > 0);
    assert_eq!(table, reference.keys().len() + 1);
}

/// A commit whose WAL write or fsync fails has an unknown outcome. Its
/// retry after reopening applies it exactly once: from the log if the
/// record got there, now if it didn't.
#[test]
fn a_retry_after_an_unknown_outcome_applies_once() {
    // (failpoint, whether recovery finds the record)
    let cases = [
        (Rule::new(Call::Write, When::Before, Action::Fail), false),
        (Rule::new(Call::Write, When::Midway, Action::NoSpace), false),
        (Rule::new(Call::Write, When::After, Action::Fail), true),
        (Rule::new(Call::Sync, When::Before, Action::Fail), true),
        (Rule::new(Call::Sync, When::After, Action::Fail), true),
    ];
    for (rule, found) in cases {
        let dir = tempfile::tempdir().unwrap();
        let fs = TestFs::default();
        let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
        let mut reference = reference();
        run_keyed(&store, &mut reference, &workload(10, 5), "w");
        fs.add(rule.clone());
        let error = store.commit_with(&node("z"), &key("in-flight")).unwrap_err();
        assert!(matches!(error, Error::Io { .. }), "{:?}: {}", rule, error);
        // Read-only now; the retry can't commit, and the key isn't known
        assert!(matches!(store.commit_with(&node("z"), &key("in-flight")), Err(Error::ReadOnly { .. })));
        // A key the store knows is still answered
        let known = store.read(|ns| ns.keys().entries().next().map(|e| (e.key.clone(), e.result.clone())));
        if let Some((k, result)) = known {
            let retry = store.read(|ns| ns.keys().get(&k).cloned()).unwrap();
            assert_eq!(retry.result, result);
        }
        drop(store);

        let store = Store::open(dir.path(), options(2)).unwrap();
        let retry = store.commit_with(&node("z"), &key("in-flight")).unwrap();
        assert_eq!(retry.deduplicated, found, "{:?}", rule);
        // Applied exactly once either way
        let mut expected = reference;
        expected.commit(&node("z")).unwrap();
        let (graph, catalog, seq, _) = state(&expected);
        let (g, c, s, _) = store_state(&store);
        assert_eq!((g, c, s), (graph, catalog, seq), "{:?}", rule);
    }
}

#[test]
fn a_duplicate_is_answered_while_the_store_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
    let first = store.commit_with(&node("a"), &key("k")).unwrap();
    fs.add(Rule::new(Call::Write, When::Before, Action::Fail));
    assert!(store.commit(&node("b")).is_err());
    assert!(store.read_only().is_some());
    assert_retry(&store, &node("a"), "k", &first);
}
