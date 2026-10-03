//! Continuous WAL archiving (ADR 0009): every segment the
//! checkpointer removes is durable in the archive first, so the archive
//! and the WAL together hold the whole history; archiving is idempotent;
//! a conflict, an archive of another history and every failing archive
//! write leave the WAL in place. Restoring from an archive is tested in
//! `pitr.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{verify, Error, FsyncPolicy, Kind, Namespace, Store, StoreOptions};
use iwdb_storage::archive::{Archive, ARCHIVE_MARKER_NAME};
use iwdb_storage::io::StdFs;
use iwdb_storage::{HistoryId, WalReader};
use support::{options, pad, reference, run, segment_seqs, state, store_state, workload, Step};
use tempfile::TempDir;

/// Options with an archive in `dir/archive`, one checkpoint kept.
fn archived(dir: &Path) -> StoreOptions {
    StoreOptions { archive: Some(dir.join("archive")), ..options(1) }
}

/// Commit `steps`, with a checkpoint after every 10.
fn commit_with_checkpoints<F: iwdb::LogFs + Clone + Send + Sync + 'static>(
    store: &Store<F>,
    reference: &mut Namespace,
    steps: &[Step],
) where
    F::File: Send,
{
    for chunk in steps.chunks(10) {
        run(store, reference, chunk);
        let _ = store.checkpoint();
    }
}

/// The segments of the archive and the WAL, as one log (the WAL's copy
/// where both have one).
fn whole_log(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut all: BTreeMap<u64, PathBuf> =
        iwdb_storage::list_segments(&dir.join("archive/ns/00000000000000000001")).unwrap().into_iter().collect();
    all.extend(iwdb_storage::list_segments(&dir.join("data").join("ns/00000000000000000001/wal")).unwrap());
    all.into_iter().collect()
}

/// The archive and the WAL together replay to the reference: no record
/// the checkpointer removed was lost.
fn assert_whole_history(dir: &Path, reference: &Namespace) {
    let mut ns = support::reference();
    for record in WalReader::from_segments(whole_log(dir), 1, u64::MAX).unwrap() {
        ns.replay(record.unwrap(), None).unwrap();
    }
    assert_eq!(state(&ns), state(reference));
}

#[test]
fn the_archive_and_the_wal_hold_the_whole_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data"), archived(dir.path())).unwrap();
    let mut reference = reference();
    commit_with_checkpoints(&store, &mut reference, &workload(80, 21));
    let archived_segments = segment_seqs_in(&dir.path().join("archive/ns/00000000000000000001"));
    assert!(archived_segments.len() > 5, "{:?}", archived_segments);
    assert_eq!(archived_segments[0], 1, "archived from the first segment");
    assert!(segment_seqs(&dir.path().join("data"))[0] > 1, "the WAL was cut");
    assert_whole_history(dir.path(), &reference);
    let report = verify(&dir.path().join("archive")).unwrap();
    assert!(report.is_ok(), "{:#?}", report);
    assert_eq!((report.kind, report.history, report.first_seq), (Kind::Archive, Some(store.history()), Some(1)));
    // Reopened, the store archives into the same directory
    store.close().unwrap();
    let store = Store::open(&dir.path().join("data"), archived(dir.path())).unwrap();
    commit_with_checkpoints(&store, &mut reference, &workload(30, 22));
    assert_whole_history(dir.path(), &reference);
}

fn segment_seqs_in(dir: &Path) -> Vec<u64> {
    iwdb_storage::list_segments(dir).unwrap().into_iter().map(|(s, _)| s).collect()
}

#[test]
fn an_archive_belongs_to_one_history() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(&dir.path().join("data"), archived(dir.path())).unwrap();
    // Another store can't use it: neither while the first holds it...
    let other = Store::open(&dir.path().join("other"), archived(dir.path()));
    assert!(matches!(other, Err(Error::Locked { .. })), "{:?}", other.err());
    drop(first);
    // ...nor afterwards: it belongs to the first store's history
    match Store::open(&dir.path().join("other"), archived(dir.path())) {
        Err(Error::ArchiveMismatch { expected, found, .. }) => assert_ne!(expected, found),
        other => panic!("{:?}", other.err()),
    }
    // A directory with other files isn't an archive
    let foreign = dir.path().join("foreign");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("notes.txt"), b"x").unwrap();
    let opts = StoreOptions { archive: Some(foreign.clone()), ..options(1) };
    assert!(matches!(Store::open(&dir.path().join("third"), opts), Err(Error::NotAnArchive { .. })));
    assert!(!foreign.join(ARCHIVE_MARKER_NAME).exists());
    // The marker and lock, opened directly
    let id = HistoryId::random();
    let archive = Archive::open(StdFs, &dir.path().join("direct"), id).unwrap();
    assert!(matches!(Archive::open(StdFs, &dir.path().join("direct"), id), Err(Error::Locked { .. })));
    drop(archive);
    Archive::open(StdFs, &dir.path().join("direct"), id).unwrap();
}

/// A crash between archiving and removal (here: the removal fails) leaves
/// segments in both places; the next checkpoint archives them again
/// (identical) and removes them.
#[test]
fn archiving_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let store = Store::open_with(fs.clone(), &dir.path().join("data"), archived(dir.path())).unwrap();
    let mut reference = reference();
    run(&store, &mut reference, &workload(30, 23));
    fs.add(Rule::new(Call::RemoveFile, When::Before, Action::Fail).path("/wal/"));
    assert!(store.checkpoint().is_err());
    let in_both: Vec<u64> = segment_seqs_in(&dir.path().join("archive/ns/00000000000000000001"))
        .into_iter()
        .filter(|s| segment_seqs(&dir.path().join("data")).contains(s))
        .collect();
    assert!(!in_both.is_empty());
    drop(store);
    let store = Store::open(&dir.path().join("data"), archived(dir.path())).unwrap();
    commit_with_checkpoints(&store, &mut reference, &workload(20, 24));
    assert!(store.checkpoint_failure().is_none(), "{:?}", store.checkpoint_failure());
    assert!(segment_seqs(&dir.path().join("data"))[0] > in_both[in_both.len() - 1]);
    assert_whole_history(dir.path(), &reference);
}

/// A segment of the same name with other contents in the archive: the
/// checkpoint fails, nothing is removed, and the store stays writable.
#[test]
fn a_conflicting_segment_in_the_archive_keeps_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data"), archived(dir.path())).unwrap();
    let mut reference = reference();
    run(&store, &mut reference, &workload(30, 25));
    fs::create_dir_all(dir.path().join("archive/ns/00000000000000000001")).unwrap();
    fs::write(
        dir.path().join("archive/ns/00000000000000000001").join(iwdb_storage::format::segment_name(1)),
        b"another history",
    )
    .unwrap();
    let before = segment_seqs(&dir.path().join("data"));
    match store.checkpoint() {
        Err(Error::ArchiveConflict { path }) => assert!(path.ends_with(iwdb_storage::format::segment_name(1))),
        other => panic!("{:?}", other),
    }
    assert_eq!(segment_seqs(&dir.path().join("data")), before);
    assert!(store.checkpoint_failure().unwrap().contains("differs"));
    run(&store, &mut reference, &[pad(1)]);
    assert!(store.read_only().is_none());
    assert_eq!(store_state(&store), state(&reference));
}

/// Every write of archiving, failing: no segment leaves the WAL, the
/// store stays writable, and the archive and the WAL still hold the whole
/// history. A failed copy is retried by the next checkpoint; a failed sync
/// of the archive directory disables checkpoints until the store is
/// reopened.
#[test]
fn every_archive_write_can_fail_without_losing_a_segment() {
    // (call, point, path, whether checkpoints are disabled afterwards)
    let rules = [
        (Call::Create, When::Before, "/archive/", false),
        (Call::Write, When::Before, "/archive/", false),
        (Call::Write, When::Midway, "/archive/", false),
        (Call::Write, When::After, "/archive/", false),
        (Call::Sync, When::Before, "/archive/", false),
        (Call::Sync, When::After, "/archive/", false),
        (Call::Rename, When::Before, "/archive/", false),
        (Call::Rename, When::After, "/archive/", false),
        (Call::SyncDir, When::Before, "/archive", true),
        (Call::SyncDir, When::After, "/archive", true),
    ];
    let mut opts = options(1);
    opts.wal.fsync = FsyncPolicy::Off;
    for (i, (call, when, path, disables)) in rules.into_iter().enumerate() {
        let dir: TempDir = tempfile::tempdir().unwrap();
        let fs = TestFs::default();
        let opts = StoreOptions { archive: Some(dir.path().join("archive")), ..opts.clone() };
        let store = Store::open_with(fs.clone(), &dir.path().join("data"), opts.clone()).unwrap();
        let mut reference = reference();
        run(&store, &mut reference, &workload(30, 30 + i as u64));
        let action = if i % 3 == 0 { Action::NoSpace } else { Action::Fail };
        let rule = Rule::new(call, when, action).path(path);
        fs.add(rule.clone());
        let before = segment_seqs(&dir.path().join("data"));
        assert!(store.checkpoint().is_err(), "{}", rule);
        assert_eq!(fs.state().fired, vec![rule.clone()], "{}", rule);
        assert_eq!(segment_seqs(&dir.path().join("data")), before, "{}: nothing removed", rule);
        assert!(store.read_only().is_none());
        run(&store, &mut reference, &workload(10, 60 + i as u64));
        let next = store.checkpoint();
        if disables {
            assert!(matches!(next, Err(Error::CheckpointsDisabled { .. })), "{}: {:?}", rule, next);
            drop(store);
            let store = Store::open(&dir.path().join("data"), opts).unwrap();
            commit_with_checkpoints(&store, &mut reference, &workload(10, 90 + i as u64));
        } else {
            next.unwrap();
        }
        assert_whole_history(dir.path(), &reference);
        let report = verify(&dir.path().join("archive")).unwrap();
        assert!(report.is_ok(), "{}: {:#?}", rule, report);
    }
}
