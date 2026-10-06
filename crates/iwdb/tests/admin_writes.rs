//! The admin writes of step 16e on the store (ADR 0055): a checkpoint
//! during a throttled backup (design rule 3), verifying an open store, and
//! pruning a WAL archive without removing what a kept backup needs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::assert_matches;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use iwdb::{Error, RestoreSources, RestoreTarget, Store, StoreOptions, prune_archive, restore, verify};
use support::{History, Step, options, pad, snapshot, store_state, workload};

/// A store archiving into `work/archive`, keeping `keep` checkpoints.
fn store(work: &Path, keep: usize) -> (Store, StoreOptions) {
    let mut opts = options(keep);
    opts.archive = Some(work.join("archive"));
    (Store::open(&work.join("data"), opts.clone()).unwrap(), opts)
}

/// Run `steps` in chunks of `every`, with a checkpoint after each.
fn build(store: &Store, history: &mut History, steps: &[Step], every: usize) {
    for chunk in steps.chunks(every) {
        history.run(store, chunk);
        store.checkpoint().unwrap();
    }
}

/// The size of the files under `dir`.
fn bytes(dir: &Path) -> u64 {
    snapshot(dir).iter().map(|(_, b)| b.len() as u64).sum()
}

/// Restore `sources` to `target` under `work`, verify, open, and compare
/// with the history at the restored seq; returns that seq.
fn restore_and_check(
    work: &Path,
    name: &str,
    history: &History,
    sources: &RestoreSources,
    target: RestoreTarget,
) -> u64 {
    let dest = work.join(name);
    let report = restore(&dest, sources, target).unwrap_or_else(|e| panic!("{} {:?}: {}", name, target, e));
    let seq = report.namespace("default").unwrap().seq;
    let verified = verify(&dest).unwrap();
    let default = verified.namespaces.iter().find(|n| n.name == "default").unwrap();
    assert!(verified.is_ok() && default.seq == Some(seq), "{:#?}", verified);
    let restored = Store::open(&dest, options(2)).unwrap();
    assert_eq!(store_state(&restored), history.state_at(seq), "{} {:?}", name, target);
    seq
}

/// Design rule 3: a checkpoint started during a throttled backup waits for
/// the copy and then completes; the backup restores to the state at its
/// seq, and with the archive to the store's last commit.
#[test]
fn a_checkpoint_during_a_throttled_backup_completes_and_both_are_consistent() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 2);
    let store = Arc::new(store);
    let mut history = History::default();
    let steps = workload(120, 16);
    build(&store, &mut history, &steps[..60], 15);
    history.run(&store, &steps[60..80]);

    // About a second for the whole copy
    let rate = bytes(&work.path().join("data")).max(1);
    let dest = work.path().join("backup");
    let backup = {
        let (store, dest) = (store.clone(), dest.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            let report = store.backup_with(&dest, Some(rate)).unwrap();
            (report, start.elapsed())
        })
    };
    let started = Instant::now();
    while store.backup_stats().running == 0 {
        assert!(started.elapsed() < Duration::from_secs(10), "the backup never started");
        std::thread::sleep(Duration::from_millis(1));
    }
    // Commits go on while it copies; a checkpoint waits for it
    history.run(&store, &steps[80..100]);
    let committed = store.seq();
    let done = Arc::new(AtomicBool::new(false));
    let checkpoint = {
        let (store, done) = (store.clone(), done.clone());
        std::thread::spawn(move || {
            let outcome = store.checkpoint().unwrap();
            done.store(true, Ordering::SeqCst);
            outcome
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    // Done first, running second: a checkpoint done before a moment the
    // backup still ran finished during the copy
    let finished = done.load(Ordering::SeqCst);
    assert!(!(finished && store.backup_stats().running > 0), "a checkpoint finished while the backup copied");
    let (report, elapsed) = backup.join().unwrap();
    let outcome = checkpoint.join().unwrap();
    assert!(outcome.written && outcome.seq >= committed, "{:?} after seq {}", outcome, committed);
    // The copy kept to its rate (the first file's bytes are its only burst)
    let floor = Duration::from_secs_f64(report.bytes as f64 / rate as f64 * 0.8);
    assert!(elapsed >= floor, "{} bytes at {}/s took {:?}", report.bytes, rate, elapsed);
    let stats = store.backup_stats();
    assert_eq!((stats.running, stats.ok, stats.failed), (0, 1, 0));
    assert!(stats.bytes >= report.bytes && stats.last.is_some(), "{:?}", stats);

    // Both are consistent
    history.run(&store, &steps[100..]);
    let live = store.verify().unwrap();
    assert!(live.is_ok(), "{:#?}", live.problems);
    let backup_seq = report.namespace("default").unwrap().seq;
    let only = RestoreSources { backup: Some(dest.clone()), archive: None };
    assert_eq!(restore_and_check(work.path(), "from-backup", &history, &only, RestoreTarget::Latest), backup_seq);
    store.checkpoint().unwrap();
    let both = RestoreSources { backup: Some(dest), archive: Some(work.path().join("archive")) };
    let reached = restore_and_check(work.path(), "with-archive", &history, &both, RestoreTarget::Latest);
    assert!(reached > backup_seq && reached <= store.seq(), "{} of {}", reached, store.seq());
}

/// `Store::verify` checks an open store while commits run, without false
/// alarms from the WAL's growing tail, and finds a damaged checkpoint.
#[test]
fn an_open_store_verifies_while_it_commits_and_damage_is_found() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 2);
    let store = Arc::new(store);
    let mut history = History::default();
    let steps = workload(80, 17);
    build(&store, &mut history, &steps[..60], 20);
    let stop = Arc::new(AtomicBool::new(false));
    let committer = {
        let (store, stop) = (store.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::SeqCst) {
                let Step::Tx(m) = pad(10_000 + i) else { unreachable!() };
                store.commit(&m).unwrap();
                i += 1;
            }
        })
    };
    let built = history.ns.seq();
    let checks: Vec<_> = (0..5).map(|_| store.verify()).collect();
    stop.store(true, Ordering::SeqCst);
    committer.join().unwrap();
    for report in checks {
        let report = report.unwrap();
        assert!(report.is_ok(), "{:#?}", report.problems);
        let ns = report.namespaces.iter().find(|n| n.name == "default").unwrap();
        assert!(ns.seq.is_some_and(|s| s >= built) && ns.checkpoints_checked >= 1, "{:?}", ns);
    }

    let report = store.verify().unwrap();
    let ns = report.namespaces.iter().find(|n| n.name == "default").unwrap();
    assert_eq!(ns.seq, Some(store.seq()), "nothing committed during the check: replay reaches the live seq");

    // Damage to the oldest checkpoint
    let oldest = support::checkpoints(&work.path().join("data"))[0];
    let path = support::checkpoint_path(&work.path().join("data"), oldest);
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x40;
    std::fs::write(&path, bytes).unwrap();
    let report = store.verify().unwrap();
    assert!(!report.is_ok(), "the damaged checkpoint was missed");
    assert!(report.problems.iter().any(|p| p.path.as_deref() == Some(path.as_path())), "{:#?}", report.problems);
}

/// The segments of namespace 1 in an archive, by first seq.
fn archived(archive: &Path) -> Vec<u64> {
    iwdb_storage::archive::archive_segments(archive, 3, 1).unwrap().into_iter().map(|(s, _)| s).collect()
}

/// Pruning before a backup keeps every record after the backup's oldest
/// checkpoint, so restores from the backup and the archive reach every
/// seq they reached before; a dry run removes nothing; namespaces the
/// backup doesn't hold are left alone; another history is refused.
#[test]
fn pruning_never_removes_what_a_kept_backup_needs() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 1);
    let mut history = History::default();
    let steps = workload(200, 18);
    build(&store, &mut history, &steps[..80], 10);
    let old = work.path().join("old-backup");
    store.backup(&old).unwrap();
    build(&store, &mut history, &steps[80..120], 10);
    let kept = work.path().join("kept-backup");
    let kept_report = store.backup(&kept).unwrap();
    let kept_seq = kept_report.namespace("default").unwrap().seq;
    let oldest_kept = kept_report.namespace("default").unwrap().checkpoints[0];
    // A namespace the kept backup doesn't hold, with archived segments
    store.create_namespace("later", None).unwrap();
    let later = store.namespace("later").unwrap();
    for i in 0..40 {
        let Step::Tx(m) = pad(20_000 + i) else { unreachable!() };
        later.commit(&m).unwrap();
        if i % 10 == 9 {
            later.checkpoint().unwrap();
        }
    }
    build(&store, &mut history, &steps[120..], 10);
    let archive = work.path().join("archive");
    let before = archived(&archive);
    assert!(before.len() > 4 && before[0] == 1, "{:?}", before);

    // A dry run says what would go and removes nothing
    let files = snapshot(&archive);
    let dry = prune_archive(&archive, &kept, true).unwrap();
    assert_eq!(snapshot(&archive), files);
    let ns = dry.namespaces.iter().find(|n| n.name == "default").unwrap();
    assert_eq!(ns.backup_checkpoint, oldest_kept);
    assert!(!ns.removed_segments.is_empty() && dry.bytes > 0, "{:?}", dry);
    let later_id = later.id();
    assert!(dry.untouched.contains(&later_id), "{:?}", dry);

    let pruned = prune_archive(&archive, &kept, false).unwrap();
    assert_eq!(pruned.namespaces, dry.namespaces);
    let after = archived(&archive);
    assert_eq!(after.len(), before.len() - ns.removed_segments.len());
    // Every record after the backup's oldest checkpoint is still archived
    assert!(after[0] <= oldest_kept + 1, "{:?} for checkpoint {}", after, oldest_kept);
    assert!(before.iter().filter(|s| !after.contains(s)).all(|s| ns.removed_segments.contains(s)));
    let verified = verify(&archive).unwrap();
    assert!(verified.is_ok(), "{:#?}", verified.problems);
    // The later namespace's segments are all there
    assert!(!iwdb_storage::archive::archive_segments(&archive, 3, later_id).unwrap().is_empty());
    // Again: nothing more to remove
    let again = prune_archive(&archive, &kept, false).unwrap();
    assert!(again.namespaces.iter().all(|n| n.removed_segments.is_empty()), "{:?}", again);

    // Restores from the kept backup and the archive reach what they reached
    store.checkpoint_all().unwrap();
    let both = RestoreSources { backup: Some(kept.clone()), archive: Some(archive.clone()) };
    let only_default = [iwdb::NamespaceName::new("default").unwrap()];
    for (name, seq) in [("at-backup", kept_seq), ("between", kept_seq + 30), ("at-oldest", oldest_kept)] {
        let dest = work.path().join(name);
        let report = iwdb::restore_namespaces(&dest, &both, RestoreTarget::Seq(seq), Some(&only_default)).unwrap();
        assert_eq!(report.seq, seq, "{}", name);
        let restored = Store::open(&dest, options(2)).unwrap();
        assert_eq!(store_state(&restored), history.state_at(seq), "{}", name);
    }
    let latest = restore_and_check(work.path(), "latest", &history, &both, RestoreTarget::Latest);
    assert!(latest > kept_seq);

    // Another history is refused
    let other = tempfile::tempdir().unwrap();
    let foreign = Store::open(&other.path().join("data"), options(2)).unwrap();
    foreign.backup(&other.path().join("backup")).unwrap();
    assert_matches!(prune_archive(&archive, &other.path().join("backup"), false), Err(Error::HistoryMismatch { .. }));
    assert_matches!(prune_archive(&archive, work.path(), false), Err(Error::NotADataDir { .. }));
    assert_matches!(prune_archive(&work.path().join("data"), &kept, false), Err(Error::NotAnArchive { .. }));
    // The older backup can't restore past the pruned gap any more: that is
    // what pruning before the kept backup means
    let _ = old;
}
