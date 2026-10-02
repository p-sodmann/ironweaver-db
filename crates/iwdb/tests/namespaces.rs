//! Namespaces (step 9, ADR 0017): independent graphs, ids, keys, drops
//! and their waiters, constraints under concurrent writers, and every file
//! operation of create, drop and the layout upgrade failing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{
    AttrPath, CatalogChange, CommitOptions, Constraint, ConstraintKind, Error, IdempotencyKey, IndexDef, IndexState,
    Label, Mutation, ReadOptions, Store, Value,
};
use support::options;

fn upsert(id: &str, email: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["Person".into()],
        attr: [("email".to_owned(), Value::from(email))].into(),
        meta: Default::default(),
        expected_version: None,
    }
}

fn key(k: &str) -> IdempotencyKey {
    IdempotencyKey::new(k).unwrap()
}

fn with_key(k: &str) -> CommitOptions {
    CommitOptions { idempotency_key: Some(key(k)) }
}

fn path(p: &str) -> AttrPath {
    AttrPath::new([p]).unwrap()
}

fn unique(label: &str, p: &str) -> Constraint {
    Constraint { kind: ConstraintKind::Unique, label: Label::new(label).unwrap(), path: path(p) }
}

fn names(store: &Store) -> Vec<String> {
    store.namespaces().iter().map(|n| n.name.to_string()).collect()
}

fn ns_dirs(root: &Path) -> BTreeSet<String> {
    fs::read_dir(root.join("ns")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
}

#[test]
fn namespaces_are_independent_graphs_with_their_own_seqs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(names(&store), ["default"]);
    let created = store.create_namespace("social", None).unwrap();
    assert!(!created.deduplicated && created.event.id == 2 && created.event.seq == 2);
    store.create_namespace("a-b_1", None).unwrap();
    assert_eq!(names(&store), ["a-b_1", "default", "social"]);

    // The same node id in two namespaces; seqs start at 1 in each
    let social = store.namespace("social").unwrap();
    assert_eq!((social.id(), social.name()), (2, "social"));
    assert_eq!(social.commit(&[upsert("alice", "a@x")]).unwrap().seq, 1);
    assert_eq!(store.commit(&[upsert("alice", "other@x")]).unwrap().seq, 1);
    assert_eq!(social.commit(&[upsert("bob", "b@x")]).unwrap().seq, 2);
    assert_eq!(store.namespace("a-b_1").unwrap().seq(), 0);
    assert_eq!(social.node("alice").unwrap().attr["email"], Value::from("a@x"));
    assert_eq!(store.node("alice").unwrap().attr["email"], Value::from("other@x"));
    assert!(store.node("bob").is_none());
    assert_eq!(store.status().seq, 1, "the store's shorthand fields are default's");

    // Catalogs are per namespace
    social.commit_catalog(CatalogChange::AddConstraint(unique("Person", "email"))).unwrap();
    assert_eq!(social.catalog().constraints().count(), 1);
    assert_eq!(store.catalog().constraints().count(), 0);
    // so a value that is taken in `social` is free in `default`
    assert!(social.commit(&[upsert("carol", "a@x")]).is_err());
    store.commit(&[upsert("carol", "a@x")]).unwrap();

    // Status: counts and indexes per namespace
    let status = store.status();
    let s = status.namespaces.iter().find(|n| n.name == "social").unwrap();
    assert_eq!((s.nodes, s.constraints, s.seq), (2, 1, 3));
    assert_eq!(s.indexes.len(), 1);
    assert!(s.indexes[0].unique && !s.indexes[0].declared && s.indexes[0].state == IndexState::Ready);
    assert!(s.memory_bytes > 0);
    let size = s.indexes[0].size.unwrap();
    assert_eq!((size.entries, size.distinct_keys), (2, 2));
    assert!(size.memory_bytes > 0 && size.memory_bytes < s.memory_bytes);
    assert_eq!(social.index_entries(&path("email")), Some(2));

    // Unknown and invalid names
    assert!(matches!(store.namespace("nope"), Err(Error::NoSuchNamespace { .. })));
    assert!(matches!(store.create_namespace("social", None), Err(Error::NamespaceExists { .. })));
    for bad in ["", "-x", "a b", "a/b", "..", &"n".repeat(65)] {
        assert!(matches!(store.create_namespace(bad, None), Err(Error::Engine(_))), "{}", bad);
    }
    assert!(matches!(store.drop_namespace("default", None), Err(Error::InvalidOptions(_))));
    assert!(matches!(store.drop_namespace("nope", None), Err(Error::NoSuchNamespace { .. })));
}

#[test]
fn namespaces_survive_close_checkpoints_and_crashes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    store.create_namespace("one", None).unwrap();
    store.create_namespace("two", None).unwrap();
    for (i, n) in ["one", "two"].iter().enumerate() {
        let ns = store.namespace(n).unwrap();
        for j in 0..30 {
            ns.commit(&[upsert(&format!("n{}", j), &format!("{}{}@x", i, j))]).unwrap();
            if j == 12 {
                ns.checkpoint().unwrap();
            }
        }
    }
    store.drop_namespace("one", None).unwrap();
    store.create_namespace("three", None).unwrap();
    store.namespace("three").unwrap().commit(&[upsert("t", "t@x")]).unwrap();
    let before = store.status();
    drop(store); // like a crash: no sync, no checkpoint

    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(names(&store), ["default", "three", "two"]);
    let after = store.status();
    for (a, b) in before.namespaces.iter().zip(&after.namespaces) {
        assert_eq!((&a.name, a.id, a.seq, a.nodes), (&b.name, b.id, b.seq, b.nodes));
    }
    assert_eq!(store.namespace("two").unwrap().seq(), 30);
    assert_eq!(store.recovery().namespaces.len(), 3);
    assert_eq!(store.recovery().namespace("two").unwrap().checkpoint, Some(13));
    // The dropped namespace's directory is gone, the others' are there
    assert_eq!(ns_dirs(dir.path()).len(), 3);
    assert!(!dir.path().join("ns/00000000000000000002").exists());
    store.close().unwrap();
    let report = iwdb::verify(dir.path()).unwrap();
    assert!(report.is_ok(), "{:#?}", report);
    assert_eq!(report.namespaces.len(), 3);
}

#[test]
fn ids_are_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let first = store.create_namespace("x", None).unwrap().event.id;
    store.namespace("x").unwrap().commit(&[upsert("a", "a@x")]).unwrap();
    let handle = store.namespace("x").unwrap();
    store.drop_namespace("x", None).unwrap();
    let second = store.create_namespace("x", None).unwrap().event.id;
    assert_ne!(first, second);
    // The new namespace is empty and at seq 0; the old handle fails
    assert_eq!(store.namespace("x").unwrap().seq(), 0);
    assert!(store.namespace("x").unwrap().node("a").is_none());
    assert!(matches!(handle.commit(&[upsert("b", "b@x")]), Err(Error::NamespaceDropped { .. })));
    drop(store);
    // Ids survive a reopen: the next one is higher than every earlier one
    let store = Store::open(dir.path(), options(2)).unwrap();
    store.drop_namespace("x", None).unwrap();
    let third = store.create_namespace("y", None).unwrap().event.id;
    assert!(third > second);
}

#[test]
fn namespace_operations_take_idempotency_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let k = key("create-1");
    let first = store.create_namespace("a", Some(&k)).unwrap();
    let again = store.create_namespace("a", Some(&k)).unwrap();
    assert!(!first.deduplicated && again.deduplicated);
    assert_eq!(first.event, again.event);
    // Another request under the key
    assert!(matches!(
        store.create_namespace("b", Some(&k)),
        Err(Error::Engine(iwdb_engine::Error::IdempotencyKeyReused { .. }))
    ));
    assert!(matches!(store.drop_namespace("a", Some(&k)), Err(Error::Engine(_))));
    assert_eq!(names(&store), ["a", "default"]);
    // A drop's key is answered after the namespace is gone, and after a reopen
    let d = key("drop-1");
    let dropped = store.drop_namespace("a", Some(&d)).unwrap();
    store.close().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let retry = store.drop_namespace("a", Some(&d)).unwrap();
    assert!(retry.deduplicated && retry.event == dropped.event);
    assert!(store.create_namespace("a", Some(&k)).unwrap().deduplicated, "an old create's key still answers");
    assert_eq!(names(&store), ["default"], "and creates nothing");
    // The key tables of data commits are per namespace: one key, two namespaces, two commits
    store.create_namespace("p", None).unwrap();
    store.create_namespace("q", None).unwrap();
    let (p, q) = (store.namespace("p").unwrap(), store.namespace("q").unwrap());
    let r1 = p.commit_with(&[upsert("n", "n@x")], &with_key("same")).unwrap();
    let r2 = q.commit_with(&[upsert("n", "n@x")], &with_key("same")).unwrap();
    assert!(!r1.deduplicated && !r2.deduplicated);
    assert!(p.commit_with(&[upsert("n", "n@x")], &with_key("same")).unwrap().deduplicated);
    // and the namespace key space is separate from the data key space
    assert!(!store.create_namespace("r", Some(&key("same"))).unwrap().deduplicated);
}

#[test]
fn a_dropped_namespace_fails_commits_and_wakes_waiters_but_finishes_reads() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), options(2)).unwrap());
    store.create_namespace("gone", None).unwrap();
    let ns = store.namespace("gone").unwrap();
    ns.commit(&[upsert("a", "a@x")]).unwrap();

    let waiter = {
        let store = store.clone();
        std::thread::spawn(move || {
            let ns = store.namespace("gone").unwrap();
            ns.wait_for_seq(1000, &ReadOptions { timeout: Some(Duration::from_secs(20)), ..ReadOptions::default() })
        })
    };
    // A read in progress while the namespace is dropped finishes on what it read
    let (started_tx, started) = std::sync::mpsc::channel();
    let (go_tx, go) = std::sync::mpsc::channel::<()>();
    let reader = {
        let store = store.clone();
        std::thread::spawn(move || {
            let ns = store.namespace("gone").unwrap();
            ns.read(|n| {
                started_tx.send(()).unwrap();
                go.recv().unwrap();
                n.graph().node_count()
            })
        })
    };
    started.recv().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    // The drop waits for no reader: it needs the writer's lock, not the namespace's
    let dropper = {
        let store = store.clone();
        std::thread::spawn(move || store.drop_namespace("gone", None).unwrap())
    };
    let waited = waiter.join().unwrap();
    assert!(matches!(waited, Err(Error::NamespaceDropped { .. })), "{:?}", waited);
    go_tx.send(()).unwrap();
    assert_eq!(reader.join().unwrap(), 1);
    dropper.join().unwrap();
    assert!(matches!(ns.commit(&[upsert("b", "b@x")]), Err(Error::NamespaceDropped { .. })));
    assert!(matches!(
        ns.commit_catalog(CatalogChange::CreateIndex(IndexDef { path: path("email") })),
        Err(Error::NamespaceDropped { .. })
    ));
    assert!(matches!(store.namespace("gone"), Err(Error::NoSuchNamespace { .. })));
    assert!(!dir.path().join("ns/00000000000000000002").exists());
}

/// Threads committing conflicting values to one namespace: exactly one wins
/// each value. The same values in two namespaces don't conflict.
#[test]
fn unique_constraints_hold_under_concurrent_writers_per_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), options(2)).unwrap());
    for n in ["left", "right"] {
        store.create_namespace(n, None).unwrap();
        store.namespace(n).unwrap().commit_catalog(CatalogChange::AddConstraint(unique("Person", "email"))).unwrap();
    }
    const THREADS: usize = 6;
    const VALUES: usize = 40;
    let wins = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
    let barrier = Arc::new(Barrier::new(THREADS * 2));
    let mut handles = Vec::new();
    for t in 0..THREADS * 2 {
        let (store, wins, barrier) = (store.clone(), wins.clone(), barrier.clone());
        handles.push(std::thread::spawn(move || {
            let (name, counter) = if t % 2 == 0 { ("left", &wins.0) } else { ("right", &wins.1) };
            let ns = store.namespace(name).unwrap();
            barrier.wait();
            for v in 0..VALUES {
                // Every thread of a namespace tries every value, under its own node id
                match ns.commit(&[upsert(&format!("{}-{}-{}", name, t, v), &format!("v{}@x", v))]) {
                    Ok(_) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(Error::Engine(iwdb_engine::Error::ConstraintViolation { .. })) => {}
                    Err(e) => panic!("{}", e),
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // Each value won once per namespace: VALUES wins in each, and every value is there once
    assert_eq!((wins.0.load(Ordering::SeqCst), wins.1.load(Ordering::SeqCst)), (VALUES, VALUES));
    for name in ["left", "right"] {
        let ns = store.namespace(name).unwrap();
        let emails: Vec<String> =
            ns.read(|n| n.graph().nodes().map(|(_, node)| format!("{:?}", node.data.attr["email"])).collect());
        let distinct: BTreeSet<_> = emails.iter().collect();
        assert_eq!((emails.len(), distinct.len()), (VALUES, VALUES), "{}", name);
        let report = ns.read(iwdb_engine::invariants::check);
        assert!(report.is_empty(), "{:?}", report);
    }
}

// ---- every file operation of create, drop and the upgrade can fail ----

/// The file calls `op` makes on a store set up by `setup` (and opened after).
fn calls_of(setup: impl FnOnce(&TestFs, &Path), op: impl FnOnce(&Store<TestFs>)) -> Vec<Call> {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    setup(&fs, dir.path());
    let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
    let before = fs.state().calls.len();
    op(&store);
    let calls = fs.state().calls[before..].to_vec();
    calls
}

/// How many calls of each kind there are.
fn per_kind(calls: &[Call]) -> Vec<(Call, u64)> {
    Call::ALL.iter().map(|c| (*c, calls.iter().filter(|x| *x == c).count() as u64)).filter(|(_, n)| *n > 0).collect()
}

/// After a failure at any file operation of creating `name`: the next open
/// shows the namespace fully there (usable, with its directory) or fully
/// gone (no directory), the store is consistent, and a retry with the same
/// key ends with the namespace there exactly once.
#[test]
fn every_file_operation_of_a_create_can_fail() {
    let setup = |fs: &TestFs, dir: &Path| {
        let store = Store::open_with(fs.clone(), dir, options(2)).unwrap();
        store.create_namespace("keep", None).unwrap();
        store.namespace("keep").unwrap().commit(&[upsert("k", "k@x")]).unwrap();
        store.close().unwrap();
    };
    let calls = calls_of(setup, |store| {
        let _ = store.create_namespace("fresh", Some(&key("create-fresh")));
    });
    assert!(calls.len() > 10, "a create makes file calls ({:?})", calls);
    for needed in [Call::CreateDir, Call::SyncDir, Call::Write, Call::Sync] {
        assert!(calls.contains(&needed), "{:?}", needed);
    }
    let mut seen_gone = false;
    let mut seen_there = false;
    // Failing the k-th call of each kind, k from 0 up to what the create does
    for (call, count) in per_kind(&calls) {
        for skip in 0..count {
            for when in [When::Before, When::After] {
                let dir = tempfile::tempdir().unwrap();
                let fs = TestFs::default();
                setup(&fs, dir.path());
                let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
                // Only calls from here on count
                let rule = Rule::new(call, when, Action::Fail).skip(skip);
                fs.add(rule.clone());
                let result = store.create_namespace("fresh", Some(&key("create-fresh")));
                let fired = !fs.state().fired.is_empty();
                drop(store);
                if !fired {
                    assert!(result.is_ok(), "{}: {:?}", rule, result.err());
                    continue;
                }
                // Reopen: fully there, or fully gone
                let store = Store::open(dir.path(), options(2)).unwrap_or_else(|e| panic!("{}: {}", rule, e));
                let listed = names(&store).contains(&"fresh".to_owned());
                let dirs = ns_dirs(dir.path());
                if listed {
                    seen_there = true;
                    assert_eq!(dirs.len(), 3, "{}: {:?}", rule, dirs);
                    let fresh = store.namespace("fresh").unwrap();
                    fresh.commit(&[upsert("n", "n@x")]).unwrap();
                } else {
                    seen_gone = true;
                    assert_eq!(dirs.len(), 2, "{}: {:?}", rule, dirs);
                }
                assert_eq!(store.namespace("keep").unwrap().seq(), 1, "{}", rule);
                // The retry ends with the namespace there once
                let retry = store.create_namespace("fresh", Some(&key("create-fresh"))).unwrap();
                assert_eq!(retry.deduplicated, listed, "{}", rule);
                assert_eq!(names(&store), ["default", "fresh", "keep"], "{}", rule);
                store.close().unwrap();
                let report = iwdb::verify(dir.path()).unwrap();
                assert!(report.is_ok(), "{}: {:#?}", rule, report.problems);
            }
        }
    }
    assert!(seen_gone && seen_there, "failures landed both before and after the create event");
}

/// The same for a drop: the namespace is fully there, with all its data,
/// or fully gone with no directory.
#[test]
fn every_file_operation_of_a_drop_can_fail() {
    let setup = |fs: &TestFs, dir: &Path| {
        let store = Store::open_with(fs.clone(), dir, options(2)).unwrap();
        store.create_namespace("victim", None).unwrap();
        let v = store.namespace("victim").unwrap();
        for i in 0..25 {
            v.commit(&[upsert(&format!("n{}", i), &format!("{}@x", i))]).unwrap();
            if i == 10 {
                v.checkpoint().unwrap();
            }
        }
        store.close().unwrap();
    };
    let calls = calls_of(setup, |store| {
        let _ = store.drop_namespace("victim", Some(&key("drop-victim")));
    });
    assert!(calls.contains(&Call::RemoveDirAll) && calls.contains(&Call::Write), "{:?}", calls);
    let (mut seen_gone, mut seen_there) = (false, false);
    for (call, count) in per_kind(&calls) {
        for skip in 0..count {
            for when in [When::Before, When::After] {
                let dir = tempfile::tempdir().unwrap();
                let fs = TestFs::default();
                setup(&fs, dir.path());
                let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
                let rule = Rule::new(call, when, Action::Fail).skip(skip);
                fs.add(rule.clone());
                let _ = store.drop_namespace("victim", Some(&key("drop-victim")));
                let fired = !fs.state().fired.is_empty();
                drop(store);
                if !fired {
                    continue;
                }
                let store = Store::open(dir.path(), options(2)).unwrap_or_else(|e| panic!("{}: {}", rule, e));
                let listed = names(&store).contains(&"victim".to_owned());
                if listed {
                    seen_there = true;
                    let v = store.namespace("victim").unwrap();
                    assert_eq!((v.seq(), v.read(|n| n.graph().node_count())), (25, 25), "{}: all its data", rule);
                    assert_eq!(ns_dirs(dir.path()).len(), 2, "{}", rule);
                } else {
                    seen_gone = true;
                    assert_eq!(ns_dirs(dir.path()).len(), 1, "{}: {:?}", rule, ns_dirs(dir.path()));
                }
                let retry = store.drop_namespace("victim", Some(&key("drop-victim"))).unwrap();
                assert_eq!(retry.deduplicated, !listed, "{}", rule);
                assert_eq!(names(&store), ["default"], "{}", rule);
                store.close().unwrap();
                assert!(iwdb::verify(dir.path()).unwrap().is_ok(), "{}", rule);
                assert_eq!(ns_dirs(dir.path()).len(), 1, "{}", rule);
            }
        }
    }
    assert!(seen_gone && seen_there);
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A failure at any file operation of the upgrade from layout 1, 2 or 3:
/// the open fails or succeeds, and the next open recovers the same state,
/// at layout 4, with the history id kept (layouts 2 and 3).
#[test]
fn every_file_operation_of_the_layout_upgrade_can_fail() {
    for version in 1..=3 {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/data-dir-v{}/store", version));
        let expected = {
            let dir = tempfile::tempdir().unwrap();
            copy_dir(&fixture, dir.path());
            let store = Store::open(dir.path(), options(2)).unwrap();
            let (state, history) = (store.read(support::state), store.history());
            assert_eq!(store.recovery().upgraded_from, Some(version));
            (state, history)
        };
        let calls = {
            let dir = tempfile::tempdir().unwrap();
            copy_dir(&fixture, dir.path());
            let fs = TestFs::default();
            Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
            let calls = fs.state().calls.clone();
            calls
        };
        for (call, count) in per_kind(&calls) {
            for skip in 0..count {
                for when in [When::Before, When::After] {
                    let dir = tempfile::tempdir().unwrap();
                    copy_dir(&fixture, dir.path());
                    let fs = TestFs::default();
                    let rule = Rule::new(call, when, Action::Fail).skip(skip);
                    fs.add(rule.clone());
                    let first = Store::open_with(fs.clone(), dir.path(), options(2));
                    let fired = !fs.state().fired.is_empty();
                    drop(first);
                    if !fired {
                        continue;
                    }
                    let store =
                        Store::open(dir.path(), options(2)).unwrap_or_else(|e| panic!("v{} {}: {}", version, rule, e));
                    assert_eq!(store.read(support::state), expected.0, "v{} {}", version, rule);
                    if version > 1 {
                        assert_eq!(store.history(), expected.1, "v{} {}", version, rule);
                    }
                    drop(store);
                    assert_eq!(
                        iwdb_storage::layout::read_marker(dir.path()).unwrap().unwrap().version,
                        iwdb_storage::layout::LAYOUT_VERSION
                    );
                    assert!(iwdb::verify(dir.path()).unwrap().is_ok(), "v{} {}", version, rule);
                }
            }
        }
    }
}
