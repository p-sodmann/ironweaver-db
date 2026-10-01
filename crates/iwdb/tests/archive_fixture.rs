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

/// Format 2: `default`, `a`, and `gone` (created, filled and dropped), each
/// with its segments under `ns/<id>/`, and the archive's copy of the
/// namespace log. `expected.txt`: the history, then per namespace a
/// `namespace <name> <id> last <seq>` line and the description of its
/// state at the end of its archived records.
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
    store.create_namespace("a", None).unwrap();
    store.create_namespace("gone", None).unwrap();
    for name in ["a", "gone"] {
        let ns = store.namespace(name).unwrap();
        for chunk in workload(40, 7_500).chunks(10) {
            for step in chunk {
                if let support::Step::Tx(m) = step {
                    let _ = ns.commit(m);
                }
            }
            ns.checkpoint().unwrap();
        }
    }
    store.drop_namespace("gone", None).unwrap();
    let history = store.history();
    let ids: Vec<(String, u64)> = vec![("default".into(), 1), ("a".into(), 2), ("gone".into(), 3)];
    drop(store);
    let mut expected = format!("history {}\n", history);
    for (name, id) in ids {
        let segments = dir.join("archive").join(format!("ns/{:020}", id));
        let (records, _) = iwdb_storage::read_log(&segments, 1).unwrap();
        let mut at_end = reference_ns(&name);
        for record in records {
            at_end.replay(record, None).unwrap();
        }
        expected += &format!("namespace {} {} last {}\n{}", name, id, at_end.seq(), describe(&at_end));
    }
    fs::remove_file(dir.join("archive").join("LOCK")).unwrap();
    fs::write(dir.join("expected.txt"), expected).unwrap();
}

fn reference_ns(name: &str) -> Namespace {
    Namespace::new(iwdb::NamespaceName::new(name).unwrap())
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

/// The format 2 archive: verifies, has the three namespaces' segments and
/// the log, and restores (to the latest: what the archive reaches, for the
/// namespaces that exist at its end).
#[test]
fn the_v2_archive_verifies_and_restores() {
    let dir = fixture(2).join("archive");
    let before = snapshot(&dir);
    let report = verify(&dir).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!((report.kind, report.namespaces.len()), (Kind::Archive, 3));
    let expected = fs::read_to_string(fixture(2).join("expected.txt")).unwrap();
    let history = expected.lines().next().unwrap().strip_prefix("history ").unwrap();
    assert_eq!(read_archive_marker(&dir).unwrap().unwrap().to_string(), history);
    let work = tempfile::tempdir().unwrap();
    let sources = iwdb::RestoreSources { backup: None, archive: Some(dir.clone()) };
    let dest = work.path().join("restored");
    let restored = iwdb::restore(&dest, &sources, iwdb::RestoreTarget::Latest).unwrap();
    // `gone` was dropped before the end of the log
    assert_eq!(restored.namespaces.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["default", "a"]);
    let store = Store::open(&dest, options(1)).unwrap();
    for section in expected.split("namespace ").skip(1) {
        let (head, body) = section.split_once('\n').unwrap();
        let words: Vec<&str> = head.split(' ').collect();
        if words[0] == "gone" {
            assert!(store.namespace("gone").is_err());
            continue;
        }
        assert_eq!(restored.namespace(words[0]).unwrap().seq, words[3].parse::<u64>().unwrap());
        assert_eq!(store.namespace(words[0]).unwrap().read(describe), body, "{}", words[0]);
    }
    assert_eq!(snapshot(&dir), before);
}
