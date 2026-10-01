//! Compatibility fixtures for the data directory layouts and their
//! checkpoint format (design rule 4). Each is a data directory with a
//! checkpoint and WAL records after it, and every later version must open
//! it and recover the state in `expected.txt`:
//!
//! - `tests/fixtures/data-dir-v1/`: layout 1, written by step 5 (WAL format
//!   1). Opening a copy upgrades it to the current layout;
//! - `tests/fixtures/data-dir-v2/`: layout 2, written by step 7 (WAL format
//!   2), with its history id in `expected.txt`. Opening a copy upgrades it
//!   to layout 3 with the same history;
//! - `tests/fixtures/data-dir-v3/`: layout 3, written by step 8 (WAL format
//!   3), with keyed commits before and after its checkpoint: the key table
//!   is in the checkpoint's graph meta and in the WAL records. Opening a
//!   copy upgrades it to layout 4 (one namespace, `default`);
//! - `tests/fixtures/data-dir-v4/`: layout 4, written by step 9: three
//!   namespaces (one of them with keyed commits before and after its
//!   checkpoint, an index and constraints), and one that was dropped.
//!
//! A new layout version gets a new fixture next to these, written by:
//!
//! ```text
//! cargo test -p iwdb --test data_dir_fixture -- --ignored generate_fixture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{HistoryId, Namespace, Store};
use iwdb_storage::layout::{read_marker, LAYOUT_VERSION, MARKER_NAME};
use support::{options, pad, reference, run, run_keyed, snapshot, workload};

fn fixture(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/data-dir-v{}", version))
}

/// The expected state as text: the canonical graph, the catalog, the seq
/// and (from layout 3, when it has entries) the idempotency key table,
/// with commit times.
fn describe(ns: &Namespace) -> String {
    let mut out = String::new();
    for line in iwdb_engine::testutil::canonical(ns.graph()) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("catalog {:?}\nseq {}\n", ns.catalog(), ns.seq()));
    for entry in ns.keys().entries() {
        out.push_str(&format!("key {:?}\n", entry));
    }
    out
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

/// Writes the fixture of the current layout, if it doesn't exist yet.
///
/// Layout 4: the default namespace as in layout 3, then `people` (a
/// unique constraint, an index, keyed commits before and after its
/// checkpoint), `scratch` (commits, no checkpoint) and `gone` (created
/// with a key, filled, dropped with a key). `expected.txt` is the default
/// namespace's description and `history`, then a `namespace <name> <id>`
/// line and the description of each other live namespace, and a `dropped
/// <name> <id>` line.
#[test]
#[ignore = "writes the fixture"]
fn generate_fixture() {
    use iwdb::{AttrPath, CatalogChange, CommitOptions, Constraint, ConstraintKind, IdempotencyKey, IndexDef, Label};
    use iwdb::{Mutation, Value};
    let dir = fixture(LAYOUT_VERSION);
    if dir.exists() {
        return;
    }
    let mut reference = reference();
    let steps = workload(40, 5_000);
    let store = Store::open(&dir.join("store"), options(2)).unwrap();
    run(&store, &mut reference, &steps[..30]);
    // Keyed commits (layout 3) before the checkpoint, so the key table is
    // in it, and after it, in the WAL
    run_keyed(&store, &mut reference, &steps[30..40], "before-");
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[40..]);
    run_keyed(&store, &mut reference, &[pad(1000), pad(1001)], "after-");
    // A retry: the store answers it from the table, nothing is logged
    run_keyed(&store, &mut reference, &[pad(1000)], "after-");

    let key = |k: &str| IdempotencyKey::new(k).unwrap();
    let keyed = |k: &str| CommitOptions { idempotency_key: Some(key(k)) };
    let person = |i: i64| Mutation::UpsertNode {
        id: format!("p{}", i),
        labels: vec!["Person".into()],
        attr: [("email".to_owned(), Value::from(format!("p{}@example.org", i)))].into(),
        meta: Default::default(),
        expected_version: None,
    };
    store.create_namespace("people", Some(&key("mk-people"))).unwrap();
    store.create_namespace("scratch", None).unwrap();
    store.create_namespace("gone", Some(&key("mk-gone"))).unwrap();
    let people = store.namespace("people").unwrap();
    let path = AttrPath::new(["email"]).unwrap();
    people
        .commit_catalog(CatalogChange::AddConstraint(Constraint {
            kind: ConstraintKind::Unique,
            label: Label::new("Person").unwrap(),
            path: path.clone(),
        }))
        .unwrap();
    people.commit_catalog(CatalogChange::CreateIndex(IndexDef { path })).unwrap();
    for i in 0..6 {
        people.commit_with(&[person(i)], &keyed(&format!("person-{}", i))).unwrap();
    }
    people.checkpoint().unwrap();
    for i in 6..9 {
        people.commit_with(&[person(i)], &keyed(&format!("person-{}", i))).unwrap();
    }
    let scratch = store.namespace("scratch").unwrap();
    for i in 0..5 {
        scratch.commit(&[person(100 + i)]).unwrap();
    }
    let gone = store.namespace("gone").unwrap();
    for i in 0..3 {
        gone.commit(&[person(200 + i)]).unwrap();
    }
    store.drop_namespace("gone", Some(&key("rm-gone"))).unwrap();

    let history = store.history();
    let mut expected = describe(&reference) + &format!("history {}\n", history);
    for info in store.namespaces() {
        if info.name.as_str() != "default" {
            let ns = store.namespace(info.name.as_str()).unwrap();
            expected += &format!("namespace {} {}\n{}", info.name, info.id, ns.read(describe));
        }
    }
    expected += "dropped gone 4\n";
    // No close: the WAL holds records after the checkpoint
    drop(store);
    fs::write(dir.join("expected.txt"), expected).unwrap();
}

/// Open a copy of the fixture of layout `version`; check its state.
fn open_fixture(version: u32) -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture(version).join("store"), dir.path());
    let before = snapshot(&fixture(version));
    let store = Store::open(dir.path(), options(2)).unwrap();
    let expected = fs::read_to_string(fixture(version).join("expected.txt")).unwrap();
    let (state, rest) = expected.split_at(expected.find("history ").unwrap_or(expected.len()));
    assert_eq!(store.read(describe), state);
    assert_eq!(store.read(|ns| ns.keys().is_empty()), version < 3);
    let report = store.recovery();
    assert!(report.checkpoint.is_some());
    assert!(report.replayed > 0, "the fixture has WAL records after its checkpoint");
    assert!(report.skipped_checkpoints.is_empty() && report.torn_tail.is_none());
    assert!(report.index_changes.created.is_empty() && report.index_changes.dropped.is_empty());
    assert_eq!(snapshot(&fixture(version)), before, "the fixture itself is unchanged");
    (dir, store, rest.to_owned())
}

#[test]
fn the_v1_fixture_opens_recovers_its_state_and_is_upgraded() {
    let (dir, store, _) = open_fixture(1);
    assert_eq!(store.recovery().upgraded_from, Some(1));
    let history = store.history();
    drop(store);
    let marker = read_marker(dir.path()).unwrap().unwrap();
    assert_eq!((marker.version, marker.history), (LAYOUT_VERSION, Some(history)));
    assert_eq!(fs::read(dir.path().join(MARKER_NAME)).unwrap().len(), 32);
    // Reopened, it is a directory of the current layout with the same history
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!((store.recovery().upgraded_from, store.history()), (None, history));
}

#[test]
fn the_v2_fixture_opens_recovers_its_state_and_history_and_is_upgraded() {
    let (dir, store, rest) = open_fixture(2);
    assert_eq!(store.recovery().upgraded_from, Some(2));
    let history: HistoryId = rest.trim().strip_prefix("history ").unwrap().parse().unwrap();
    assert_eq!(store.history(), history);
    drop(store);
    let marker = read_marker(dir.path()).unwrap().unwrap();
    assert_eq!((marker.version, marker.history), (LAYOUT_VERSION, Some(history)));
}

#[test]
fn the_v3_fixture_opens_recovers_its_state_keys_and_history_and_is_upgraded() {
    let (_dir, store, rest) = open_fixture(3);
    assert_eq!(store.recovery().upgraded_from, Some(3));
    let history: HistoryId = rest.trim().strip_prefix("history ").unwrap().parse().unwrap();
    assert_eq!(store.history(), history);
    // A retry of a keyed commit before the checkpoint, and of one after it,
    // returns the original result and commits nothing
    let seq = store.seq();
    for key in ["before-3", "after-1"] {
        let entry = store.read(|ns| ns.keys().get(&iwdb::IdempotencyKey::new(key).unwrap()).cloned());
        assert!(entry.is_some(), "{}", key);
    }
    let step = support::pad(1001);
    let support::Step::Tx(mutations) = step else { panic!("a transaction") };
    let options = iwdb::CommitOptions { idempotency_key: Some(iwdb::IdempotencyKey::new("after-1").unwrap()) };
    let result = store.commit_with(&mutations, &options).unwrap();
    assert!(result.deduplicated && result.time.is_some());
    assert_eq!(store.seq(), seq);
}

/// A failure while the marker is upgraded (layouts 1 and 2): before the
/// rename, the old marker stays; after it, the new one is in place. Either
/// way the open fails with `Io`, nothing else changed, and the next open
/// recovers the fixture's state (layout 2: with its history id).
#[test]
fn a_failed_marker_upgrade_fails_the_open_and_the_next_one_finishes_it() {
    use common::{Action, Call, Rule, TestFs, When};
    for version in [1, 2, 3] {
        for when in [When::Before, When::Midway, When::After] {
            let dir = tempfile::tempdir().unwrap();
            copy_dir(&fixture(version).join("store"), dir.path());
            let fs = TestFs::default();
            fs.add(Rule::new(Call::WriteAtomic, when, Action::Fail).path("IWDB"));
            let error = Store::open_with(fs, dir.path(), options(2)).unwrap_err();
            assert!(matches!(error, iwdb::Error::Io { .. }), "{} {:?}: {}", version, when, error);
            let marker = read_marker(dir.path()).unwrap().unwrap();
            let expected = if when == When::After { LAYOUT_VERSION } else { version };
            assert_eq!(marker.version, expected, "{} {:?}", version, when);

            let store = Store::open(dir.path(), options(2)).unwrap();
            let text = fs::read_to_string(fixture(version).join("expected.txt")).unwrap();
            let (state, rest) = text.split_at(text.find("history ").unwrap_or(text.len()));
            assert_eq!(store.read(describe), state);
            if let Some(history) = rest.trim().strip_prefix("history ") {
                assert_eq!(store.history(), history.parse::<HistoryId>().unwrap());
            }
            drop(store);
            assert_eq!(read_marker(dir.path()).unwrap().unwrap().version, LAYOUT_VERSION);
        }
    }
}

/// The layout 4 fixture: every namespace comes back with its state and
/// keys, the dropped one stays dropped, and its id isn't reused.
#[test]
fn the_v4_fixture_opens_with_all_its_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture(4).join("store"), dir.path());
    let before = snapshot(&fixture(4));
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(snapshot(&fixture(4)), before, "the fixture itself is unchanged");
    let expected = fs::read_to_string(fixture(4).join("expected.txt")).unwrap();
    // Split into the default's part and one part per `namespace` line
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut history = String::new();
    for line in expected.split_inclusive('\n') {
        if let Some(h) = line.strip_prefix("history ") {
            history = h.trim().to_owned();
            parts.insert(0, ("default".into(), String::new()));
            // The lines before it were the default's
            let before = expected[..expected.find("history ").unwrap()].to_owned();
            parts[0].1 = before;
        } else if let Some(rest) = line.strip_prefix("namespace ") {
            parts.push((rest.split(' ').next().unwrap().to_owned(), String::new()));
        } else if line.starts_with("dropped ") {
        } else if parts.len() > 1 {
            parts.last_mut().unwrap().1.push_str(line);
        }
    }
    assert_eq!(history.parse::<HistoryId>().unwrap(), store.history());
    let names: Vec<_> = store.namespaces().iter().map(|n| n.name.to_string()).collect();
    assert_eq!(names, ["default", "people", "scratch"]);
    for (name, text) in &parts {
        assert_eq!(&store.namespace(name).unwrap().read(describe), text, "{}", name);
    }
    // The key table of `people`, from its checkpoint and its WAL
    let people = store.namespace("people").unwrap();
    for key in ["person-0", "person-8"] {
        assert!(people.read(|ns| ns.keys().get(&iwdb::IdempotencyKey::new(key).unwrap()).is_some()), "{}", key);
    }
    // The constraint and the index are live
    let duplicate = iwdb::Mutation::UpsertNode {
        id: "other".into(),
        labels: vec!["Person".into()],
        attr: [("email".to_owned(), iwdb::Value::from("p1@example.org"))].into(),
        meta: Default::default(),
        expected_version: None,
    };
    assert!(people.commit(&[duplicate]).is_err());
    assert_eq!(people.status().indexes.len(), 1);
    // Keys of namespace operations answer after the reopen, also for the dropped one
    let key = |k: &str| iwdb::IdempotencyKey::new(k).unwrap();
    let again = store.create_namespace("gone", Some(&key("mk-gone"))).unwrap();
    assert!(again.deduplicated);
    assert!(store.drop_namespace("gone", Some(&key("rm-gone"))).unwrap().deduplicated);
    assert!(store.namespace("gone").is_err());
    // Ids aren't reused
    let next = store.create_namespace("fresh", None).unwrap();
    assert_eq!(next.event.id, 5);
    assert_eq!(store.recovery().upgraded_from, None);
}
