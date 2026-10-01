//! Restore and point-in-time recovery (step 7, ADR 0009): restoring to a
//! random mid-history seq from a backup alone, from an archive alone and
//! from both gives the reference state at that seq (canonical graph,
//! catalog, seq), and the restored store goes on at seq + 1; restoring to
//! a time; online backups taken while commits and checkpoints run;
//! refusals; and every write of a restore failing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{restore, restore_with, verify, CheckpointOptions, CommitTime, Error, FsyncPolicy, Store, StoreOptions};
use iwdb::{RestoreSources, RestoreTarget};
use iwdb_engine::testutil::workload::Stream;
use support::{options, pad, segment_seqs, snapshot, store_state, workload, History, Step};

/// A small deterministic random number generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % n.max(1)
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
}

fn from_backup(backup: &Path) -> RestoreSources {
    RestoreSources { backup: Some(backup.to_path_buf()), archive: None }
}

fn from_archive(archive: &Path) -> RestoreSources {
    RestoreSources { backup: None, archive: Some(archive.to_path_buf()) }
}

fn from_both(backup: &Path, archive: &Path) -> RestoreSources {
    RestoreSources { backup: Some(backup.to_path_buf()), archive: Some(archive.to_path_buf()) }
}

/// Restore into a new directory under `work`, open it, and compare with
/// the history at the seq it reports (and at `expected`). The restored
/// store goes on at seq + 1.
fn restore_and_check(work: &Path, history: &History, sources: &RestoreSources, target: RestoreTarget, expected: u64) {
    let dest = work.join(format!("restored-{:?}-{}", target, expected).replace(['(', ')', ' '], ""));
    let _ = std::fs::remove_dir_all(&dest);
    let report = restore(&dest, sources, target).unwrap_or_else(|e| panic!("{:?} to {}: {}", target, expected, e));
    assert_eq!(report.seq, expected, "{:?}", target);
    let verified = verify(&dest).unwrap();
    assert!(verified.is_ok() && verified.seq == Some(expected), "{:#?}", verified);
    let store = Store::open(&dest, options(2)).unwrap();
    assert_eq!(store.seq(), expected);
    assert_eq!(store_state(&store), history.state_at(expected), "{:?} to {}", target, expected);
    assert_eq!(Some(store.history()), Some(report.history));
    assert_ne!(Some(report.history), report.source_history, "a restore starts a new history");
    let next = store.commit(&tx(pad(9_999))).unwrap();
    assert_eq!(next.seq, expected + 1);
}

fn tx(step: Step) -> Vec<iwdb::Mutation> {
    match step {
        Step::Tx(m) => m,
        Step::Catalog(_) => unreachable!(),
    }
}

/// A store with a history of `n` steps from `seed`, a checkpoint after
/// every `every`, keeping `keep` checkpoints, optionally archiving.
fn store(work: &Path, keep: usize, archive: bool) -> (Store, StoreOptions) {
    let mut opts = options(keep);
    if archive {
        opts.archive = Some(work.join("archive"));
    }
    (Store::open(&work.join("data"), opts.clone()).unwrap(), opts)
}

fn build(store: &Store, history: &mut History, steps: &[Step], every: usize) {
    for chunk in steps.chunks(every) {
        history.run(store, chunk);
        store.checkpoint().unwrap();
    }
}

#[test]
fn pitr_from_a_backup_alone() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 3, false);
    let mut history = History::default();
    build(&store, &mut history, &workload(60, 41), 15);
    history.run(&store, &workload(10, 42));
    let backup = work.path().join("backup");
    let report = store.backup(&backup).unwrap();
    assert_eq!(report.seq, history.seq());
    let oldest = report.checkpoints[0];
    let mut rng = Rng(7);
    let mut targets = vec![oldest, report.seq, report.checkpoints[1]];
    targets.extend((0..6).map(|_| rng.range(oldest, report.seq)));
    for seq in targets {
        restore_and_check(work.path(), &history, &from_backup(&backup), RestoreTarget::Seq(seq), seq);
    }
    restore_and_check(work.path(), &history, &from_backup(&backup), RestoreTarget::Latest, report.seq);
    // Before its oldest checkpoint, the backup alone can't restore
    let early = restore(&work.path().join("early"), &from_backup(&backup), RestoreTarget::Seq(oldest - 1));
    assert!(matches!(early, Err(Error::MissingRecords { .. })), "{:?}", early);
    // Beyond its seq neither
    let late = restore(&work.path().join("late"), &from_backup(&backup), RestoreTarget::Seq(report.seq + 1));
    assert!(matches!(late, Err(Error::LogEndsBefore { .. })), "{:?}", late);
}

#[test]
fn pitr_from_the_archive_alone() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 1, true);
    let mut history = History::default();
    build(&store, &mut history, &workload(80, 43), 10);
    let archive = work.path().join("archive");
    let end = verify(&archive).unwrap().last_seq.unwrap();
    assert!(end > 40 && end < history.seq(), "{}", end);
    let mut rng = Rng(8);
    let mut targets = vec![0, 1, end];
    targets.extend((0..6).map(|_| rng.range(1, end)));
    for seq in targets {
        restore_and_check(work.path(), &history, &from_archive(&archive), RestoreTarget::Seq(seq), seq);
    }
    restore_and_check(work.path(), &history, &from_archive(&archive), RestoreTarget::Latest, end);
    // While the store goes on archiving into it
    history.run(&store, &workload(5, 44));
    restore_and_check(work.path(), &history, &from_archive(&archive), RestoreTarget::Seq(end - 3), end - 3);
}

#[test]
fn pitr_from_a_backup_and_the_archive() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 2, true);
    let mut history = History::default();
    build(&store, &mut history, &workload(40, 45), 10);
    history.run(&store, &workload(5, 46));
    let backup = work.path().join("backup");
    let b = store.backup(&backup).unwrap();
    // Go on: the archive grows past the backup's seq
    build(&store, &mut history, &workload(60, 47), 10);
    let archive = work.path().join("archive");
    let end = verify(&archive).unwrap().last_seq.unwrap();
    assert!(end > b.seq + 10, "{} {}", end, b.seq);
    // The segment the backup cut is in the archive whole: the restore
    // takes the longer copy
    let cut = *b.segments.last().unwrap();
    assert!(segment_seqs_in(&archive).contains(&cut));
    let mut rng = Rng(9);
    let mut targets = vec![b.checkpoints[0], b.seq, b.seq + 1, end];
    targets.extend((0..6).map(|_| rng.range(b.checkpoints[0], end)));
    for seq in targets {
        restore_and_check(work.path(), &history, &from_both(&backup, &archive), RestoreTarget::Seq(seq), seq);
    }
    restore_and_check(work.path(), &history, &from_both(&backup, &archive), RestoreTarget::Latest, end);
}

fn segment_seqs_in(dir: &Path) -> Vec<u64> {
    // An archive keeps a namespace's segments in ns/<id>/
    let dir = if dir.join("IWDBARCH").exists() { dir.join("ns/00000000000000000001") } else { dir.to_path_buf() };
    iwdb_storage::list_segments(&dir).unwrap().into_iter().map(|(s, _)| s).collect()
}

/// The commit time of every record in the archive and the WAL.
fn commit_times(work: &Path) -> BTreeMap<u64, CommitTime> {
    let mut segments: BTreeMap<u64, PathBuf> =
        iwdb_storage::list_segments(&work.join("archive/ns/00000000000000000001")).unwrap().into_iter().collect();
    segments.extend(iwdb_storage::list_segments(&work.join("data").join("ns/00000000000000000001/wal")).unwrap());
    let mut reader = iwdb_storage::WalReader::from_segments(segments.into_iter().collect(), 1, u64::MAX).unwrap();
    let mut times = BTreeMap::new();
    while let Some(record) = reader.next() {
        times.insert(record.unwrap().seq, reader.time().unwrap());
    }
    times
}

#[test]
fn pitr_to_a_time() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 1, true);
    let mut history = History::default();
    for chunk in workload(60, 48).chunks(10) {
        history.run(&store, chunk);
        store.checkpoint().unwrap();
        std::thread::sleep(Duration::from_millis(3));
    }
    let archive = work.path().join("archive");
    let end = verify(&archive).unwrap().last_seq.unwrap();
    let times = commit_times(work.path());
    let values: Vec<CommitTime> = times.values().copied().collect();
    assert!(values.windows(2).all(|w| w[0] <= w[1]), "commit times never go backwards");
    // The last record in seq order at or before the time
    let expected_at = |t: CommitTime| times.iter().filter(|(s, time)| **s <= end && **time <= t).map(|(s, _)| *s).max();
    let mut rng = Rng(10);
    for _ in 0..6 {
        let seq = rng.range(1, end);
        let t = times[&seq];
        let expected = expected_at(t).unwrap();
        assert!(expected >= seq);
        restore_and_check(work.path(), &history, &from_archive(&archive), RestoreTarget::Time(t), expected);
        // Just before it
        let before = CommitTime(t.0 - 1);
        if let Some(expected) = expected_at(before) {
            restore_and_check(work.path(), &history, &from_archive(&archive), RestoreTarget::Time(before), expected);
        }
    }
    let first = times[&1];
    // Between the namespace's creation and its first commit it was empty (layout 4 knows when a
    // namespace was created; layout 1 to 3 stores don't, see `the_fixtures_restore`)
    restore_and_check(work.path(), &history, &from_archive(&archive), RestoreTarget::Time(CommitTime(first.0 - 1)), 0);
    // Before the namespace existed there is nothing to restore
    match restore(&work.path().join("too-early"), &from_archive(&archive), RestoreTarget::Time(CommitTime(0))) {
        Err(Error::NoCommitAtOrBefore { .. }) => {}
        other => panic!("{:?}", other),
    }
    let future = CommitTime(times[&end].0 + 3_600_000_000);
    restore_and_check(work.path(), &history, &from_archive(&archive), RestoreTarget::Time(future), end);
}

#[test]
fn restores_that_must_be_refused() {
    let work = tempfile::tempdir().unwrap();
    let (store, opts) = store(work.path(), 2, true);
    let mut history = History::default();
    build(&store, &mut history, &workload(30, 49), 10);
    let backup = work.path().join("backup");
    store.backup(&backup).unwrap();
    let archive = work.path().join("archive");
    let before = (snapshot(&backup), snapshot(&archive));

    // No source; a destination that isn't empty or is inside a source
    assert!(matches!(
        restore(&work.path().join("x"), &RestoreSources::default(), RestoreTarget::Latest),
        Err(Error::InvalidOptions(_))
    ));
    let taken = work.path().join("taken");
    std::fs::create_dir(&taken).unwrap();
    std::fs::write(taken.join("f"), b"x").unwrap();
    assert!(matches!(
        restore(&taken, &from_backup(&backup), RestoreTarget::Latest),
        Err(Error::DestinationNotEmpty { .. })
    ));
    assert!(matches!(
        restore(&backup.join("inside"), &from_backup(&backup), RestoreTarget::Latest),
        Err(Error::InvalidOptions(_))
    ));
    // A store's directory while it is open
    let live = restore(&work.path().join("live"), &from_backup(&work.path().join("data")), RestoreTarget::Latest);
    assert!(matches!(live, Err(Error::Locked { .. })), "{:?}", live);

    // Another history's archive
    let other_work = tempfile::tempdir().unwrap();
    let (other, _) = self::store(other_work.path(), 1, true);
    build(&other, &mut History::default(), &workload(30, 50), 10);
    let mixed = restore(
        &work.path().join("mixed"),
        &from_both(&backup, &other_work.path().join("archive")),
        RestoreTarget::Latest,
    );
    assert!(matches!(mixed, Err(Error::HistoryMismatch { .. })), "{:?}", mixed);
    // A layout 1 directory has no history to match an archive with
    let v1 = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data-dir-v1/store");
    assert!(matches!(
        restore(&work.path().join("v1"), &from_both(&v1, &archive), RestoreTarget::Latest),
        Err(Error::HistoryMismatch { .. })
    ));
    // An interrupted backup
    let partial = work.path().join("partial");
    std::fs::create_dir_all(partial.join("ns/00000000000000000001/wal")).unwrap();
    std::fs::write(partial.join("BACKUP"), b"").unwrap();
    assert!(matches!(
        restore(&work.path().join("p"), &from_backup(&partial), RestoreTarget::Latest),
        Err(Error::NotADataDir { .. })
    ));
    // The sources are unchanged
    assert_eq!((snapshot(&backup), snapshot(&archive)), before);

    // A restored store has a new history: the old archive is refused
    restore(&work.path().join("restored"), &from_backup(&backup), RestoreTarget::Latest).unwrap();
    drop(store);
    let reuse = Store::open(&work.path().join("restored"), StoreOptions { archive: opts.archive, ..options(2) });
    assert!(matches!(reuse, Err(Error::ArchiveMismatch { .. })), "{:?}", reuse.err());
}

#[test]
fn the_fixtures_restore() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let work = tempfile::tempdir().unwrap();
    for (sources, file) in [
        (from_backup(&fixtures.join("backup-v1/backup")), "backup-v1/expected.txt"),
        (from_archive(&fixtures.join("archive-v1/archive")), "archive-v1/expected.txt"),
        (from_backup(&fixtures.join("data-dir-v1/store")), "data-dir-v1/expected.txt"),
        (from_backup(&fixtures.join("data-dir-v2/store")), "data-dir-v2/expected.txt"),
    ] {
        let dest = work.path().join(file.replace('/', "-"));
        restore(&dest, &sources, RestoreTarget::Latest).unwrap();
        let store = Store::open(&dest, options(2)).unwrap();
        let expected = std::fs::read_to_string(fixtures.join(file)).unwrap();
        let mut described = String::new();
        store.read(|ns| {
            for line in iwdb_engine::testutil::canonical(ns.graph()) {
                described += &line;
                described.push('\n');
            }
            described += &format!("catalog {:?}\nseq {}\n", ns.catalog(), ns.seq());
        });
        assert!(expected.starts_with(&described), "{}", file);
    }
}

/// Backups taken while another thread commits, with background
/// checkpoints that remove WAL segments: each restores to the history at
/// the seq it reports.
#[test]
fn online_backups_while_commits_and_checkpoints_run() {
    let work = tempfile::tempdir().unwrap();
    let mut opts = options(1);
    opts.wal.fsync = FsyncPolicy::Group { max_delay: Duration::from_millis(2), max_batch: 8 };
    opts.checkpoint = CheckpointOptions {
        wal_size: Some(4 << 10),
        interval: Some(Duration::from_millis(5)),
        on_close: false,
        keep: 1,
        background: true,
    };
    let store = Arc::new(Store::open(&work.path().join("data"), opts).unwrap());
    let history = Arc::new(Mutex::new(History::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (store, history, stop) = (store.clone(), history.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut steps = Stream::new(51);
            while !stop.load(Ordering::SeqCst) {
                let step = steps.next().unwrap();
                history.lock().unwrap().run(&store, &[step]);
            }
        })
    };
    let mut backups = Vec::new();
    for i in 0..8 {
        std::thread::sleep(Duration::from_millis(15));
        let dest = work.path().join(format!("backup-{}", i));
        let report = store.backup(&dest).unwrap();
        backups.push((dest, report.seq));
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    assert!(store.checkpoint_failure().is_none(), "{:?}", store.checkpoint_failure());
    assert!(backups.windows(2).all(|w| w[0].1 <= w[1].1));
    assert!(segment_seqs(&work.path().join("data"))[0] > 1, "the checkpointer removed segments");
    let history = history.lock().unwrap();
    for (dest, seq) in &backups {
        restore_and_check(work.path(), &history, &from_backup(dest), RestoreTarget::Latest, *seq);
    }
}

/// A checkpoint that wants to remove segments while a backup copies them
/// waits for the backup: it can't remove a file the backup still needs.
#[test]
fn a_checkpoint_waits_for_the_backups_copy() {
    let work = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let store = Arc::new(Store::open_with(fs.clone(), &work.path().join("data"), options(1)).unwrap());
    let mut history = History::default();
    history.run(&store, &workload(40, 52));
    // Pause the backup at its first WAL segment
    let (paused_tx, paused) = mpsc::channel();
    let (resume, resume_rx) = mpsc::channel::<()>();
    let resume_rx = Mutex::new(resume_rx);
    fs.set_pause(Some(Arc::new(move |_: &Rule, _: &Path| {
        paused_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    })));
    fs.add(Rule::new(Call::Create, When::Before, Action::Pause).path("/backup/ns/00000000000000000001/wal/"));
    let backup = work.path().join("backup");
    let copying = {
        let (store, backup) = (store.clone(), backup.clone());
        std::thread::spawn(move || store.backup(&backup).unwrap())
    };
    paused.recv().unwrap();
    // Commits go on; a checkpoint that would cut the WAL waits
    history.run(&store, &workload(20, 53));
    let before = segment_seqs(&work.path().join("data"));
    let done = Arc::new(AtomicBool::new(false));
    let checkpoint = {
        let (store, done) = (store.clone(), done.clone());
        std::thread::spawn(move || {
            let outcome = store.checkpoint().unwrap();
            done.store(true, Ordering::SeqCst);
            outcome
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    assert!(!done.load(Ordering::SeqCst), "the checkpoint ran during the copy");
    assert_eq!(segment_seqs(&work.path().join("data")), before);
    resume.send(()).unwrap();
    let report = copying.join().unwrap();
    let outcome = checkpoint.join().unwrap();
    assert!(!outcome.removed_segments.is_empty(), "then it cut the WAL");
    assert!(report.seq < history.seq());
    restore_and_check(work.path(), &history, &from_backup(&backup), RestoreTarget::Latest, report.seq);
}

/// Every write of a restore, failing: the restore fails, and what it
/// leaves is refused by a store (or is complete, if the error came after
/// the marker's rename); a new restore succeeds.
#[test]
fn every_write_of_a_restore_can_fail_and_leaves_nothing_wrong() {
    let work = tempfile::tempdir().unwrap();
    let (store, _) = store(work.path(), 2, false);
    let mut history = History::default();
    build(&store, &mut history, &workload(30, 54), 10);
    history.run(&store, &workload(5, 55));
    let backup = work.path().join("backup");
    let b = store.backup(&backup).unwrap();
    drop(store);
    let target = RestoreTarget::Seq(b.seq - 2);
    // (call, point, path, skip, complete)
    let rules = [
        (Call::Create, When::Before, "RESTORING", 0, false),
        (Call::Write, When::Midway, "RESTORING", 0, false),
        (Call::Sync, When::Before, "RESTORING", 0, false),
        (Call::SyncDir, When::Before, "/dest", 0, false),
        (Call::WriteAtomic, When::Before, "/dest/ns/00000000000000000001/checkpoints/", 0, false),
        (Call::WriteAtomic, When::Midway, "/dest/ns/00000000000000000001/checkpoints/", 0, false),
        (Call::WriteAtomic, When::WriterDone, "/dest/ns/00000000000000000001/checkpoints/", 0, false),
        (Call::WriteAtomic, When::After, "/dest/ns/00000000000000000001/checkpoints/", 0, false),
        (Call::SyncDir, When::Before, "/dest/ns/00000000000000000001/checkpoints", 0, false),
        (Call::RemoveFile, When::Before, "RESTORING", 0, false),
        (Call::RemoveFile, When::After, "RESTORING", 0, false),
        (Call::SyncDir, When::Before, "/dest", 7, false),
        (Call::CreateDir, When::Before, "/dest/ns", 0, false),
        (Call::CreateDir, When::Before, "/dest/ns/", 1, false),
        (Call::WriteAtomic, When::Before, "NAMESPACES", 0, false),
        (Call::WriteAtomic, When::WriterDone, "NAMESPACES", 0, false),
        (Call::WriteAtomic, When::Before, "dest/IWDB", 0, false),
        (Call::WriteAtomic, When::WriterDone, "dest/IWDB", 0, false),
        (Call::WriteAtomic, When::After, "dest/IWDB", 0, true),
        (Call::SyncDir, When::Before, "/dest", 8, true),
    ];
    for (i, (call, when, path, skip, complete)) in rules.into_iter().enumerate() {
        let action = if i % 3 == 0 { Action::NoSpace } else { Action::Fail };
        let fs = TestFs::default();
        let rule = Rule::new(call, when, action).path(path).skip(skip);
        fs.add(rule.clone());
        let dest = work.path().join(format!("case-{}", i)).join("dest");
        let error = restore_with(&fs, &dest, &from_backup(&backup), target).expect_err("the restore fails");
        assert_eq!(fs.state().fired, vec![rule.clone()], "{}", rule);
        assert!(matches!(error, Error::Io { .. }), "{}: {:?}", rule, error);
        // Failed before anything was written: just the directory, as for
        // any empty directory
        let empty = std::fs::read_dir(&dest).unwrap().next().is_none();
        match Store::open(&dest, options(2)) {
            Ok(_) if empty => {}
            Ok(store) => {
                assert!(complete, "{}: an incomplete restore opened", rule);
                assert_eq!(store_state(&store), history.state_at(b.seq - 2), "{}", rule);
            }
            Err(Error::InterruptedRestore { .. }) | Err(Error::NotADataDir { .. }) => assert!(!complete, "{}", rule),
            Err(e) => panic!("{}: {}", rule, e),
        }
        // Removed, and restored again
        std::fs::remove_dir_all(&dest).unwrap();
        restore(&dest, &from_backup(&backup), target).unwrap();
        assert_eq!(store_state(&Store::open(&dest, options(2)).unwrap()), history.state_at(b.seq - 2));
    }
}
