//! Compatibility fixture for the WAL archive (design rule 4):
//! `tests/fixtures/archive-v1/` is an archive written by step 7, holding a
//! store's history from seq 1. Every later version must verify it, read
//! its history, and restore from it to the state in `expected.txt`
//! (`pitr.rs` restores it). A new archive version gets a new fixture next
//! to this one, written by:
//!
//! ```text
//! cargo test -p iwdb --test archive_fixture -- --ignored generate_fixture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{verify, Kind, Namespace, Store, StoreOptions};
use iwdb_storage::archive::{read_archive_marker, ARCHIVE_VERSION};
use support::{options, reference, run, snapshot, workload};

fn fixture(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/archive-v{}", version))
}

/// The state as text, as in `data_dir_fixture.rs`.
fn describe(ns: &Namespace) -> String {
    let mut out = String::new();
    for line in iwdb_engine::testutil::canonical(ns.graph()) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("catalog {:?}\nseq {}\n", ns.catalog(), ns.seq()));
    out
}

#[test]
#[ignore = "writes the fixture"]
fn generate_fixture() {
    let dir = fixture(ARCHIVE_VERSION);
    if dir.exists() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let opts = StoreOptions { archive: Some(dir.join("archive")), ..options(1) };
    let store = Store::open(&work.path().join("store"), opts).unwrap();
    let mut reference = reference();
    for chunk in workload(60, 7_000).chunks(15) {
        run(&store, &mut reference, chunk);
        store.checkpoint().unwrap();
    }
    let history = store.history();
    drop(store);
    // The state at the end of the archive (its last record)
    let end = iwdb_storage::read_log(&dir.join("archive"), 1).unwrap().1.next_seq - 1;
    let mut at_end = support::reference();
    for record in iwdb_storage::read_log(&dir.join("archive"), 1).unwrap().0 {
        at_end.replay(record, None).unwrap();
    }
    assert_eq!(at_end.seq(), end);
    fs::remove_file(dir.join("archive").join("LOCK")).unwrap();
    fs::write(dir.join("expected.txt"), describe(&at_end) + &format!("history {}\n", history)).unwrap();
}

#[test]
fn the_v1_archive_verifies_and_reads() {
    let dir = fixture(1).join("archive");
    let before = snapshot(&dir);
    let report = verify(&dir).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!((report.kind, report.first_seq), (Kind::Archive, Some(1)));
    let expected = fs::read_to_string(fixture(1).join("expected.txt")).unwrap();
    let history = expected.lines().find_map(|l| l.strip_prefix("history ")).unwrap();
    assert_eq!(read_archive_marker(&dir).unwrap().unwrap().to_string(), history);
    let seq = expected.lines().find_map(|l| l.strip_prefix("seq ")).unwrap();
    assert_eq!(report.last_seq, Some(seq.parse().unwrap()));
    assert_eq!(snapshot(&dir), before);
}
