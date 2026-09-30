//! Compatibility fixture for data directory layout 1 and its checkpoint
//! format (design rule 4): `tests/fixtures/data-dir-v1/` is a data
//! directory written by step 5, with a checkpoint and WAL records after
//! it. Every later version must open it and recover the state in
//! `expected.txt`. Regenerate only when the layout version is bumped (and
//! then keep this one as the N-1 fixture):
//!
//! ```text
//! cargo test -p iwdb --test data_dir_fixture -- --ignored generate_fixture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
#[path = "../../iwdb-engine/tests/workload/mod.rs"]
mod workload;

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{Namespace, Store};
use support::{options, pad, reference, run, workload};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data-dir-v1")
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

#[test]
#[ignore = "writes the fixture"]
fn generate_fixture() {
    let dir = fixture();
    let _ = fs::remove_dir_all(&dir);
    let mut reference = reference();
    let steps = workload(40, 5_000);
    let store = Store::open(&dir.join("store"), options(2)).unwrap();
    run(&store, &mut reference, &steps[..40]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[40..]);
    run(&store, &mut reference, &[pad(1000)]);
    // No close: the WAL holds records after the checkpoint
    drop(store);
    fs::write(dir.join("expected.txt"), describe(&reference)).unwrap();
}

#[test]
fn the_v1_fixture_opens_and_recovers_its_state() {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture().join("store"), dir.path());
    let store = Store::open(dir.path(), options(2)).unwrap();
    let expected = fs::read_to_string(fixture().join("expected.txt")).unwrap();
    assert_eq!(store.read(describe), expected);
    let report = store.recovery();
    assert!(report.checkpoint.is_some());
    assert!(report.replayed > 0, "the fixture has WAL records after its checkpoint");
    assert!(report.skipped_checkpoints.is_empty() && report.torn_tail.is_none());
    assert!(report.index_changes.created.is_empty() && report.index_changes.dropped.is_empty());
}
