//! Step 6: every failpoint of the storage layer, through the store. Each
//! test injects a failure (an I/O error, a full disk, a torn write, an
//! error after the operation happened) at one call, checks the defined
//! outcome (documentation/guarantees.md, "Simulated failures"), then
//! reopens and compares the canonical state, catalog and seq with the
//! reference namespace. A commit whose write or fsync failed has an unknown
//! outcome: the test says whether recovery must find it, lose it, or may
//! do either.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::path::Path;
use std::time::Duration;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{Error, FsyncPolicy, Mutation, Namespace, Store, StoreOptions};
use iwdb_storage::layout::MARKER_NAME;
use support::{checkpoints, options, pad, reference, run, segment_seqs, snapshot, state, store_state, workload, Step};
use tempfile::TempDir;

/// A store (through a `TestFs`) with a history: a workload with a
/// checkpoint in the middle, several WAL segments.
struct Case {
    dir: TempDir,
    fs: TestFs,
    store: Store<TestFs>,
    reference: Namespace,
    opts: StoreOptions,
}

fn case_with(opts: StoreOptions, seed: u64) -> Case {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let store = Store::open_with(fs.clone(), dir.path(), opts.clone()).unwrap();
    let mut reference = reference();
    let steps = workload(40, seed);
    run(&store, &mut reference, &steps[..40]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[40..]);
    Case { dir, fs, store, reference, opts }
}

fn case(seed: u64) -> Case {
    case_with(options(2), seed)
}

/// A commit with a 1200-byte value: a frame larger than a 1 KiB segment,
/// so it rotates unless the current segment is empty.
fn big(i: usize) -> Vec<Mutation> {
    vec![Mutation::UpsertNode {
        id: "big".into(),
        labels: vec![],
        attr: [("v".to_owned(), iwdb::Value::from(format!("{:0>1200}", i)))].into(),
        meta: Default::default(),
        expected_version: None,
    }]
}

fn tx(step: Step) -> Vec<Mutation> {
    match step {
        Step::Tx(m) => m,
        Step::Catalog(_) => unreachable!(),
    }
}

/// What recovery must do with a commit whose outcome is unknown.
#[derive(Clone, Copy, Debug, PartialEq)]
enum InFlight {
    /// Not in the log.
    Lost,
    /// Complete in the log (written, the page cache survived).
    Kept,
}

impl Case {
    /// Commit `mutations` and expect an I/O error; then the store is
    /// read-only, refuses commits, and reads see the reference.
    fn failed_commit(&self, mutations: &[Mutation]) -> Error {
        let error = self.store.commit(mutations).expect_err("the commit fails");
        assert!(matches!(error, Error::Io { .. }), "{:?}", error);
        assert!(self.store.read_only().is_some());
        assert!(matches!(self.store.commit(&tx(pad(0))), Err(Error::ReadOnly { .. })));
        assert_eq!(store_state(&self.store), state(&self.reference), "not applied");
        error
    }

    /// Drop the store (a crash), reopen through `StdFs`, apply the commit
    /// in flight to the reference if recovery must find it, and compare.
    fn reopen(mut self, in_flight: Option<(&[Mutation], InFlight)>) -> (TempDir, Store, Namespace) {
        drop(self.store);
        if let Some((mutations, InFlight::Kept)) = in_flight {
            self.reference.commit(mutations).unwrap();
        }
        let store = Store::open(self.dir.path(), self.opts).unwrap();
        assert_eq!(store_state(&store), state(&self.reference));
        (self.dir, store, self.reference)
    }
}

// The WAL append: the commit path

#[test]
fn a_failed_or_full_wal_write_loses_only_the_failed_commit() {
    for action in [Action::Fail, Action::NoSpace] {
        let c = case(1);
        c.fs.add(Rule::new(Call::Write, When::Before, action));
        let error = c.failed_commit(&tx(pad(9)));
        if let (Action::NoSpace, Error::Io { source, .. }) = (action, &error) {
            assert_eq!(source.kind(), std::io::ErrorKind::StorageFull, "{}", error);
        }
        let (_, store, _) = c.reopen(Some((&tx(pad(9)), InFlight::Lost)));
        assert!(store.recovery().torn_tail.is_none());
    }
}

#[test]
fn a_disk_full_halfway_through_a_frame_leaves_a_torn_tail_that_recovery_cuts() {
    let c = case(2);
    c.fs.add(Rule::new(Call::Write, When::Midway, Action::NoSpace));
    c.failed_commit(&tx(pad(9)));
    let (_, store, _) = c.reopen(Some((&tx(pad(9)), InFlight::Lost)));
    let tail = store.recovery().torn_tail.clone().expect("a torn tail");
    assert_eq!(tail.discarded_frames, 0);
}

#[test]
fn a_write_that_fails_after_reaching_the_file_is_recovered() {
    let c = case(3);
    c.fs.add(Rule::new(Call::Write, When::After, Action::Fail));
    c.failed_commit(&tx(pad(9)));
    c.reopen(Some((&tx(pad(9)), InFlight::Kept)));
}

#[test]
fn a_failed_wal_fsync_is_not_retried_and_its_record_is_recovered() {
    for (when, action) in [(When::Before, Action::Fail), (When::Before, Action::NoSpace), (When::After, Action::Fail)] {
        let c = case(4);
        c.fs.add(Rule::new(Call::Sync, when, action));
        c.failed_commit(&tx(pad(9)));
        let syncs = c.fs.count(Call::Sync);
        assert!(matches!(c.store.sync(), Err(Error::ReadOnly { .. })));
        // A read-only store still checkpoints what was synced, without an fsync
        assert!(c.store.checkpoint().is_ok());
        assert_eq!(c.fs.count(Call::Sync), syncs, "{:?} {:?}: never retried", when, action);
        // Its record was written before the fsync; the page cache has it
        c.reopen(Some((&tx(pad(9)), InFlight::Kept)));
    }
}

/// Each call of a segment rotation fails in turn: the commit that needed
/// the new segment is lost, the store is read-only, and reopening
/// recovers (a half-created segment is a temporary file, or a segment
/// with only its header).
#[test]
fn a_failed_rotation_loses_only_the_commit_that_needed_it() {
    let calls = [
        (Call::Create, Action::NoSpace),
        (Call::Write, Action::NoSpace),
        (Call::Sync, Action::Fail),
        (Call::Rename, Action::Fail),
        (Call::SyncDir, Action::Fail),
        (Call::OpenAppend, Action::Fail),
    ];
    for (call, action) in calls {
        let mut c = case(5);
        // The first big commit leaves records in its segment, so the
        // second must rotate
        c.store.commit(&big(1)).unwrap();
        c.reference.commit(&big(1)).unwrap();
        let segments = segment_seqs(c.dir.path()).len();
        c.fs.add(Rule::new(call, When::Before, action));
        c.failed_commit(&big(2));
        assert_eq!(c.fs.state().failed, vec![call]);
        let (dir, store, mut reference) = c.reopen(Some((&big(2), InFlight::Lost)));
        assert!(store.recovery().removed_temp_files.len() <= 1, "{:?}", call);
        // The recovered store rotates normally
        run(&store, &mut reference, &[Step::Tx(big(3)), Step::Tx(big(4))]);
        assert!(segment_seqs(dir.path()).len() > segments, "{:?}", call);
    }
}

/// The group commit timer's fsync fails: the store turns read-only
/// without a commit noticing, and recovery (after a process crash) finds
/// every acknowledged commit.
#[test]
fn a_failed_group_commit_timer_fsync_makes_the_store_read_only() {
    let mut opts = options(2);
    opts.wal.fsync = FsyncPolicy::Group { max_delay: Duration::from_millis(50), max_batch: 1000 };
    opts.wal.segment_size = iwdb_storage::DEFAULT_SEGMENT_SIZE;
    let c = case_with(opts, 6);
    c.store.sync().unwrap();
    c.fs.add(Rule::new(Call::Sync, When::Before, Action::Fail));
    let mut reference = c.reference;
    run(&c.store, &mut reference, &[pad(1), pad(2)]);
    let start = std::time::Instant::now();
    while c.store.read_only().is_none() {
        assert!(start.elapsed() < Duration::from_secs(10), "the timer fsynced");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(c.store.synced_seq() < c.store.seq());
    assert!(matches!(c.store.commit(&tx(pad(3))), Err(Error::ReadOnly { .. })));
    Case { reference, ..c }.reopen(None);
}

// Opening: recovery and initialization. A failed open changes nothing
// that the next open doesn't finish, and releases the lock.

/// A store whose last commit failed halfway through its frame, leaving a
/// torn tail. (If the commit rotates first, the failure tears the new
/// segment's header in its temporary file instead: then the next seed.)
fn crashed_with_torn_tail(seed: u64) -> (TempDir, Namespace) {
    for seed in seed.. {
        let c = case(seed);
        c.fs.add(Rule::new(Call::Write, When::Midway, Action::Fail));
        c.failed_commit(&tx(pad(9)));
        drop(c.store);
        let wal = c.dir.path().join("wal");
        let first = iwdb_storage::list_segments(&wal).unwrap()[0].0;
        let (_, end) = iwdb_storage::read_log(&wal, first).unwrap();
        if end.torn().is_some() {
            return (c.dir, c.reference);
        }
    }
    unreachable!()
}

fn open_fails(dir: &Path, rule: Rule) -> Error {
    let fs = TestFs::default();
    fs.add(rule.clone());
    let error = Store::open_with(fs.clone(), dir, options(2)).expect_err("the open fails");
    assert_eq!(fs.state().fired, vec![rule]);
    assert!(matches!(error, Error::Io { .. }), "{:?}", error);
    error
}

fn open_finishes(dir: &Path, reference: &Namespace) -> Store {
    let store = Store::open(dir, options(2)).unwrap();
    assert_eq!(store_state(&store), state(reference));
    store
}

#[test]
fn a_failed_truncation_of_the_torn_tail_is_finished_by_the_next_open() {
    for (when, action) in [(When::Before, Action::NoSpace), (When::After, Action::Fail)] {
        let (dir, reference) = crashed_with_torn_tail(7);
        open_fails(dir.path(), Rule::new(Call::Truncate, when, action));
        let store = open_finishes(dir.path(), &reference);
        // After: the cut happened although it reported an error
        assert_eq!(store.recovery().torn_tail.is_some(), when == When::Before);
    }
}

/// A last segment without a valid header (a crash while creating it
/// outside the writer's protocol) is removed by recovery; a failed removal
/// or directory sync fails the open, and the next open finishes it.
#[test]
fn a_failed_removal_of_a_headerless_segment_is_finished_by_the_next_open() {
    let c = case(8);
    drop(c.store);
    let next = c.reference.seq() + 1;
    let path = c.dir.path().join("wal").join(iwdb_storage::format::segment_name(next));
    std::fs::write(&path, &iwdb_storage::format::encode_segment_header(next)[..10]).unwrap();
    open_fails(c.dir.path(), Rule::new(Call::RemoveFile, When::Before, Action::Fail).path("/wal/"));
    assert!(path.exists());
    open_fails(c.dir.path(), Rule::new(Call::SyncDir, When::Before, Action::Fail).path("/wal"));
    assert!(!path.exists(), "removed before the sync failed");
    open_finishes(c.dir.path(), &c.reference);
}

#[test]
fn a_failed_removal_of_temporary_files_is_finished_by_the_next_open() {
    let c = case(9);
    drop(c.store);
    let tmp = c.dir.path().join("checkpoints").join(".00000000000000000099.ckpt.1.0.tmp");
    std::fs::write(&tmp, b"half").unwrap();
    // (Temporary directories are named .tmp*, so match the file's own name)
    open_fails(c.dir.path(), Rule::new(Call::RemoveFile, When::Before, Action::Fail).path("ckpt.1.0.tmp"));
    assert!(tmp.exists());
    // The directory sync after the removal fails: the file is gone
    open_fails(c.dir.path(), Rule::new(Call::SyncDir, When::Before, Action::Fail).path("/checkpoints"));
    let store = open_finishes(c.dir.path(), &c.reference);
    assert!(!tmp.exists() && store.recovery().removed_temp_files.is_empty());
}

#[test]
fn a_full_disk_when_the_writer_starts_fails_the_open_and_loses_nothing() {
    let c = case(10);
    drop(c.store);
    for rule in [
        Rule::new(Call::OpenAppend, When::Before, Action::Fail),
        Rule::new(Call::Sync, When::Before, Action::Fail),
        Rule::new(Call::Create, When::Before, Action::NoSpace),
        Rule::new(Call::Write, When::Midway, Action::NoSpace),
        Rule::new(Call::Rename, When::Before, Action::NoSpace),
        Rule::new(Call::SyncDir, When::Before, Action::Fail).path("/wal"),
    ] {
        open_fails(c.dir.path(), rule);
    }
    let store = open_finishes(c.dir.path(), &c.reference);
    let mut reference = c.reference;
    run(&store, &mut reference, &[pad(1)]);
}

#[test]
fn a_failed_initialization_is_finished_by_the_next_open() {
    for rule in [
        Rule::new(Call::SyncDir, When::Before, Action::Fail),
        Rule::new(Call::WriteAtomic, When::Before, Action::NoSpace).path(MARKER_NAME),
        Rule::new(Call::WriteAtomic, When::Midway, Action::NoSpace).path(MARKER_NAME),
        Rule::new(Call::WriteAtomic, When::WriterDone, Action::Fail).path(MARKER_NAME),
        Rule::new(Call::SyncDir, When::Before, Action::Fail).skip(1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        open_fails(dir.path(), rule.clone());
        let marker = dir.path().join(MARKER_NAME).exists();
        assert_eq!(marker, rule.skip == 1, "{}: the marker is written last", rule);
        let store = open_finishes(dir.path(), &reference());
        assert_eq!(store.recovery().created, !marker, "{}", rule);
        let mut reference = reference();
        run(&store, &mut reference, &workload(5, 11));
    }
}

// Checkpoints: a failure before the new file is in place is retried at the
// next checkpoint and deletes nothing; a failure after it disables
// checkpoints until reopening and deletes nothing more. Commits go on.

/// Every file with its bytes.
type Files = Vec<(std::path::PathBuf, Vec<u8>)>;

/// A case whose next checkpoint fails at `rule`: the case, the error, the
/// checkpoint files and the WAL segments before it.
fn failed_checkpoint(rule: Rule) -> (Case, Error, Files, Vec<u64>) {
    let c = case_with(options(1), 12);
    let before = snapshot(&c.dir.path().join("checkpoints"));
    let segments = segment_seqs(c.dir.path());
    c.fs.add(rule.clone());
    let error = c.store.checkpoint().expect_err("the checkpoint fails");
    assert_eq!(c.fs.state().fired, vec![rule]);
    assert!(c.store.checkpoint_failure().is_some());
    assert!(c.store.read_only().is_none(), "commits are not affected");
    (c, error, before, segments)
}

#[test]
fn a_checkpoint_that_fails_before_its_rename_deletes_nothing_and_is_retried() {
    for rule in [
        Rule::new(Call::WriteAtomic, When::Before, Action::NoSpace),
        Rule::new(Call::WriteAtomic, When::Midway, Action::NoSpace),
        Rule::new(Call::WriteAtomic, When::WriterDone, Action::Fail),
    ] {
        let (mut c, error, before, segments) = failed_checkpoint(rule.clone());
        assert!(matches!(error, Error::Io { op: "write checkpoint", .. }), "{}: {:?}", rule, error);
        assert_eq!(snapshot(&c.dir.path().join("checkpoints")), before, "{}: untouched, no temporary file", rule);
        assert_eq!(segment_seqs(c.dir.path()), segments, "{}", rule);
        run(&c.store, &mut c.reference, &[pad(1)]);
        let outcome = c.store.checkpoint().unwrap();
        assert_eq!(outcome.seq, c.reference.seq(), "{}: retried", rule);
        assert!(!outcome.removed_segments.is_empty());
        let (_, store, _) = c.reopen(None);
        assert_eq!(store.recovery().checkpoint, Some(outcome.seq));
    }
}

/// `write_atomic` renamed the file and then reported an error: the file
/// is a valid checkpoint, but the checkpointer treats the run as failed
/// and deletes nothing. Recovery may use it.
#[test]
fn a_checkpoint_whose_write_reports_an_error_after_the_rename_deletes_nothing() {
    let (c, error, _, segments) = failed_checkpoint(Rule::new(Call::WriteAtomic, When::After, Action::Fail));
    assert!(matches!(error, Error::Io { op: "write checkpoint", .. }), "{:?}", error);
    assert_eq!(checkpoints(c.dir.path()).len(), 2);
    assert_eq!(segment_seqs(c.dir.path()), segments);
    let newest = checkpoints(c.dir.path())[1];
    let (_, store, _) = c.reopen(None);
    assert_eq!(store.recovery().checkpoint, Some(newest));
}

/// Each step after the rename fails in turn: the directory sync, the
/// removal of the old checkpoint and its sync, the removal of WAL segments
/// (the first, or one in the middle) and its sync.
#[test]
fn a_checkpoint_that_fails_after_its_rename_disables_checkpoints_until_reopened() {
    let rules = [
        Rule::new(Call::SyncDir, When::Before, Action::Fail).path("/checkpoints"),
        Rule::new(Call::RemoveFile, When::Before, Action::Fail).path("/checkpoints/"),
        Rule::new(Call::SyncDir, When::Before, Action::Fail).path("/checkpoints").skip(1),
        Rule::new(Call::RemoveFile, When::Before, Action::Fail).path("/wal/"),
        Rule::new(Call::RemoveFile, When::Before, Action::Fail).path("/wal/").skip(1),
        Rule::new(Call::SyncDir, When::Before, Action::Fail).path("/wal"),
    ];
    for rule in rules {
        let (mut c, error, _, segments) = failed_checkpoint(rule.clone());
        assert!(matches!(error, Error::Io { .. }), "{}: {:?}", rule, error);
        let newest = *checkpoints(c.dir.path()).last().unwrap();
        assert_eq!(newest, c.reference.seq(), "{}: the new checkpoint is in place", rule);
        let removed = segments.len() - segment_seqs(c.dir.path()).len();
        match rule.skip {
            _ if rule.path == "/wal/" => assert_eq!(removed, rule.skip as usize, "{}", rule),
            _ if rule.path == "/wal" => assert!(removed > 0, "{}", rule),
            _ => assert_eq!(removed, 0, "{}", rule),
        }
        assert!(matches!(c.store.checkpoint(), Err(Error::CheckpointsDisabled { .. })), "{}", rule);
        run(&c.store, &mut c.reference, &[pad(1)]);
        let (dir, store, mut reference) = c.reopen(None);
        assert_eq!(store.recovery().checkpoint, Some(newest), "{}", rule);
        run(&store, &mut reference, &[pad(2)]);
        store.checkpoint().unwrap();
        assert_eq!(checkpoints(dir.path()).len(), 1, "{}: checkpoints work again", rule);
    }
}
