//! The data directory and its lock: a second open fails, the lock is
//! released on close, drop and process exit (also `kill -9`), and
//! directories that aren't ours, or are newer, are refused unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
#[path = "../../iwdb-engine/tests/workload/mod.rs"]
mod workload;

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use iwdb::{Error, Store};
use iwdb_storage::layout::{encode_marker, LAYOUT_VERSION, LOCK_NAME, MARKER_NAME};
use support::{options, pad, reference, run, snapshot, state, store_state};

#[test]
fn a_new_directory_gets_the_layout() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("a").join("b");
    Store::open(&dir, options(2)).unwrap().close().unwrap();
    let mut names: Vec<String> =
        fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["IWDB", "LOCK", "checkpoints", "wal"]);
    assert_eq!(fs::read(dir.join(MARKER_NAME)).unwrap(), encode_marker(LAYOUT_VERSION));
    assert_eq!(&fs::read(dir.join(MARKER_NAME)).unwrap()[..8], b"IWDBDIR\n");
}

#[test]
fn a_second_open_fails_until_the_first_is_closed_or_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &[pad(0)]);
    match Store::open(dir.path(), options(2)) {
        Err(Error::Locked { path }) => assert_eq!(path, dir.path().join(LOCK_NAME)),
        other => panic!("{:?}", other),
    }
    // The failed open changed nothing in the open store
    run(&store, &mut reference, &[pad(1)]);
    store.close().unwrap();

    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::Locked { .. })));
    drop(store);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
}

/// A store on another thread holds the lock as well.
#[test]
fn the_lock_holds_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let path = dir.path().to_path_buf();
    let other = std::thread::spawn(move || matches!(Store::open(&path, options(2)), Err(Error::Locked { .. })));
    assert!(other.join().unwrap());
    drop(store);
}

/// Run by `the_lock_is_released_when_the_process_dies` in a child process:
/// open the store named in the environment, report it, and wait.
#[test]
fn child_holds_the_lock() {
    let Ok(dir) = std::env::var("IWDB_LOCK_CHILD_DIR") else { return };
    let store = Store::open(dir.as_ref(), options(2)).unwrap();
    store.commit(&[pad_mutation()]).unwrap();
    println!("locked");
    std::thread::sleep(std::time::Duration::from_secs(60));
    drop(store);
}

fn pad_mutation() -> iwdb::Mutation {
    match pad(0) {
        workload::Step::Tx(mut m) => m.remove(0),
        workload::Step::Catalog(_) => unreachable!(),
    }
}

#[test]
fn the_lock_is_released_when_the_process_dies() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_holds_the_lock", "--nocapture", "--test-threads=1"])
        .env("IWDB_LOCK_CHILD_DIR", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let locked = BufReader::new(stdout).lines().map_while(Result::ok).any(|line| line.contains("locked"));
    assert!(locked, "the child opened the store");
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::Locked { .. })));

    // SIGKILL on Unix: no destructor runs
    child.kill().unwrap();
    child.wait().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store.seq(), 1, "the child's acknowledged commit survived");
}

#[test]
fn directories_that_are_not_ours_are_refused_unchanged() {
    // Some other file in it
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
    let before = snapshot(dir.path());
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::NotADataDir { .. })));
    assert_eq!(snapshot(dir.path()), before, "not even a LOCK file was created");

    // A marker that isn't ours
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(MARKER_NAME), b"something else").unwrap();
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::NotADataDir { .. })));

    // A file, not a directory
    let file = dir.path().join("file");
    fs::write(&file, b"x").unwrap();
    assert!(matches!(Store::open(&file, options(2)), Err(Error::NotADataDir { .. })));

    // Missing, and not to be created
    let missing = dir.path().join("missing");
    let no_create = iwdb::StoreOptions { create_if_missing: false, ..options(2) };
    assert!(matches!(Store::open(&missing, no_create.clone()), Err(Error::NotADataDir { .. })));
    assert!(!missing.exists());
    let empty = tempfile::tempdir().unwrap();
    assert!(matches!(Store::open(empty.path(), no_create), Err(Error::NotADataDir { .. })));
}

#[test]
fn a_newer_layout_or_a_damaged_marker_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), options(2)).unwrap().close().unwrap();
    let marker = dir.path().join(MARKER_NAME);

    fs::write(&marker, encode_marker(LAYOUT_VERSION + 1)).unwrap();
    let before = snapshot(dir.path());
    match Store::open(dir.path(), options(2)) {
        Err(Error::UnsupportedLayout { version, .. }) => assert_eq!(version, 2),
        other => panic!("{:?}", other),
    }
    assert_eq!(snapshot(dir.path()), before);

    let mut damaged = encode_marker(LAYOUT_VERSION);
    damaged[9] ^= 1;
    fs::write(&marker, damaged).unwrap();
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::InvalidDataDir { .. })));

    fs::write(&marker, encode_marker(LAYOUT_VERSION)).unwrap();
    fs::remove_dir_all(dir.path().join("wal")).unwrap();
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::InvalidDataDir { .. })));
}

/// An initialization interrupted before the marker was written leaves only
/// our own entries; the next open finishes it.
#[test]
fn an_interrupted_initialization_is_finished() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("wal")).unwrap();
    fs::write(dir.path().join(LOCK_NAME), b"").unwrap();
    fs::write(dir.path().join(".IWDB.1.0.tmp"), b"half").unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert!(store.recovery().created);
    assert!(!dir.path().join(".IWDB.1.0.tmp").exists());
}
