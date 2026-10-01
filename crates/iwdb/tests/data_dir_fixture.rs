//! Compatibility fixtures for the data directory layouts and their
//! checkpoint format (design rule 4). Each is a data directory with a
//! checkpoint and WAL records after it, and every later version must open
//! it and recover the state in `expected.txt`:
//!
//! - `tests/fixtures/data-dir-v1/`: layout 1, written by step 5 (WAL format
//!   1). Opening a copy upgrades it to layout 2;
//! - `tests/fixtures/data-dir-v2/`: layout 2, written by step 7 (WAL format
//!   2), with its history id in `expected.txt`.
//!
//! A new layout version gets a new fixture next to these, written by:
//!
//! ```text
//! cargo test -p iwdb --test data_dir_fixture -- --ignored generate_fixture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{HistoryId, Namespace, Store};
use iwdb_storage::layout::{read_marker, LAYOUT_VERSION, MARKER_NAME};
use support::{options, pad, reference, run, snapshot, workload};

fn fixture(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/data-dir-v{}", version))
}

/// The expected state as text: the canonical graph, the catalog and the seq.
fn describe(ns: &Namespace) -> String {
    let mut out = String::new();
    for line in iwdb_engine::testutil::canonical(ns.graph()) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("catalog {:?}\nseq {}\n", ns.catalog(), ns.seq()));
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
    run(&store, &mut reference, &steps[..40]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[40..]);
    run(&store, &mut reference, &[pad(1000)]);
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
    assert_eq!((marker.version, marker.history), (2, Some(history)));
    assert_eq!(fs::read(dir.path().join(MARKER_NAME)).unwrap().len(), 32);
    // Reopened, it is a layout 2 directory with the same history
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!((store.recovery().upgraded_from, store.history()), (None, history));
}

#[test]
fn the_v2_fixture_opens_and_recovers_its_state_and_history() {
    let (_dir, store, rest) = open_fixture(2);
    assert_eq!(store.recovery().upgraded_from, None);
    let history: HistoryId = rest.trim().strip_prefix("history ").unwrap().parse().unwrap();
    assert_eq!(store.history(), history);
}
