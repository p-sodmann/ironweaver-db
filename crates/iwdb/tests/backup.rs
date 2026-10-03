//! Online backup (ADR 0009): what a backup holds and reaches, its
//! destination, its manifest, and every write it makes failing. Restoring
//! backups is tested in `pitr.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{verify, Error, FsyncPolicy, Kind, Store, StoreOptions};
use iwdb_storage::backup::read_manifest;
use iwdb_storage::layout::{BACKUP_NAME, MARKER_NAME};
use support::{checkpoints, options, pad, reference, run, segment_seqs, snapshot, workload};
use tempfile::TempDir;

/// A store with two checkpoints and WAL after the newest.
fn store_with_history(fs: TestFs, opts: StoreOptions) -> (TempDir, Store<TestFs>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with(fs, &dir.path().join("data"), opts).unwrap();
    let mut reference = reference();
    let steps = workload(60, 11);
    run(&store, &mut reference, &steps[..20]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[20..40]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[40..]);
    (dir, store)
}

#[test]
fn a_backup_holds_the_checkpoints_and_the_wal_up_to_its_seq() {
    let (dir, store) = store_with_history(TestFs::default(), options(2));
    let dest = dir.path().join("backup");
    let report = store.backup(&dest).unwrap();
    assert_eq!(report.seq, store.seq());
    assert_eq!(report.history, store.history());
    assert!(report.time.is_some());
    let data = dir.path().join("data");
    assert_eq!(report.checkpoints, checkpoints(&data));
    assert_eq!(checkpoints(&dest), checkpoints(&data));
    // The WAL from the oldest checkpoint on, the last segment cut at the seq
    let oldest = report.checkpoints[0];
    assert!(report.segments[0] <= oldest + 1 && report.segments.len() > 1);
    assert_eq!(segment_seqs(&dest), report.segments);

    let verified = verify(&dest).unwrap();
    assert!(verified.is_ok(), "{:#?}", verified.problems);
    assert_eq!(
        (verified.kind, verified.seq, verified.history),
        (Kind::Backup, Some(report.seq), Some(store.history()))
    );
    assert_eq!(verified.last_seq, Some(report.seq));
    assert!(verified.notes.is_empty(), "{:#?}", verified.notes);
    let manifest = read_manifest(&dest.join(BACKUP_NAME)).unwrap();
    assert_eq!(
        (manifest.namespaces[0].seq, manifest.history, manifest.namespaces[0].time),
        (report.seq, report.history, report.time)
    );
    assert_eq!(manifest.files.len(), report.checkpoints.len() + report.segments.len() + 1, "and the namespace log");

    // The store goes on; a store doesn't open the backup
    run(&store, &mut reference_at(&store), &[pad(1)]);
    assert!(matches!(Store::open(&dest, options(2)), Err(Error::IsBackup { .. })));
}

/// A reference at the store's state, to commit more steps against.
fn reference_at(store: &Store<TestFs>) -> iwdb::Namespace {
    use iwdb_engine::codec;
    let loaded = store.read(|ns| codec::from_binary(&codec::to_binary(ns.graph(), &ns.graph_meta()).unwrap()).unwrap());
    iwdb::Namespace::from_loaded(loaded)
}

#[test]
fn a_backup_of_a_new_or_idle_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data"), options(2)).unwrap();
    // Nothing committed: a backup at seq 0, without files
    let report = store.backup(&dir.path().join("empty")).unwrap();
    assert_eq!((report.seq, report.checkpoints.len(), report.segments.len()), (0, 0, 0));
    let verified = verify(&dir.path().join("empty")).unwrap();
    assert!(verified.is_ok(), "{:#?}", verified);
    assert_eq!(verified.seq, Some(0));
    // Right after a checkpoint: the checkpoint and no WAL
    run(&store, &mut reference(), &[pad(0), pad(1)]);
    store.checkpoint().unwrap();
    let report = store.backup(&dir.path().join("at-checkpoint")).unwrap();
    assert_eq!((report.seq, report.checkpoints.as_slice(), report.segments.len()), (2, &[2][..], 0));
    assert!(verify(&dir.path().join("at-checkpoint")).unwrap().is_ok());
}

#[test]
fn the_destination_must_be_new_or_empty_and_outside_the_store() {
    let (dir, store) = store_with_history(TestFs::default(), options(2));
    let taken = dir.path().join("taken");
    fs::create_dir(&taken).unwrap();
    fs::write(taken.join("x"), b"x").unwrap();
    let before = snapshot(&taken);
    assert!(matches!(store.backup(&taken), Err(Error::DestinationNotEmpty { .. })));
    assert_eq!(snapshot(&taken), before);
    let file = dir.path().join("file");
    fs::write(&file, b"x").unwrap();
    assert!(matches!(store.backup(&file), Err(Error::DestinationNotEmpty { .. })));
    assert!(matches!(store.backup(&dir.path().join("data").join("inside")), Err(Error::InvalidOptions(_))));
    assert!(!dir.path().join("data").join("inside").exists());
    // An empty directory is fine
    let empty = dir.path().join("empty");
    fs::create_dir(&empty).unwrap();
    store.backup(&empty).unwrap();
    assert!(store.read_only().is_none());
}

/// Under `group` the last commits aren't synced yet: the backup syncs
/// first, so it reaches the last commit and holds only synced records.
#[test]
fn a_backup_syncs_first_and_reaches_the_last_commit() {
    let mut opts = options(2);
    opts.wal.fsync = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 1000 };
    let (dir, store) = store_with_history(TestFs::default(), opts);
    run(&store, &mut reference_at(&store), &[pad(5), pad(6)]);
    assert!(store.synced_seq() < store.seq());
    let report = store.backup(&dir.path().join("backup")).unwrap();
    assert_eq!(report.seq, store.seq());
    assert_eq!(store.synced_seq(), store.seq());
}

#[test]
fn damage_to_a_backup_that_its_files_cant_show_is_found_by_the_manifest() {
    let (dir, store) = store_with_history(TestFs::default(), options(2));
    let pristine = dir.path().join("backup");
    let report = store.backup(&pristine).unwrap();
    let copy = |name: &str| {
        let to = dir.path().join(name);
        copy_dir(&pristine, &to);
        to
    };
    let last = segment_seqs(&pristine).pop().unwrap();
    let last_name = iwdb_storage::format::segment_name(last);

    // The last segment cut at a frame boundary: every frame left is valid
    let cut = copy("cut");
    let path = cut.join("ns/00000000000000000001/wal").join(&last_name);
    let bytes = fs::read(&path).unwrap();
    let ends = common::frame_ends(&bytes);
    fs::write(&path, &bytes[..ends[ends.len() - 2]]).unwrap();
    let verified = verify(&cut).unwrap();
    assert!(verified.problems.iter().any(|p| p.message.contains("the manifest says")), "{:#?}", verified);
    assert!(verified.problems.iter().any(|p| p.message.contains("WAL ends at seq")), "{:#?}", verified);

    // The last segment gone
    let gone = copy("gone");
    fs::remove_file(gone.join("ns/00000000000000000001/wal").join(&last_name)).unwrap();
    let verified = verify(&gone).unwrap();
    assert!(verified.problems.iter().any(|p| p.message.contains("missing")), "{:#?}", verified);
    assert!(verified.problems.iter().any(|p| p.message.contains(&format!("manifest says {}", report.seq))));

    // A file the backup didn't write
    let extra = copy("extra");
    fs::write(extra.join("ns/00000000000000000001/checkpoints").join("00000000000000000001.ckpt"), b"x").unwrap();
    let verified = verify(&extra).unwrap();
    assert!(verified.problems.iter().any(|p| p.message.contains("not listed")), "{:#?}", verified);

    // The manifest damaged
    let damaged = copy("damaged");
    let manifest = damaged.join(BACKUP_NAME);
    let mut bytes = fs::read(&manifest).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 1;
    fs::write(&manifest, bytes).unwrap();
    let verified = verify(&damaged).unwrap();
    assert!(verified.problems.iter().any(|p| p.message.contains("checksum")), "{:#?}", verified);
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

/// Every write a backup makes, failing: the backup fails, what it leaves
/// is refused by a store, verify and restore (no marker), the store is
/// unaffected, and a new backup succeeds. A failure after the marker's
/// rename (the operation happened, the error came after) leaves a complete
/// backup, which verifies.
#[test]
fn every_write_of_a_backup_can_fail_and_leaves_nothing_valid_looking() {
    // (call, point, path, skip, whether the backup is complete on disk)
    let rules = [
        // The empty manifest that goes first
        (Call::Create, When::Before, "dest/BACKUP", 0, false),
        (Call::Sync, When::After, "dest/BACKUP", 0, false),
        // A file: created, written (whole and halfway), fsynced
        (Call::Create, When::Before, "/dest/ns/00000000000000000001/checkpoints/", 0, false),
        (Call::Create, When::Before, "/dest/ns/00000000000000000001/wal/", 0, false),
        (Call::Write, When::Before, "/dest/ns/00000000000000000001/checkpoints/", 0, false),
        (Call::Write, When::Midway, "/dest/ns/00000000000000000001/checkpoints/", 0, false),
        (Call::Write, When::Midway, "/dest/ns/00000000000000000001/wal/", 1, false),
        (Call::Write, When::After, "/dest/ns/00000000000000000001/wal/", 0, false),
        (Call::Sync, When::Before, "/dest/ns/00000000000000000001/checkpoints/", 1, false),
        (Call::Sync, When::Before, "/dest/ns/00000000000000000001/wal/", 0, false),
        (Call::Sync, When::After, "/dest/ns/00000000000000000001/wal/", 2, false),
        // The directories' syncs: the new directory, checkpoints/, wal/,
        // after the manifest, after the marker
        (Call::SyncDir, When::Before, "/dest", 0, false),
        (Call::SyncDir, When::Before, "/dest", 1, false),
        (Call::SyncDir, When::Before, "/dest", 2, false),
        (Call::SyncDir, When::Before, "/dest", 3, false),
        (Call::SyncDir, When::Before, "/dest", 4, false),
        (Call::SyncDir, When::After, "/dest", 5, false),
        (Call::SyncDir, When::Before, "/dest", 6, true),
        // The namespace log
        (Call::Create, When::Before, "dest/NAMESPACES", 0, false),
        (Call::Sync, When::After, "dest/NAMESPACES", 0, false),
        (Call::CreateDir, When::Before, "/dest/ns", 0, false),
        (Call::CreateDir, When::After, "/dest/ns/", 1, false),
        // The manifest and the marker
        (Call::WriteAtomic, When::Before, "BACKUP", 0, false),
        (Call::WriteAtomic, When::Midway, "BACKUP", 0, false),
        (Call::WriteAtomic, When::WriterDone, "BACKUP", 0, false),
        (Call::WriteAtomic, When::After, "BACKUP", 0, false),
        (Call::WriteAtomic, When::Before, "dest/IWDB", 0, false),
        (Call::WriteAtomic, When::WriterDone, "dest/IWDB", 0, false),
        (Call::WriteAtomic, When::After, "dest/IWDB", 0, true),
    ];
    // The store's own fsyncs are not under test here: `off` keeps it fast
    let mut opts = options(2);
    opts.wal.fsync = FsyncPolicy::Off;
    for (i, (call, when, path, skip, complete)) in rules.into_iter().enumerate() {
        // A full disk for some, an I/O error for the others
        let action = if i % 3 == 0 { Action::NoSpace } else { Action::Fail };
        {
            let fs = TestFs::default();
            let (dir, store) = store_with_history(fs.clone(), opts.clone());
            let rule = Rule::new(call, when, action).path(path).skip(skip);
            fs.add(rule.clone());
            let dest = dir.path().join("dest");
            let error = store.backup(&dest).expect_err("the backup fails");
            assert_eq!(fs.state().fired, vec![rule.clone()], "{}", rule);
            assert!(matches!(error, Error::Io { .. }), "{}: {:?}", rule, error);
            if complete {
                assert!(verify(&dest).unwrap().is_ok(), "{}", rule);
            } else {
                assert!(!dest.join(MARKER_NAME).exists(), "{}", rule);
                assert!(matches!(verify(&dest), Err(Error::NotADataDir { .. })), "{}", rule);
                // Either nothing but the directory, or refused
                let empty = fs::read_dir(&dest).unwrap().next().is_none();
                if !empty {
                    assert!(dest.join(BACKUP_NAME).exists(), "{}", rule);
                    assert!(matches!(Store::open(&dest, options(2)), Err(Error::NotADataDir { .. })), "{}", rule);
                }
            }
            // The store is unaffected, and a new backup works
            assert!(store.read_only().is_none() && store.checkpoint_failure().is_none(), "{}", rule);
            run(&store, &mut reference_at(&store), &[pad(7)]);
            store.checkpoint().unwrap();
            let report = store.backup(&dir.path().join("again")).unwrap();
            assert_eq!(report.seq, store.seq());
            assert!(verify(&dir.path().join("again")).unwrap().is_ok(), "{}", rule);
        }
    }
}
