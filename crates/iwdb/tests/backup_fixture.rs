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

use iwdb::{Kind, Namespace, Store, verify};
use iwdb_storage::backup::{MANIFEST_VERSION, read_manifest};
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

/// Layout 4 / manifest 2: three namespaces (`default`, `a` with two
/// checkpoints and WAL after them, `b` with commits only). `expected.txt`:
/// the history, then per namespace a `namespace <name> <id> seq <seq> time
/// <time>` line followed by its description, and `files <n>`.
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
    store.create_namespace("a", None).unwrap();
    store.create_namespace("b", None).unwrap();
    let mut ref_a = reference_ns("a");
    let mut ref_b = reference_ns("b");
    let steps = workload(30, 6_500);
    run_in(&store.namespace("a").unwrap(), &mut ref_a, &steps[..12]);
    store.namespace("a").unwrap().checkpoint().unwrap();
    run_in(&store.namespace("a").unwrap(), &mut ref_a, &steps[12..]);
    run_in(&store.namespace("b").unwrap(), &mut ref_b, &workload(8, 6_800));
    let report = store.backup(&dir.join("backup")).unwrap();
    let manifest = read_manifest(&dir.join("backup").join(BACKUP_NAME)).unwrap();
    let mut expected = format!("history {}\n", report.history);
    for (name, ns) in [("default", &reference), ("a", &ref_a), ("b", &ref_b)] {
        let m = manifest.namespaces.iter().find(|m| m.name.as_str() == name).unwrap();
        expected +=
            &format!("namespace {} {} seq {} time {:?}\n{}", name, m.id, m.seq, m.time.map(|t| t.0), describe(ns));
    }
    expected += &format!("files {}\n", manifest.files.len());
    fs::write(dir.join("expected.txt"), expected).unwrap();
}

fn reference_ns(name: &str) -> Namespace {
    Namespace::new(iwdb::NamespaceName::new(name).unwrap())
}

fn run_in(ns: &iwdb::Ns<'_, iwdb::StdFs>, reference: &mut Namespace, steps: &[support::Step]) {
    for step in steps {
        match step {
            support::Step::Tx(m) => {
                if let Ok(result) = ns.commit(m) {
                    reference.commit(m).unwrap();
                    assert_eq!(result.seq, reference.seq());
                }
            }
            support::Step::Catalog(c) => {
                if let Ok(result) = ns.commit_catalog(c.clone()) {
                    reference.commit_catalog(c.clone()).unwrap();
                    assert_eq!(result.seq, reference.seq());
                }
            }
        }
    }
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

/// The layout 4 backup: verifies, its manifest lists the namespaces, and a
/// restore gives each namespace's state.
#[test]
fn the_v2_backup_verifies_its_manifest_reads_and_it_restores() {
    let dir = fixture(2).join("backup");
    // Git doesn't keep empty directories, and a placeholder file would be a
    // file the manifest doesn't list: namespace `b` has no checkpoint yet
    fs::create_dir_all(dir.join("ns/00000000000000000003/checkpoints")).unwrap();
    let before = snapshot(&dir);
    let report = verify(&dir).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!((report.kind, report.namespaces.len()), (Kind::Backup, 3));
    let manifest = read_manifest(&dir.join(BACKUP_NAME)).unwrap();
    let expected = fs::read_to_string(fixture(2).join("expected.txt")).unwrap();
    assert_eq!(manifest.history.to_string(), expected.lines().next().unwrap().strip_prefix("history ").unwrap());
    assert_eq!(format!("files {}", manifest.files.len()), expected.lines().last().unwrap());
    // Each namespace's section: its line, then its description
    let sections: Vec<&str> = expected.split("namespace ").skip(1).collect();
    assert_eq!(sections.len(), 3);
    let work = tempfile::tempdir().unwrap();
    let sources = iwdb::RestoreSources { backup: Some(dir.clone()), archive: None };
    let dest = work.path().join("restored");
    let restored = iwdb::restore(&dest, &sources, iwdb::RestoreTarget::Latest).unwrap();
    let store = Store::open(&dest, options(1)).unwrap();
    for section in sections {
        let (head, body) = section.split_once('\n').unwrap();
        let body = body.strip_suffix(&format!("files {}\n", manifest.files.len())).unwrap_or(body);
        let words: Vec<&str> = head.split(' ').collect();
        let (name, id, seq) = (words[0], words[1].parse::<u64>().unwrap(), words[3].parse::<u64>().unwrap());
        let m = manifest.namespaces.iter().find(|m| m.name.as_str() == name).unwrap();
        assert_eq!((m.id, m.seq), (id, seq), "{}", name);
        assert_eq!(restored.namespace(name).unwrap().seq, seq);
        assert_eq!(store.namespace(name).unwrap().read(describe), body, "{}", name);
    }
    assert_eq!(snapshot(&dir), before);
}
