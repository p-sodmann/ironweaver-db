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
//!   is in the checkpoint's graph meta and in the WAL records.
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
#[test]
#[ignore = "writes the fixture"]
fn generate_fixture() {
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
    let history = store.history();
    // No close: the WAL holds records after the checkpoint
    drop(store);
    fs::write(dir.join("expected.txt"), describe(&reference) + &format!("history {}\n", history)).unwrap();
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
fn the_v3_fixture_opens_and_recovers_its_state_keys_and_history() {
    let (_dir, store, rest) = open_fixture(3);
    assert_eq!(store.recovery().upgraded_from, None);
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
    for version in [1, 2] {
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
