//! `verify` on a layout 4 store and backup with damaged or inconsistent
//! namespaces (ADR 0017): each kind of damage is a problem naming
//! the namespace, and what a crash leaves is a note.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{Mutation, Store, Value, VerifyReport, verify};
use support::options;

const A: &str = "ns/00000000000000000002";
const B: &str = "ns/00000000000000000003";

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn put(i: i64) -> Mutation {
    Mutation::UpsertNode {
        id: format!("n{}", i % 5),
        labels: vec!["L".into()],
        attr: [("v".to_owned(), Value::Int(i))].into(),
        meta: Default::default(),
        expected_version: None,
    }
}

/// A closed store with namespaces default, a (id 2) and b (id 3), commits
/// and a checkpoint in each, and a backup of it.
fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let data = root.join("data");
    let store = Store::open(&data, options(1)).unwrap();
    store.create_namespace("a", None).unwrap();
    store.create_namespace("b", None).unwrap();
    for n in ["default", "a", "b"] {
        let ns = store.namespace(n).unwrap();
        for i in 0..8 {
            ns.commit(&[put(i)]).unwrap();
        }
        ns.checkpoint().unwrap();
        for i in 8..11 {
            ns.commit(&[put(i)]).unwrap();
        }
    }
    let backup = root.join("backup");
    store.backup(&backup).unwrap();
    store.close().unwrap();
    (data, backup)
}

/// A copy of `from` to damage.
fn copy_of(root: &Path, from: &Path, name: &str) -> PathBuf {
    let to = root.join(name);
    copy_dir(from, &to);
    to
}

fn problems(report: &VerifyReport) -> Vec<String> {
    report.problems.iter().map(|p| p.message.clone()).collect()
}

fn first_file(dir: &Path) -> PathBuf {
    let mut files: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    files.remove(0)
}

fn flip(path: &Path, at: usize) {
    let mut bytes = fs::read(path).unwrap();
    let at = at % bytes.len();
    bytes[at] ^= 0x40;
    fs::write(path, bytes).unwrap();
}

#[test]
fn an_intact_store_and_backup_verify() {
    let work = tempfile::tempdir().unwrap();
    let (data, backup) = fixture(work.path());
    for dir in [&data, &backup] {
        let report = verify(dir).unwrap();
        assert!(report.is_ok(), "{:#?}", report);
        assert_eq!(report.namespaces.len(), 3);
        assert!(report.namespaces.iter().all(|n| n.seq == Some(11)), "{:#?}", report.namespaces);
        assert_eq!(report.seq, None, "several namespaces: no one seq");
    }
}

#[test]
fn damage_in_one_namespace_names_it() {
    let work = tempfile::tempdir().unwrap();
    let (data, _) = fixture(work.path());
    // A flipped byte in b's checkpoint
    let dir = copy_of(work.path(), &data, "flipped");
    flip(&first_file(&dir.join(B).join("checkpoints")), 200);
    let report = verify(&dir).unwrap();
    assert!(!report.is_ok());
    assert!(problems(&report).iter().any(|p| p.contains("namespace 'b'")), "{:#?}", report.problems);
    assert!(!problems(&report).iter().any(|p| p.contains("namespace 'a'")), "{:#?}", report.problems);
    // A flipped byte in a's WAL
    let dir = copy_of(work.path(), &data, "wal-flipped");
    flip(&first_file(&dir.join(A).join("wal")), 80);
    let report = verify(&dir).unwrap();
    assert!(problems(&report).iter().any(|p| p.contains("namespace 'a'")), "{:#?}", report.problems);
    // A missing WAL directory, a missing checkpoints directory, a missing namespace
    for (name, remove) in
        [("no-wal", format!("{}/wal", B)), ("no-checkpoints", format!("{}/checkpoints", A)), ("no-ns", B.to_owned())]
    {
        let dir = copy_of(work.path(), &data, name);
        fs::remove_dir_all(dir.join(&remove)).unwrap();
        let report = verify(&dir).unwrap();
        assert!(!report.is_ok(), "{}", name);
        assert!(
            problems(&report).iter().any(|p| p.contains("missing") || p.contains("namespace")),
            "{}: {:#?}",
            name,
            report
        );
        // A store refuses it too, rather than recover less
        assert!(Store::open(&dir, options(1)).is_err(), "{}", name);
    }
    // The untouched namespaces still verify
    let report = verify(&data).unwrap();
    assert!(report.is_ok());
}

#[test]
fn the_namespace_log_and_the_directories_must_agree() {
    let work = tempfile::tempdir().unwrap();
    let (data, _) = fixture(work.path());
    // A directory the log doesn't list, with data: damage (a lost create event)
    let dir = copy_of(work.path(), &data, "orphan-data");
    copy_dir(&dir.join(A), &dir.join("ns/00000000000000000009"));
    let report = verify(&dir).unwrap();
    assert!(problems(&report).iter().any(|p| p.contains("holds data")), "{:#?}", report.problems);
    assert!(Store::open(&dir, options(1)).is_err());
    // An empty one is what an interrupted create leaves: a note, and the next open removes it
    let dir = copy_of(work.path(), &data, "orphan-empty");
    fs::create_dir_all(dir.join("ns/00000000000000000009/wal")).unwrap();
    fs::create_dir_all(dir.join("ns/00000000000000000009/checkpoints")).unwrap();
    let report = verify(&dir).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert!(report.notes.iter().any(|n| n.message.contains("the log doesn't list")), "{:#?}", report.notes);
    drop(Store::open(&dir, options(1)).unwrap());
    assert!(!dir.join("ns/00000000000000000009").exists());
    // A damaged event in the middle of the log: damage, and no open
    let dir = copy_of(work.path(), &data, "log-flipped");
    flip(&dir.join("NAMESPACES"), 16 + 30);
    let report = verify(&dir).unwrap();
    assert!(!report.is_ok(), "{:#?}", report);
    assert!(Store::open(&dir, options(1)).is_err());
    // A damaged header
    let dir = copy_of(work.path(), &data, "log-header");
    flip(&dir.join("NAMESPACES"), 3);
    assert!(!verify(&dir).unwrap().is_ok());
    // A missing log
    let dir = copy_of(work.path(), &data, "log-missing");
    fs::remove_file(dir.join("NAMESPACES")).unwrap();
    assert!(!verify(&dir).unwrap().is_ok());
    // A torn tail: an event that was never acknowledged. A note; the next
    // open cuts it (the create of "b" is the last event, so b's dir is
    // then an empty-or-not orphan: with data it must refuse, never delete)
    let dir = copy_of(work.path(), &data, "log-torn");
    let log = dir.join("NAMESPACES");
    let mut bytes = fs::read(&log).unwrap();
    bytes.extend_from_slice(&[7, 0, 0]);
    fs::write(&log, bytes).unwrap();
    let report = verify(&dir).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert!(report.notes.iter().any(|n| n.message.contains("torn tail")), "{:#?}", report.notes);
    drop(Store::open(&dir, options(1)).unwrap());
    let notes = verify(&dir).unwrap().notes;
    assert!(!notes.iter().any(|n| n.message.contains("torn tail")), "{:#?}", notes);
}

#[test]
fn a_backup_must_hold_what_its_manifest_lists() {
    let work = tempfile::tempdir().unwrap();
    let (_, backup) = fixture(work.path());
    let dir = copy_of(work.path(), &backup, "no-ns");
    fs::remove_dir_all(dir.join(A)).unwrap();
    let report = verify(&dir).unwrap();
    assert!(!report.is_ok(), "{:#?}", report);
    let dir = copy_of(work.path(), &backup, "log-missing");
    fs::remove_file(dir.join("NAMESPACES")).unwrap();
    assert!(!verify(&dir).unwrap().is_ok());
    let dir = copy_of(work.path(), &backup, "wal-short");
    let segment = first_file(&dir.join(B).join("wal"));
    let len = fs::metadata(&segment).unwrap().len();
    fs::OpenOptions::new().write(true).open(&segment).unwrap().set_len(len - 30).unwrap();
    let report = verify(&dir).unwrap();
    assert!(!report.is_ok(), "{:#?}", report);
    assert!(problems(&report).iter().any(|p| p.contains("namespace 'b'")), "{:#?}", report.problems);
}
