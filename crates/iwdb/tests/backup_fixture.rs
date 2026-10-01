//! Compatibility fixture for the backup manifest (design rule 4):
//! `tests/fixtures/backup-v1/` is a backup written by step 7, with two
//! checkpoints and WAL after them. Every later version must verify it,
//! read its manifest as `expected.txt` says, and restore it to the state
//! there (`pitr.rs` restores it). A new manifest version gets a new
//! fixture next to this one, written by:
//!
//! ```text
//! cargo test -p iwdb --test backup_fixture -- --ignored generate_fixture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{verify, Kind, Namespace, Store};
use iwdb_storage::backup::{read_manifest, MANIFEST_VERSION};
use iwdb_storage::layout::BACKUP_NAME;
use support::{options, pad, reference, run, snapshot, workload};

fn fixture(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/backup-v{}", version))
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
    let dir = fixture(MANIFEST_VERSION);
    if dir.exists() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let steps = workload(50, 6_000);
    let store = Store::open(&work.path().join("store"), options(2)).unwrap();
    run(&store, &mut reference, &steps[..20]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[20..35]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[35..]);
    run(&store, &mut reference, &[pad(1000)]);
    let report = store.backup(&dir.join("backup")).unwrap();
    let manifest = read_manifest(&dir.join("backup").join(BACKUP_NAME)).unwrap();
    let mut expected = describe(&reference);
    expected += &format!(
        "history {}\nbackup seq {} time {:?} files {}\n",
        report.history,
        manifest.namespaces[0].seq,
        manifest.namespaces[0].time.map(|t| t.0),
        manifest.files.len()
    );
    fs::write(dir.join("expected.txt"), expected).unwrap();
}

#[test]
fn the_v1_backup_verifies_and_its_manifest_reads() {
    let dir = fixture(1).join("backup");
    let before = snapshot(&dir);
    let report = verify(&dir).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!(report.kind, Kind::Backup);
    let manifest = read_manifest(&dir.join(BACKUP_NAME)).unwrap();
    let expected = fs::read_to_string(fixture(1).join("expected.txt")).unwrap();
    let line = expected.lines().find(|l| l.starts_with("backup seq")).unwrap();
    let described = format!(
        "backup seq {} time {:?} files {}",
        manifest.namespaces[0].seq,
        manifest.namespaces[0].time.map(|t| t.0),
        manifest.files.len()
    );
    assert_eq!(line, described);
    let history = expected.lines().find_map(|l| l.strip_prefix("history ")).unwrap();
    assert_eq!(manifest.history.to_string(), history);
    assert_eq!(report.seq, Some(manifest.namespaces[0].seq));
    assert_eq!(snapshot(&dir), before);
}
