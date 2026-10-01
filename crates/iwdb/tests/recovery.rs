//! Step 5 acceptance: `Store::open` recovers to the last acknowledged
//! commit after a clean shutdown, a crash after an append (with and
//! without a torn last frame), a crash in the middle of a checkpoint, and
//! with a damaged newest checkpoint. Every case compares the canonical
//! state, catalog and seq with a reference namespace that ran the same
//! step 3 workload in memory. Also the error cases of recovery: each
//! refuses to open and changes nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;

use common::{Call, Fault, TestFs};
use iwdb::{Error, Store};
use iwdb_engine::{Change, CommitRecord};
use iwdb_storage::format::Damage;
use iwdb_storage::{Wal, WalOptions};
use support::{
    checkpoint_path, checkpoints, frame, last_segment, options, reference, run, segment_seqs, snapshot, state,
    store_state, workload,
};

#[test]
fn clean_shutdown_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let steps = workload(60, 1);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert!(store.recovery().created);
    run(&store, &mut reference, &steps);
    store.close().unwrap();

    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    let report = store.recovery();
    assert!(!report.created);
    assert_eq!(report.checkpoint, Some(reference.seq()), "close wrote a checkpoint");
    assert_eq!(report.replayed, 0, "nothing to replay after a clean shutdown");
    assert!(report.torn_tail.is_none() && report.skipped_checkpoints.is_empty());

    // The reopened store continues where it left off
    let more = workload(20, 2);
    run(&store, &mut reference, &more[1..]);
    store.close().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
}

#[test]
fn crash_after_append() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let steps = workload(80, 3);
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &steps[..40]);
    let at = store.checkpoint().unwrap().seq;
    run(&store, &mut reference, &steps[40..]);
    // A crash: no close, so no sync and no checkpoint (with `always` every
    // acknowledged commit is synced already)
    drop(store);

    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    let report = store.recovery();
    assert_eq!(report.checkpoint, Some(at));
    assert_eq!(report.replayed, reference.seq() - at);
    assert!(report.torn_tail.is_none());
}

#[test]
fn crash_during_an_append_leaves_a_torn_last_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(30, 4));
    drop(store);

    // The next commit's frame was being written when the process died: it
    // was never acknowledged
    let segment = last_segment(dir.path());
    let valid_len = fs::metadata(&segment).unwrap().len();
    let next = reference.seq() + 1;
    let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
    file.write_all(&frame(next, next - 1, &[0; 64])[..40]).unwrap();
    drop(file);

    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    let tail = store.recovery().torn_tail.clone().expect("a torn tail");
    assert_eq!((tail.path.clone(), tail.valid_len, tail.file_len), (segment.clone(), valid_len, valid_len + 40));
    assert_eq!((tail.damage, tail.discarded_frames, tail.removed), (Damage::Truncated, 0, false));
    assert_eq!(fs::metadata(&segment).unwrap().len(), valid_len, "the tail was cut off");

    // Commits continue at the next seq, and survive another crash
    let more = workload(10, 5);
    run(&store, &mut reference, &more[1..]);
    drop(store);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    assert!(store.recovery().torn_tail.is_none());
}

#[test]
fn a_segment_without_a_valid_header_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(20, 6));
    drop(store);

    // A new segment with half its header (created outside the writer's
    // temp-and-rename protocol, as damage would leave it)
    let next = reference.seq() + 1;
    let path = dir.path().join("ns/00000000000000000001/wal").join(iwdb_storage::format::segment_name(next));
    fs::write(&path, &iwdb_storage::format::encode_segment_header(next)[..10]).unwrap();

    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    let tail = store.recovery().torn_tail.clone().unwrap();
    assert!(tail.removed);
    assert_eq!((tail.path, tail.valid_len), (path.clone(), 0));
    // The writer created the segment again, with a valid header
    assert_eq!(fs::read(&path).unwrap(), iwdb_storage::format::encode_segment_header(next));
}

#[test]
fn crash_mid_checkpoint_leaves_a_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(40, 7));
    let at = store.checkpoint().unwrap().seq;
    drop(store);

    // What `write_atomic` leaves when the process dies before the rename,
    // and a WAL segment whose creation was interrupted
    let ckpt_tmp = dir
        .path()
        .join("ns/00000000000000000001/checkpoints")
        .join(format!(".{}.123.0.tmp", iwdb_storage::checkpoint::checkpoint_name(at + 5)));
    fs::write(&ckpt_tmp, b"half a checkpoint").unwrap();
    let wal_tmp =
        dir.path().join("ns/00000000000000000001/wal").join(format!("{}.tmp", iwdb_storage::format::segment_name(999)));
    fs::write(&wal_tmp, b"half a header").unwrap();

    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    let mut removed = vec![ckpt_tmp.clone(), wal_tmp.clone()];
    removed.sort();
    assert_eq!(store.recovery().removed_temp_files, removed);
    assert!(!ckpt_tmp.exists() && !wal_tmp.exists());
    assert_eq!(store.recovery().checkpoint, Some(at));
}

/// The checkpoint was written and its directory synced, but the process
/// died before the old checkpoint and the WAL segments were removed. Here
/// the removal fails instead, which leaves the same files.
#[test]
fn crash_mid_checkpoint_before_the_wal_is_cut() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let mut reference = reference();
    let steps = workload(90, 8);
    let store = Store::open_with(fs.clone(), dir.path(), options(1)).unwrap();
    run(&store, &mut reference, &steps[..30]);
    let first = store.checkpoint().unwrap().seq;
    run(&store, &mut reference, &steps[30..60]);
    let segments_before = segment_seqs(dir.path());

    fs.inject(Call::RemoveFile, Fault::Fail);
    assert!(matches!(store.checkpoint(), Err(Error::Io { op: "remove", .. })));
    let second = reference.seq();
    assert_eq!(checkpoints(dir.path()), vec![first, second], "the new checkpoint exists, the old one too");
    assert_eq!(segment_seqs(dir.path()), segments_before, "no segment was removed");
    // Checkpoints are disabled until reopened; commits are not affected
    assert!(matches!(store.checkpoint(), Err(Error::CheckpointsDisabled { .. })));
    assert!(store.checkpoint_failure().is_some());
    run(&store, &mut reference, &steps[60..]);
    drop(store);

    let store = Store::open_with(fs, dir.path(), options(1)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    assert_eq!(store.recovery().checkpoint, Some(second));
    assert_eq!(store.recovery().replayed, reference.seq() - second);
    // Checkpoints work again, and cut the WAL now
    store.checkpoint().unwrap();
    assert_eq!(checkpoints(dir.path()), vec![reference.seq()]);
    assert!(segment_seqs(dir.path()).len() < segments_before.len());
}

/// Flip a byte in, or truncate, the newest checkpoint: recovery falls back
/// to the older one and replays more of the WAL.
#[test]
fn a_damaged_newest_checkpoint_falls_back_to_the_older_one() {
    for damage in ["flip", "truncate", "empty"] {
        let dir = tempfile::tempdir().unwrap();
        let mut reference = reference();
        let steps = workload(90, 9);
        let store = Store::open(dir.path(), options(2)).unwrap();
        run(&store, &mut reference, &steps[..30]);
        let older = store.checkpoint().unwrap().seq;
        run(&store, &mut reference, &steps[30..60]);
        let newer = store.checkpoint().unwrap().seq;
        run(&store, &mut reference, &steps[60..]);
        drop(store);
        assert_eq!(checkpoints(dir.path()), vec![older, newer]);

        let path = checkpoint_path(dir.path(), newer);
        let mut bytes = fs::read(&path).unwrap();
        match damage {
            "flip" => {
                let at = bytes.len() / 2;
                bytes[at] ^= 0x10;
            }
            "truncate" => bytes.truncate(bytes.len() - 7),
            _ => bytes.clear(),
        }
        fs::write(&path, &bytes).unwrap();

        let store = Store::open(dir.path(), options(2)).unwrap();
        assert_eq!(store_state(&store), state(&reference), "{}", damage);
        let report = store.recovery();
        assert_eq!(report.checkpoint, Some(older), "{}", damage);
        assert_eq!(report.skipped_checkpoints.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![newer]);
        assert_eq!(report.replayed, reference.seq() - older);
        assert_eq!(fs::read(&path).unwrap(), bytes, "the damaged checkpoint is kept");

        // The next checkpoint keeps the good older one, not the damaged one
        store.checkpoint().unwrap();
        assert_eq!(checkpoints(dir.path()), vec![older, newer, reference.seq()], "{}", damage);
        drop(store);
        let store = Store::open(dir.path(), options(2)).unwrap();
        assert_eq!(store_state(&store), state(&reference));
    }
}

/// With one checkpoint kept, the WAL before it is gone: if it is damaged,
/// recovery can't fall back, and refuses to open without changing
/// anything.
#[test]
fn no_fallback_without_the_wal_it_would_need() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(1)).unwrap();
    run(&store, &mut reference, &workload(60, 10));
    let at = store.checkpoint().unwrap().seq;
    drop(store);
    let first = segment_seqs(dir.path())[0];
    assert!(first > 1, "the WAL was cut");

    let path = checkpoint_path(dir.path(), at);
    let mut bytes = fs::read(&path).unwrap();
    bytes[40] ^= 1;
    fs::write(&path, &bytes).unwrap();
    let before = snapshot(dir.path());
    match Store::open(dir.path(), options(1)) {
        Err(Error::NoUsableCheckpoint { from: 1, first_seq, skipped }) => {
            assert_eq!(first_seq, first);
            assert_eq!(skipped.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![at]);
        }
        other => panic!("{:?}", other),
    }
    assert_eq!(snapshot(dir.path()), before);
}

/// Damage in a WAL segment other than the last is corruption: refused, and
/// nothing is truncated.
#[test]
fn wal_corruption_is_refused_without_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(40, 11));
    drop(store);
    let segments = iwdb_storage::list_segments(&dir.path().join("ns/00000000000000000001/wal")).unwrap();
    assert!(segments.len() > 2);
    let path = &segments[1].1;
    let mut bytes = fs::read(path).unwrap();
    let at = bytes.len() - 3;
    bytes[at] ^= 1;
    fs::write(path, &bytes).unwrap();

    let before = snapshot(dir.path());
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::Corrupt { .. })));
    assert_eq!(snapshot(dir.path()), before);
}

/// A logged record that fails to apply (here written behind the store's
/// back) is reported, and the log is left as it is.
#[test]
fn a_record_that_fails_to_replay_is_reported_and_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(10, 12));
    store.close().unwrap();

    let next = reference.seq() + 1;
    let mut wal = Wal::create(&dir.path().join("ns/00000000000000000001/wal"), WalOptions::default(), next).unwrap();
    let bad = ironweaver_core::Op::RemoveNode { id: "no such node".into() };
    wal.append(&CommitRecord::new(next, Change::Data(vec![bad]))).unwrap();
    wal.close().unwrap();

    let before = snapshot(dir.path());
    match Store::open(dir.path(), options(2)) {
        Err(Error::ReplayFailed { seq, source: iwdb_engine::Error::ApplyFailed { .. } }) => assert_eq!(seq, next),
        other => panic!("{:?}", other),
    }
    assert_eq!(snapshot(dir.path()), before);
}

/// The WAL ends before the checkpoint: refused.
#[test]
fn a_wal_that_ends_before_the_checkpoint_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(10, 13));
    store.close().unwrap();
    // Replace the log with an empty one that starts at 1
    let wal = dir.path().join("ns/00000000000000000001/wal");
    fs::remove_dir_all(&wal).unwrap();
    fs::create_dir(&wal).unwrap();
    Wal::create(&wal, WalOptions::default(), 1).unwrap().close().unwrap();

    let before = snapshot(dir.path());
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::LogEndsBefore { .. })));
    assert_eq!(snapshot(dir.path()), before);
}
