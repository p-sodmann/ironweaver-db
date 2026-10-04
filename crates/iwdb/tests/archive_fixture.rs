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

use iwdb::{Kind, Namespace, Store, StoreOptions, verify};
use iwdb_storage::archive::{ARCHIVE_VERSION, read_archive_marker};
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

/// Format 3: `default`, `a`, and `gone` (created, filled and dropped), each
/// with its segments under `ns/<id>/`, `imported` (made by an import: its
/// checkpoint at seq 1 in `ns/4/`, then segments from seq 2), and the
/// archive's copy of the namespace log. `expected.txt`: the history, then
/// per namespace a `namespace <name> <id> last <seq>` line and the
/// description of its state at the end of its archived records. (Format 2
/// was the same without `imported`.)
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
    let lgf = "@nodes\nlabel name size\na Ann 1\nb \"Bo B\" 2.5\n@arcs\n\t\tw\na b 3\nb a 4\n";
    store.import_namespace("imported", iwdb::import::ImportFormat::Lgf, lgf.as_bytes(), None).unwrap();
    let imported = store.namespace("imported").unwrap();
    for chunk in workload(30, 7_700).chunks(10) {
        for step in chunk {
            if let support::Step::Tx(m) = step {
                let _ = imported.commit(m);
            }
        }
        imported.checkpoint().unwrap();
    }
    let history = store.history();
    let ids: Vec<(String, u64)> =
        vec![("default".into(), 1), ("a".into(), 2), ("gone".into(), 3), ("imported".into(), 4)];
    drop(store);
    let mut expected = format!("history {}\n", history);
    for (name, id) in ids {
        let segments = dir.join("archive").join(format!("ns/{:020}", id));
        let base = segments.join(format!("{:020}.ckpt", 1));
        let mut at_end = if base.is_file() {
            let name = iwdb::NamespaceName::new(name.as_str()).unwrap();
            Namespace::from_loaded(iwdb_storage::checkpoint::load_checkpoint(&base, 1, &name).unwrap())
        } else {
            reference_ns(&name)
        };
        let (records, _) = iwdb_storage::read_log(&segments, at_end.seq() + 1).unwrap();
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

/// The format 3 archive: the four namespaces, the imported one restored
/// from its archived checkpoint and the segments after it.
#[test]
fn the_v3_archive_verifies_and_restores() {
    let dir = fixture(3).join("archive");
    let before = snapshot(&dir);
    let report = verify(&dir).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!((report.kind, report.namespaces.len()), (Kind::Archive, 4));
    let expected = fs::read_to_string(fixture(3).join("expected.txt")).unwrap();
    let work = tempfile::tempdir().unwrap();
    let sources = iwdb::RestoreSources { backup: None, archive: Some(dir.clone()) };
    let dest = work.path().join("restored");
    let restored = iwdb::restore(&dest, &sources, iwdb::RestoreTarget::Latest).unwrap();
    assert_eq!(restored.namespaces.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["default", "a", "imported"]);
    assert_eq!(restored.namespace("imported").unwrap().checkpoint, Some(1));
    let store = Store::open(&dest, options(1)).unwrap();
    for section in expected.split("namespace ").skip(1) {
        let (head, body) = section.split_once('\n').unwrap();
        let words: Vec<&str> = head.split(' ').collect();
        if words[0] == "gone" {
            continue;
        }
        assert_eq!(restored.namespace(words[0]).unwrap().seq, words[3].parse::<u64>().unwrap());
        assert_eq!(store.namespace(words[0]).unwrap().read(describe), body, "{}", words[0]);
    }
    assert_eq!(snapshot(&dir), before);
}

/// A store that archives into a format 2 archive upgrades it to format 3
/// by rewriting the marker; its files stay.
#[test]
fn a_v2_archive_is_upgraded_by_its_marker() {
    let work = tempfile::tempdir().unwrap();
    let copy = work.path().join("archive");
    copy_tree(&fixture(2).join("archive"), &copy);
    let history: iwdb::HistoryId = fs::read_to_string(fixture(2).join("expected.txt"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .strip_prefix("history ")
        .unwrap()
        .parse()
        .unwrap();
    let files = |dir: &Path| {
        let mut all = Vec::new();
        for ns in fs::read_dir(dir.join("ns")).unwrap().flatten() {
            all.extend(fs::read_dir(ns.path()).unwrap().flatten().map(|f| f.file_name()));
        }
        all.sort();
        all
    };
    let before = files(&copy);
    drop(iwdb_storage::archive::Archive::open(iwdb::StdFs, &copy, history).unwrap());
    let (version, found) = iwdb_storage::archive::read_archive_marker_info(&copy).unwrap().unwrap();
    assert_eq!((version, found), (ARCHIVE_VERSION, history));
    assert_eq!(files(&copy), before);
    assert!(verify(&copy).unwrap().is_ok());
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
