//! Checkpoints and the store's background work: a failed checkpoint never
//! deletes WAL segments or damages the previous checkpoint; the WAL is cut
//! to what the oldest kept checkpoint needs; commits keep flowing while a
//! checkpoint runs; the size and time triggers and the group commit timer
//! fire; failures make the store read-only until it is reopened.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::assert_matches;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use common::{Call, Fault, TestFs};
use iwdb::{CheckpointOptions, Error, FsyncPolicy, Store, StoreOptions};
use support::{
    checkpoint_path, checkpoints, frame, last_segment, options, pad, reference, run, segment_seqs, snapshot, state,
    store_state, workload,
};

/// Poll `done` for up to 10 s.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn files_in(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    snapshot(dir)
}

#[test]
fn a_failed_checkpoint_write_keeps_the_previous_checkpoint_and_the_wal() {
    for fault in [Fault::Fail, Fault::Partial] {
        let dir = tempfile::tempdir().unwrap();
        let fs = TestFs::default();
        let mut reference = reference();
        let steps = workload(60, 20);
        let store = Store::open_with(fs.clone(), dir.path(), options(1)).unwrap();
        run(&store, &mut reference, &steps[..40]);
        let first = store.checkpoint().unwrap().seq;
        run(&store, &mut reference, &steps[40..]);
        let ckpts = files_in(&dir.path().join("ns/00000000000000000001/checkpoints"));
        let wal = segment_seqs(dir.path());

        fs.inject(Call::WriteAtomic, fault);
        match store.checkpoint() {
            Err(Error::Io { op: "write checkpoint", .. }) => {}
            other => panic!("{:?}: {:?}", fault, other),
        }
        assert_eq!(
            files_in(&dir.path().join("ns/00000000000000000001/checkpoints")),
            ckpts,
            "{:?}: previous checkpoint intact, no temp file",
            fault
        );
        assert_eq!(segment_seqs(dir.path()), wal, "{:?}: no segment removed", fault);
        assert!(store.checkpoint_failure().is_some());
        assert!(store.read_only().is_none(), "commits are not affected");
        run(&store, &mut reference, &[pad(1)]);

        // Retried at the next checkpoint
        let outcome = store.checkpoint().unwrap();
        assert!(outcome.written && outcome.seq == reference.seq());
        assert_eq!(outcome.removed_checkpoints, vec![first]);
        assert!(store.checkpoint_failure().is_none());
        drop(store);
        let store = Store::open(dir.path(), options(1)).unwrap();
        assert_eq!(store_state(&store), state(&reference));
    }
}

/// After the rename, a failed directory fsync leaves the checkpoint's
/// durability unknown: nothing is deleted, and checkpoints stay disabled
/// until the store is reopened.
#[test]
fn a_failed_directory_sync_disables_checkpoints_until_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let mut reference = reference();
    let steps = workload(60, 21);
    let store = Store::open_with(fs.clone(), dir.path(), options(1)).unwrap();
    run(&store, &mut reference, &steps[..40]);
    let first = store.checkpoint().unwrap().seq;
    run(&store, &mut reference, &steps[40..]);
    let wal = segment_seqs(dir.path());
    let first_bytes = fs::read(checkpoint_path(dir.path(), first)).unwrap();

    fs.inject(Call::SyncDir, Fault::Fail);
    assert_matches!(store.checkpoint(), Err(Error::Io { op: "sync directory", .. }));
    assert_eq!(checkpoints(dir.path()), vec![first, reference.seq()]);
    assert_eq!(fs::read(checkpoint_path(dir.path(), first)).unwrap(), first_bytes);
    assert_eq!(segment_seqs(dir.path()), wal);
    run(&store, &mut reference, &[pad(2)]);
    assert_matches!(store.checkpoint(), Err(Error::CheckpointsDisabled { .. }));
    assert_matches!(store.checkpoint_failure(), Some(cause) if cause.contains("disabled"));
    // Close still syncs the WAL, and reports that it couldn't checkpoint
    assert_matches!(store.close(), Err(Error::CheckpointsDisabled { .. }));

    let store = Store::open_with(fs, dir.path(), options(1)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    store.checkpoint().unwrap();
    assert_eq!(checkpoints(dir.path()), vec![reference.seq()]);
}

/// Each checkpoint removes the checkpoints beyond `keep` and every WAL
/// segment whose records are all covered by the oldest kept checkpoint,
/// and nothing that recovery from that checkpoint needs.
#[test]
fn the_wal_is_cut_to_what_the_oldest_kept_checkpoint_needs() {
    for keep in [1usize, 2, 3] {
        let dir = tempfile::tempdir().unwrap();
        let mut reference = reference();
        let steps = workload(100, 22);
        let store = Store::open(dir.path(), options(keep)).unwrap();
        let mut taken = Vec::new();
        for chunk in steps.chunks(25) {
            run(&store, &mut reference, chunk);
            taken.push(store.checkpoint().unwrap().seq);
            let kept: Vec<u64> = taken.iter().rev().take(keep).rev().copied().collect();
            assert_eq!(checkpoints(dir.path()), kept, "keep {}", keep);
            let segments = segment_seqs(dir.path());
            let oldest = kept[0];
            // The first segment holds the record after the oldest kept checkpoint
            assert!(segments[0] <= oldest + 1, "keep {}: {:?} vs {}", keep, segments, oldest);
            // and the one after it starts after that record: nothing extra is kept
            if segments.len() > 1 {
                assert!(segments[1] > oldest + 1, "keep {}: {:?} vs {}", keep, segments, oldest);
            }
        }
        assert!(segment_seqs(dir.path())[0] > 1, "keep {}: segments were removed", keep);
        drop(store);
        // Every kept checkpoint can still be recovered from
        for seq in checkpoints(dir.path()).into_iter().rev().skip(1) {
            let newer: Vec<u64> = checkpoints(dir.path()).into_iter().filter(|s| *s > seq).collect();
            for s in &newer {
                fs::rename(checkpoint_path(dir.path(), *s), dir.path().join(format!("hidden-{}", s))).unwrap();
            }
            let store = Store::open(dir.path(), options(keep)).unwrap();
            assert_eq!(store.recovery().checkpoint, Some(seq));
            assert_eq!(store_state(&store), state(&reference));
            drop(store);
            for s in &newer {
                fs::rename(dir.path().join(format!("hidden-{}", s)), checkpoint_path(dir.path(), *s)).unwrap();
            }
        }
    }
}

/// The checkpointer works on its own copy of the namespace: while its
/// write is blocked, commits and reads keep working.
#[test]
fn commits_keep_flowing_while_a_checkpoint_runs() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let mut reference = reference();
    let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(30, 23));
    let target = reference.seq();

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let entered_tx = Mutex::new(entered_tx);
    fs.set_hook(Some(Arc::new(move |call, _path: &Path| {
        if call == Call::WriteAtomic {
            entered_tx.lock().unwrap().send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    })));

    std::thread::scope(|scope| {
        let checkpoint = scope.spawn(|| store.checkpoint());
        entered_rx.recv().unwrap();
        // The checkpoint is in the middle of writing its file
        for i in 0..50 {
            run(&store, &mut reference, &[pad(i)]);
        }
        assert_eq!(store.seq(), target + 50);
        assert!(store.node("p0").is_some());
        release_tx.send(()).unwrap();
        let outcome = checkpoint.join().unwrap().unwrap();
        assert_eq!(outcome.seq, target, "it covers what was synced when it started");
    });
    fs.set_hook(None);
    drop(store);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store.recovery().checkpoint, Some(target));
    assert_eq!(store_state(&store), state(&reference));
}

#[test]
fn the_size_trigger_checkpoints_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(1);
    opts.checkpoint = CheckpointOptions { wal_size: Some(4096), interval: None, background: true, ..opts.checkpoint };
    let mut reference = reference();
    let store = Store::open(dir.path(), opts.clone()).unwrap();
    for i in 0..40 {
        run(&store, &mut reference, &[pad(i)]);
    }
    eventually("a background checkpoint", || store.checkpoint_seq().is_some());
    eventually("the WAL to be cut", || segment_seqs(dir.path())[0] > 1);
    assert!(store.checkpoint_failure().is_none());
    drop(store);
    let store = Store::open(dir.path(), opts).unwrap();
    assert_eq!(store_state(&store), state(&reference));
}

#[test]
fn the_interval_trigger_checkpoints_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(2);
    opts.checkpoint = CheckpointOptions {
        wal_size: None,
        interval: Some(Duration::from_millis(30)),
        background: true,
        ..opts.checkpoint
    };
    let mut reference = reference();
    let store = Store::open(dir.path(), opts).unwrap();
    run(&store, &mut reference, &workload(10, 24));
    let seq = reference.seq();
    eventually("an interval checkpoint", || store.checkpoint_seq() == Some(seq));
}

/// With `group`, the timer syncs the last commits of a burst within about
/// `max_delay`, although no further commit comes.
#[test]
fn the_group_commit_timer_syncs_idle_commits() {
    let dir = tempfile::tempdir().unwrap();
    let opts = StoreOptions {
        wal: iwdb::WalOptions {
            fsync: FsyncPolicy::Group { max_delay: Duration::from_millis(20), max_batch: 1000 },
            ..options(2).wal
        },
        ..options(2)
    };
    let store = Store::open(dir.path(), opts).unwrap();
    let mut reference = reference();
    run(&store, &mut reference, &[pad(0), pad(1)]);
    assert!(store.synced_seq() < store.seq(), "not synced by the commits themselves");
    eventually("the timer's fsync", || store.synced_seq() == store.seq());
}

/// With `group`, background checkpoints only cover synced commits: an OS
/// crash must never leave a checkpoint newer than the log.
#[test]
fn background_checkpoints_never_pass_the_synced_seq() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(2);
    opts.wal.fsync = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 1000 };
    // Rotation fsyncs the old segment; one segment keeps everything unsynced
    opts.wal.segment_size = iwdb_storage::DEFAULT_SEGMENT_SIZE;
    opts.checkpoint = CheckpointOptions {
        wal_size: None,
        interval: Some(Duration::from_millis(10)),
        background: true,
        ..opts.checkpoint
    };
    let store = Store::open(dir.path(), opts).unwrap();
    let mut reference = reference();
    run(&store, &mut reference, &workload(20, 25));
    assert_eq!(store.synced_seq(), 0);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(store.checkpoint_seq(), None, "nothing synced, nothing checkpointed");
    store.sync().unwrap();
    let seq = reference.seq();
    eventually("a checkpoint of the synced commits", || store.checkpoint_seq() == Some(seq));
}

#[test]
fn a_wal_failure_makes_the_store_read_only_until_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let mut reference = reference();
    let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(10, 26));

    fs.inject(Call::Sync, Fault::Fail);
    // Its outcome is unknown: the record was written, the fsync failed
    assert_matches!(store.commit(&support_pad(100)), Err(Error::Io { op: "fsync", .. }));
    assert!(store.read_only().is_some());
    assert_matches!(store.commit(&support_pad(101)), Err(Error::ReadOnly { .. }));
    assert_eq!(store_state(&store), state(&reference), "reads still work");
    assert_matches!(store.close(), Err(Error::ReadOnly { .. }));

    // Reopening recovers; the record whose fsync failed is in the page
    // cache, so it is found in the log
    let store = Store::open(dir.path(), options(2)).unwrap();
    reference.commit(&support_pad(100)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
    assert!(store.read_only().is_none());
    store.commit(&support_pad(102)).unwrap();
}

fn support_pad(i: usize) -> Vec<iwdb::Mutation> {
    match pad(i) {
        support::Step::Tx(m) => m,
        support::Step::Catalog(_) => unreachable!(),
    }
}

#[test]
fn a_failed_truncation_fails_the_open_and_the_next_open_finishes_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let store = Store::open(dir.path(), options(2)).unwrap();
    run(&store, &mut reference, &workload(10, 27));
    drop(store);
    let segment = last_segment(dir.path());
    let next = reference.seq() + 1;
    OpenOptions::new().append(true).open(&segment).unwrap().write_all(&frame(next, next - 1, &[1; 32])[..20]).unwrap();

    let fs = TestFs::default();
    fs.inject(Call::Truncate, Fault::Fail);
    assert_matches!(Store::open_with(fs, dir.path(), options(2)), Err(Error::Io { op: "truncate", .. }));
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert!(store.recovery().torn_tail.is_some());
    assert_eq!(store_state(&store), state(&reference));
}

#[test]
fn the_status_and_histograms_report_commits_fsyncs_and_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut reference = reference();
    let steps = workload(30, 10);
    let store = Store::open(dir.path(), options(1)).unwrap();
    run(&store, &mut reference, &steps[..20]);
    let ns = store.default_namespace();
    let before = ns.status();
    assert_eq!(before.checkpoint, None);
    assert_eq!(before.last_checkpoint, None);
    assert_eq!(before.since_checkpoint, before.seq);
    assert_eq!(before.unsynced, Some(0), "fsync always: every commit is synced");
    let h = ns.histograms();
    // Every commit is timed, failed ones too; each applied one held the
    // write lock and was fsynced first
    assert_eq!(h.live.commits.count(), 20);
    let applied = h.live.write_holds.count();
    assert!(applied > 0 && applied <= 20, "{:?}", h.live.write_holds);
    assert!(h.live.fsyncs.count() >= applied, "{:?}", h.live.fsyncs);
    assert!(h.live.read_holds.count() >= 1, "status reads under the read lock");
    assert_eq!(h.checkpoints.count(), 0);
    assert!(ns.disk_usage().wal_bytes > 0);
    assert_eq!(ns.disk_usage().checkpoint_bytes, 0);

    let started = iwdb::CommitTime::now();
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[20..]);
    let after = ns.status();
    assert_eq!(after.checkpoint, Some(before.seq));
    assert_eq!(after.since_checkpoint, after.seq - before.seq);
    assert!(after.last_checkpoint.is_some_and(|t| t >= started), "{:?}", after.last_checkpoint);
    assert_eq!(ns.histograms().checkpoints.count(), 1);
    assert!(ns.disk_usage().checkpoint_bytes > 0);
    // A checkpoint that writes nothing isn't timed
    let seq = store.seq();
    store.checkpoint().unwrap();
    store.checkpoint().unwrap();
    assert_eq!(ns.histograms().checkpoints.count(), 2, "at seq {}", seq);

    // After reopening, the time is the checkpoint file's
    drop(ns);
    store.close().unwrap();
    let store = Store::open(dir.path(), options(1)).unwrap();
    let reopened = store.default_namespace().status();
    let written = fs::metadata(checkpoint_path(dir.path(), reopened.checkpoint.unwrap())).unwrap().modified().unwrap();
    let micros = written.duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as i64;
    assert_eq!(reopened.last_checkpoint.map(|t| t.0), Some(micros));
}
