//! With `always`, every acknowledged commit is readable
//! after reopening the files, and replaying the log gives the same state.
//! Also the fsync policies (when fsyncs happen), the record size limit and
//! the rules for starting a writer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::time::Duration;

use common::{namespace, replay, segments, state, upsert, Call, TestFs};
use ironweaver_core::Value;
use iwdb_engine::testutil::workload::{seed, step, Step};
use iwdb_engine::{CommitRecord, CommitResult, Namespace};
use iwdb_storage::format::{MAX_RECORD_LEN, SEGMENT_HEADER_LEN};
use iwdb_storage::{
    read_log, Error, FsyncPolicy, LoggedNamespace, Wal, WalOptions, WalReader, DEFAULT_SEGMENT_SIZE, MIN_SEGMENT_SIZE,
};
use proptest::collection::vec;
use proptest::prelude::*;

fn options(fsync: FsyncPolicy, segment_size: u64) -> WalOptions {
    WalOptions { fsync, segment_size }
}

/// Run `steps` through a logged namespace and, in parallel, a plain
/// namespace (prepare / apply) that collects the records of the
/// acknowledged commits.
fn run(
    logged: &mut LoggedNamespace<impl iwdb_storage::io::LogFs>,
    reference: &mut Namespace,
    steps: &[Step],
    acknowledged: &mut Vec<CommitRecord>,
) {
    for step in steps {
        let (outcome, prepared) = match step {
            Step::Tx(mutations) => (logged.commit(mutations), reference.prepare(mutations)),
            Step::Catalog(change) => (logged.commit_catalog(change.clone()), reference.prepare_catalog(change.clone())),
        };
        match (outcome, prepared) {
            (Ok(result), Ok(prepared)) => {
                acknowledged.push(prepared.record().clone());
                // The log gives the commit its time; the reference has none
                assert!(result.time.is_some());
                assert_eq!(reference.apply(prepared, None).unwrap(), CommitResult { time: None, ..result });
            }
            (Err(Error::Engine(e)), Err(expected)) => assert_eq!(e, expected),
            (outcome, prepared) => panic!("logged {:?}, reference {:?}", outcome, prepared),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Commit a random workload with `always`, reopen the files, replay the
    /// records into an empty namespace: same records, canonical state,
    /// catalog and seq. Small segments make the log rotate often.
    #[test]
    fn acknowledged_commits_survive_reopening_and_replay_exactly(
        steps in (seed(), vec(step(), 1..40)).prop_map(|(seed, mut steps)| {
            steps.insert(0, seed);
            steps
        }),
        segment_size in prop_oneof![Just(MIN_SEGMENT_SIZE), Just(4096u64), Just(DEFAULT_SEGMENT_SIZE)],
        from in 0u64..50,
        more in vec(step(), 0..10),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let wal = Wal::create(dir.path(), options(FsyncPolicy::Always, segment_size), 1).unwrap();
        let mut logged = LoggedNamespace::new(namespace(), wal).unwrap();
        let mut reference = namespace();
        let mut acknowledged = Vec::new();
        run(&mut logged, &mut reference, &steps, &mut acknowledged);
        prop_assert_eq!(logged.wal().synced_seq(), logged.namespace().seq());
        let live = state(&logged.namespace());
        // No close: with `always` every acknowledged commit is synced already
        drop(logged);

        let (records, end) = read_log(dir.path(), 1).unwrap();
        prop_assert_eq!(&records, &acknowledged);
        prop_assert_eq!(end.next_seq, live.2 + 1);
        prop_assert!(end.torn().is_none());
        prop_assert_eq!(state(&replay(records.clone())), live.clone());

        // Reading from a later seq returns the suffix
        let (suffix, end_from) = match read_log(dir.path(), from) {
            Ok(read) => read,
            Err(Error::LogEndsBefore { .. }) if from > live.2 + 1 => return Ok(()),
            Err(e) => return Err(TestCaseError::fail(e.to_string())),
        };
        let skip = records.iter().take_while(|r| r.seq < from).count();
        prop_assert_eq!(&suffix[..], &records[skip..]);
        prop_assert_eq!(end_from, end);

        // A second writer session continues the log in a new segment
        let before = segments(dir.path()).len();
        let wal = Wal::create(dir.path(), options(FsyncPolicy::Always, segment_size), live.2 + 1).unwrap();
        let mut logged = LoggedNamespace::new(replay(records), wal).unwrap();
        run(&mut logged, &mut reference, &more, &mut acknowledged);
        prop_assert!(segments(dir.path()).len() > before);
        let live = state(&logged.namespace());
        drop(logged);
        let (records, _) = read_log(dir.path(), 1).unwrap();
        prop_assert_eq!(&records, &acknowledged);
        prop_assert_eq!(state(&replay(records)), live);
    }
}

fn logged_with(fs: &TestFs, dir: &std::path::Path, fsync: FsyncPolicy, segment_size: u64) -> LoggedNamespace<TestFs> {
    let wal = Wal::create_with(fs.clone(), dir, options(fsync, segment_size), 1).unwrap();
    LoggedNamespace::new(namespace(), wal).unwrap()
}

#[test]
fn always_syncs_every_commit_and_every_new_segment() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let logged = logged_with(&fs, dir.path(), FsyncPolicy::Always, MIN_SEGMENT_SIZE);
    // Segment creation: header write, file sync, rename, directory sync
    assert_eq!((fs.count(Call::Sync), fs.count(Call::SyncDir), fs.count(Call::Rename)), (1, 1, 1));
    for i in 0..100 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
        assert_eq!(logged.wal().synced_seq(), logged.namespace().seq());
    }
    let n = segments(dir.path()).len();
    assert!(n > 2, "{} segments", n);
    assert_eq!(fs.count(Call::Sync), 100 + n);
    assert_eq!(fs.count(Call::SyncDir), n);
}

#[test]
fn group_commit_syncs_every_batch_and_when_due() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let group = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 3 };
    let logged = logged_with(&fs, dir.path(), group, DEFAULT_SEGMENT_SIZE);
    let syncs = fs.count(Call::Sync);
    for i in 1..=7 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
        // The commit that makes the batch full waits for the fsync
        assert_eq!(logged.wal().synced_seq(), (i as u64 / 3) * 3, "after commit {}", i);
    }
    assert_eq!(fs.count(Call::Sync), syncs + 2);
    // Not due yet
    assert!(!logged.sync_due().unwrap());
    logged.sync().unwrap();
    assert_eq!(logged.wal().synced_seq(), 7);
    assert_eq!(fs.count(Call::Sync), syncs + 3);
    // Nothing to sync
    logged.sync().unwrap();
    assert_eq!(fs.count(Call::Sync), syncs + 3);

    // By age: a record older than max_delay is synced by the next commit or
    // by sync_due
    let dir = tempfile::tempdir().unwrap();
    let group = FsyncPolicy::Group { max_delay: Duration::from_millis(20), max_batch: 1000 };
    let logged = logged_with(&fs, dir.path(), group, DEFAULT_SEGMENT_SIZE);
    logged.commit(&[upsert("a", Value::Int(1))]).unwrap();
    assert_eq!(logged.wal().synced_seq(), 0);
    std::thread::sleep(Duration::from_millis(30));
    logged.commit(&[upsert("a", Value::Int(2))]).unwrap();
    assert_eq!(logged.wal().synced_seq(), 2);
    logged.commit(&[upsert("a", Value::Int(3))]).unwrap();
    assert!(!logged.sync_due().unwrap());
    std::thread::sleep(Duration::from_millis(30));
    assert!(logged.sync_due().unwrap());
    assert_eq!(logged.wal().synced_seq(), 3);
    assert!(!logged.sync_due().unwrap());

    // close syncs what's left
    logged.commit(&[upsert("a", Value::Int(4))]).unwrap();
    let before = fs.count(Call::Sync);
    logged.close().unwrap();
    assert_eq!(fs.count(Call::Sync), before + 1);
}

#[test]
fn off_never_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let logged = logged_with(&fs, dir.path(), FsyncPolicy::Off, MIN_SEGMENT_SIZE);
    for i in 0..100 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
    }
    assert!(segments(dir.path()).len() > 2);
    assert!(!logged.sync_due().unwrap());
    logged.close().unwrap();
    assert_eq!((fs.count(Call::Sync), fs.count(Call::SyncDir)), (0, 0));
    // Still a complete log (a process crash loses nothing)
    let (records, end) = read_log(dir.path(), 1).unwrap();
    assert_eq!((records.len(), end.next_seq), (100, 101));
}

/// With `off`, an explicit sync makes every record durable, as it says:
/// also the segments that rotations closed without an fsync, those an
/// earlier writer left, and the directory. Until then `synced_seq` claims
/// nothing. (In step 4 a new writer claimed every earlier record synced,
/// and a sync covered only the current segment.)
#[test]
fn off_syncs_every_unsynced_segment_on_an_explicit_sync() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let synced = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = synced.clone();
    fs.set_hook(Some(std::sync::Arc::new(move |call, path: &std::path::Path| {
        if call == Call::Sync {
            seen.lock().unwrap().push(path.to_path_buf());
        }
    })));
    let off = options(FsyncPolicy::Off, MIN_SEGMENT_SIZE);
    let logged = logged_with(&fs, dir.path(), FsyncPolicy::Off, MIN_SEGMENT_SIZE);
    for i in 0..60 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
    }
    drop(logged);
    let first = segments(dir.path());
    assert!(first.len() > 2);

    // A new writer knows nothing about the earlier writer's fsyncs
    let wal = Wal::create_with(fs.clone(), dir.path(), off.clone(), 61).unwrap();
    assert_eq!(wal.synced_seq(), 0, "nothing is known to be durable");
    let (records, _) = read_log(dir.path(), 1).unwrap();
    let logged = LoggedNamespace::new(replay(records), wal).unwrap();
    for i in 60..120 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
    }
    assert_eq!(logged.wal().synced_seq(), 0);
    assert_eq!((fs.count(Call::Sync), fs.count(Call::SyncDir)), (0, 0));

    logged.sync().unwrap();
    assert_eq!(logged.wal().synced_seq(), 120);
    let synced = synced.lock().unwrap().clone();
    for segment in segments(dir.path()) {
        assert!(synced.contains(&segment), "{} was synced", segment.display());
    }
    assert_eq!(fs.count(Call::SyncDir), 1, "the directory too");
    // Frames written after the sync say so; a sync without new records
    // does nothing
    let calls = fs.count(Call::Sync);
    logged.sync().unwrap();
    assert_eq!(fs.count(Call::Sync), calls);
    logged.commit(&[upsert("a", Value::Int(120))]).unwrap();
    logged.sync().unwrap();
    assert_eq!(fs.count(Call::Sync), calls + 1, "only the segment with new records");
    let (records, _) = read_log(dir.path(), 1).unwrap();
    assert_eq!(records.len(), 121);
}

/// With `off`, a segment that disappeared before the sync (the
/// checkpointer removed it, a checkpoint holds its records) is skipped.
#[test]
fn off_sync_skips_a_segment_removed_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let logged = logged_with(&fs, dir.path(), FsyncPolicy::Off, MIN_SEGMENT_SIZE);
    for i in 0..60 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
    }
    let segments = segments(dir.path());
    assert!(segments.len() > 2);
    fs::remove_file(&segments[0]).unwrap();
    logged.sync().unwrap();
    assert_eq!(logged.wal().synced_seq(), 60);
    assert!(logged.read_only().is_none());
    assert_eq!(fs.count(Call::Sync), segments.len() - 1);
}

#[test]
fn a_record_above_the_limit_is_rejected_before_anything_happens() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::create(dir.path(), WalOptions::default(), 1).unwrap();
    let logged = LoggedNamespace::new(namespace(), wal).unwrap();
    logged.commit(&[upsert("a", Value::Int(1))]).unwrap();
    let before = state(&logged.namespace());

    let big = Value::String("x".repeat(MAX_RECORD_LEN as usize));
    match logged.commit(&[upsert("b", big)]) {
        Err(Error::RecordTooLarge { seq: 2, len, max }) => assert!(len > max && max == MAX_RECORD_LEN as usize),
        other => panic!("{:?}", other),
    }
    assert_eq!(state(&logged.namespace()), before);
    assert!(logged.read_only().is_none());
    logged.commit(&[upsert("b", Value::Int(2))]).unwrap();
    drop(logged);

    let (records, end) = read_log(dir.path(), 1).unwrap();
    assert_eq!((records.len(), end.next_seq), (2, 3));
    assert!(end.torn().is_none());
}

#[test]
fn a_writer_never_overwrites_records() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::create(dir.path(), WalOptions::default(), 1).unwrap();
    let logged = LoggedNamespace::new(namespace(), wal).unwrap();
    for i in 0..3 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
    }
    drop(logged);

    // A log with records at or after next_seq, or ending before it
    for next_seq in [1, 2, 3] {
        assert!(matches!(
            Wal::create(dir.path(), WalOptions::default(), next_seq),
            Err(Error::LogAhead { first_seq: 1, .. })
        ));
    }
    assert!(matches!(
        Wal::create(dir.path(), WalOptions::default(), 5),
        Err(Error::LogEndsBefore { from: 5, next_seq: 4 })
    ));
    // Only possible at the end; the previous segment keeps its records
    let wal = Wal::create(dir.path(), WalOptions::default(), 4).unwrap();
    // A header-only segment at next_seq (a crash right after rotating) is replaced
    drop(wal);
    assert_eq!(fs::metadata(dir.path().join("00000000000000000004.wal")).unwrap().len(), SEGMENT_HEADER_LEN as u64);
    let wal = Wal::create(dir.path(), WalOptions::default(), 4).unwrap();
    assert_eq!(wal.next_seq(), 4);
    drop(wal);
    let (records, end) = read_log(dir.path(), 1).unwrap();
    assert_eq!((records.len(), end.next_seq), (3, 4));

    // A torn tail must be truncated (recovered) first
    let last = segments(dir.path()).pop().unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&last)
        .map(|mut f| std::io::Write::write_all(&mut f, &[1, 2, 3]))
        .unwrap()
        .unwrap();
    assert!(matches!(
        Wal::create(dir.path(), WalOptions::default(), 4),
        Err(Error::TornTail { valid_len, .. }) if valid_len == SEGMENT_HEADER_LEN as u64
    ));
    fs::OpenOptions::new().write(true).open(&last).unwrap().set_len(SEGMENT_HEADER_LEN as u64).unwrap();

    // Out-of-range starts, and a namespace that doesn't match its log
    assert!(matches!(Wal::create(dir.path(), WalOptions::default(), 0), Err(Error::InvalidOptions(_))));
    let wal = Wal::create(dir.path(), WalOptions::default(), 4).unwrap();
    assert!(matches!(LoggedNamespace::new(namespace(), wal), Err(Error::OutOfOrder { expected: 1, found: 4 })));
}

#[test]
fn appends_must_come_in_seq_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::create(dir.path(), WalOptions::default(), 1).unwrap();
    let record = |seq| CommitRecord::new(seq, iwdb_engine::Change::Data(vec![]));
    wal.append(&record(1)).unwrap();
    assert!(matches!(wal.append(&record(3)), Err(Error::OutOfOrder { expected: 2, found: 3 })));
    assert!(matches!(wal.append(&record(1)), Err(Error::OutOfOrder { expected: 2, found: 1 })));
    // Still usable
    assert!(wal.failure().is_none());
    wal.append(&record(2)).unwrap();
    wal.close().unwrap();
    let (records, _) = read_log(dir.path(), 1).unwrap();
    assert_eq!(records, vec![record(1), record(2)]);
}

#[test]
fn reading_from_a_seq_needs_the_records_from_there() {
    let dir = tempfile::tempdir().unwrap();
    // An empty directory: nothing to read, from anywhere
    let (records, end) = read_log(dir.path(), 5).unwrap();
    assert!(records.is_empty() && end.next_seq == 5 && end.last_segment.is_none());

    // A log from seq 10 (as after a checkpoint deleted older segments)
    let wal = Wal::create(dir.path(), options(FsyncPolicy::Always, MIN_SEGMENT_SIZE), 10).unwrap();
    let mut ns = namespace();
    for _ in 1..10 {
        ns.commit(&[upsert("pad", Value::Int(0))]).unwrap();
    }
    let logged = LoggedNamespace::new(ns, wal).unwrap();
    for i in 0..30 {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
    }
    drop(logged);
    assert!(matches!(read_log(dir.path(), 9), Err(Error::MissingRecords { from: 9, first_seq: 10 })));
    let (records, end) = read_log(dir.path(), 10).unwrap();
    assert_eq!((records.len(), records[0].seq, end.next_seq), (30, 10, 40));
    let (records, end) = read_log(dir.path(), 25).unwrap();
    assert_eq!((records.len(), records[0].seq, end.next_seq), (15, 25, 40));
    // From the end: nothing, but a valid end
    let (records, end) = read_log(dir.path(), 40).unwrap();
    assert!(records.is_empty() && end.next_seq == 40);
    assert!(matches!(read_log(dir.path(), 41), Err(Error::LogEndsBefore { from: 41, next_seq: 40 })));

    // The iterator reports the end once it has returned None
    let mut reader = WalReader::open(dir.path(), 30).unwrap();
    assert!(reader.end().is_none());
    assert_eq!(reader.by_ref().map(|r| r.unwrap().seq).collect::<Vec<_>>(), (30..40).collect::<Vec<_>>());
    assert_eq!(reader.end().map(|e| e.next_seq), Some(40));
}
