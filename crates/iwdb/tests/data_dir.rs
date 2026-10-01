//! The data directory and its lock: a second open fails, the lock is
//! released on close, drop and process exit (also `kill -9`), and
//! directories that aren't ours, or are newer, are refused unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use iwdb::{Error, Store};
use iwdb_storage::layout::{
    encode_marker, encode_marker_with, BACKUP_NAME, LAYOUT_VERSION, LOCK_NAME, MARKER_NAME, RESTORING_NAME,
};
use support::{options, pad, reference, run, snapshot, state, store_state};

#[test]
fn a_new_directory_gets_the_layout() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("a").join("b");
    let store = Store::open(&dir, options(2)).unwrap();
    let history = store.history();
    store.close().unwrap();
    let mut names: Vec<String> =
        fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["IWDB", "LOCK", "checkpoints", "wal"]);
    assert_eq!(fs::read(dir.join(MARKER_NAME)).unwrap(), encode_marker(history));
    assert_eq!(&fs::read(dir.join(MARKER_NAME)).unwrap()[..8], b"IWDBDIR\n");
    // The history stays the same across opens, and differs between directories
    let store = Store::open(&dir, options(2)).unwrap();
    assert_eq!(store.history(), history);
    assert!(store.recovery().upgraded_from.is_none());
    let other = Store::open(&parent.path().join("other"), options(2)).unwrap();
    assert_ne!(other.history(), history);
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
        support::Step::Tx(mut m) => m.remove(0),
        support::Step::Catalog(_) => unreachable!(),
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

    let good = fs::read(&marker).unwrap();
    fs::write(&marker, encode_marker_with(LAYOUT_VERSION + 1, b"anything a newer layout holds")).unwrap();
    let before = snapshot(dir.path());
    match Store::open(dir.path(), options(2)) {
        Err(Error::UnsupportedLayout { version, .. }) => assert_eq!(version, LAYOUT_VERSION + 1),
        other => panic!("{:?}", other),
    }
    assert_eq!(snapshot(dir.path()), before);

    let mut damaged = good.clone();
    damaged[9] ^= 1;
    fs::write(&marker, damaged).unwrap();
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::InvalidDataDir { .. })));

    fs::write(&marker, good).unwrap();
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

/// A store opens neither a backup (it is restored instead) nor a directory
/// that a restore was writing; both are refused unchanged.
#[test]
fn backups_and_interrupted_restores_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), options(2)).unwrap().close().unwrap();
    fs::write(dir.path().join(BACKUP_NAME), b"manifest").unwrap();
    let before = snapshot(dir.path());
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::IsBackup { .. })));
    assert_eq!(snapshot(dir.path()), before);

    // A restore that stopped before writing the marker
    let dir = tempfile::tempdir().unwrap();
    for sub in ["checkpoints", "wal"] {
        fs::create_dir(dir.path().join(sub)).unwrap();
    }
    fs::write(dir.path().join(RESTORING_NAME), b"").unwrap();
    let before = snapshot(dir.path());
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::InterruptedRestore { .. })));
    assert_eq!(snapshot(dir.path()), before);
    // and one that stopped after its checkpoint, but before the marker
    fs::remove_file(dir.path().join(RESTORING_NAME)).unwrap();
    fs::write(dir.path().join("checkpoints").join("00000000000000000005.ckpt"), b"x").unwrap();
    match Store::open(dir.path(), options(2)) {
        Err(Error::NotADataDir { reason, .. }) => assert!(reason.contains("non-empty 'checkpoints'"), "{}", reason),
        other => panic!("{:?}", other),
    }
}

/// A process that another thread spawns holds a copy of this process's
/// open files until it execs, the lock file included, so the lock of a
/// store that was just closed can look held for that moment. Opening must
/// not fail then (a step 5 bug, found in step 7: about 3.5% of reopens
/// failed with `Locked` while another thread spawned processes).
#[cfg(unix)]
#[test]
fn reopening_while_another_thread_spawns_processes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(2);
    opts.wal.fsync = iwdb::FsyncPolicy::Off;
    Store::open(dir.path(), opts.clone()).unwrap().close().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let spawner = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut spawned = 0;
            while !stop.load(Ordering::Relaxed) {
                Command::new("true").status().unwrap();
                spawned += 1;
            }
            spawned
        })
    };
    let mut locked = 0;
    for _ in 0..400 {
        match Store::open(dir.path(), opts.clone()) {
            Err(Error::Locked { .. }) => locked += 1,
            other => drop(other.unwrap()),
        }
        // A reader's shared lock, too
        if matches!(iwdb::verify(dir.path()), Err(Error::Locked { .. })) {
            locked += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    assert!(spawner.join().unwrap() > 100);
    assert_eq!(locked, 0);
}
