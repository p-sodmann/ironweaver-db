//! Failure semantics: when a write, fsync or rotation of the log fails, the
//! commit is not applied, the namespace becomes read-only until it is
//! reopened, a failed fsync is never retried, and the log on disk holds
//! every acknowledged commit (plus, at most, the failed one, whose outcome
//! is unknown).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::{namespace, replay, segments, state, upsert, Call, Fault, TestFs};
use ironweaver_core::Value;
use iwdb_engine::testutil::State;
use iwdb_engine::{Change, CommitRecord};
use iwdb_storage::format::{Damage, SEGMENT_HEADER_LEN};
use iwdb_storage::{read_log, Error, FsyncPolicy, LoggedNamespace, Wal, WalOptions, MIN_SEGMENT_SIZE};

fn logged(fs: &TestFs, dir: &Path, fsync: FsyncPolicy, segment_size: u64) -> LoggedNamespace<TestFs> {
    let wal = Wal::create_with(fs.clone(), dir, WalOptions { fsync, segment_size }, 1).unwrap();
    LoggedNamespace::new(namespace(), wal).unwrap()
}

/// Commit `n` small transactions.
fn commit_n(logged: &mut LoggedNamespace<TestFs>, n: i64) {
    for i in 0..n {
        logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
    }
}

/// After a failed commit: the namespace is unchanged and read-only, every
/// write is refused, and nothing touches the files again.
fn assert_read_only(logged: &mut LoggedNamespace<TestFs>, fs: &TestFs, before: &State) {
    assert_eq!(state(&logged.namespace()).0, before.0);
    assert_eq!(logged.namespace().seq(), before.2);
    assert!(logged.read_only().is_some());
    let calls = fs.state().calls.len();
    assert!(matches!(logged.commit(&[upsert("z", Value::Int(0))]), Err(Error::ReadOnly { .. })));
    assert!(matches!(logged.commit_catalog(index("y")), Err(Error::ReadOnly { .. })));
    assert!(matches!(logged.sync(), Err(Error::ReadOnly { .. })));
    assert!(matches!(logged.sync_due(), Err(Error::ReadOnly { .. })));
    // No retry: no write or fsync after the failure
    assert_eq!(fs.state().calls.len(), calls);
    assert_eq!(logged.namespace().seq(), before.2);
}

fn index(key: &str) -> iwdb_engine::CatalogChange {
    let path = iwdb_engine::catalog::AttrPath::new([key]).unwrap();
    iwdb_engine::CatalogChange::CreateIndex(iwdb_engine::catalog::IndexDef { path })
}

#[test]
fn a_failed_write_is_not_applied_and_makes_the_namespace_read_only() {
    for fault in [Fault::Fail, Fault::Partial] {
        let dir = tempfile::tempdir().unwrap();
        let fs = TestFs::default();
        let mut logged = logged(&fs, dir.path(), FsyncPolicy::Always, 1 << 20);
        commit_n(&mut logged, 3);
        let before = state(&logged.namespace());
        let valid_len = std::fs::metadata(logged.wal().segment_path()).unwrap().len();

        fs.inject(Call::Write, fault);
        match logged.commit(&[upsert("b", Value::Int(1))]) {
            Err(Error::Io { op: "append", .. }) => {}
            other => panic!("{:?}", other),
        }
        assert_read_only(&mut logged, &fs, &before);
        drop(logged);

        // The log holds exactly the acknowledged commits; a partial write is
        // a torn tail right after them
        let (records, end) = read_log(dir.path(), 1).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(state(&replay(records)), before);
        let last = end.last_segment.unwrap();
        assert_eq!(last.valid_len, valid_len);
        match fault {
            Fault::Fail => assert!(last.torn.is_none()),
            Fault::Partial => {
                let torn = last.torn.unwrap();
                assert_eq!((torn.damage, torn.discarded_frames), (Damage::Truncated, 0));
                assert!(last.file_len > valid_len);
            }
        }
    }
}

#[test]
fn a_failed_fsync_is_not_applied_not_retried_and_makes_the_namespace_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let mut logged = logged(&fs, dir.path(), FsyncPolicy::Always, 1 << 20);
    commit_n(&mut logged, 3);
    let before = state(&logged.namespace());

    fs.inject(Call::Sync, Fault::Fail);
    match logged.commit(&[upsert("b", Value::Int(1))]) {
        Err(Error::Io { op: "fsync", .. }) => {}
        other => panic!("{:?}", other),
    }
    assert_eq!(logged.wal().synced_seq(), 3);
    assert_read_only(&mut logged, &fs, &before);
    // Closing doesn't sync either
    assert!(matches!(logged.close(), Err(Error::ReadOnly { .. })));
    assert_eq!(fs.state().failed, vec![Call::Sync]);

    // The record was written before the fsync failed, so here (the page
    // cache survived) it is in the log: the failed commit's outcome is
    // unknown, and recovery may find it
    let (records, end) = read_log(dir.path(), 1).unwrap();
    assert_eq!((records.len(), end.next_seq), (4, 5));
    assert!(end.torn().is_none());
}

#[test]
fn a_failed_group_fsync_fails_the_commit_that_waits_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let group = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 3 };
    let mut logged = logged(&fs, dir.path(), group, 1 << 20);
    commit_n(&mut logged, 3);
    // Two commits acknowledged without an fsync
    commit_n(&mut logged, 2);
    assert_eq!(logged.wal().synced_seq(), 3);
    let before = state(&logged.namespace());

    fs.inject(Call::Sync, Fault::Fail);
    assert!(matches!(logged.commit(&[upsert("b", Value::Int(1))]), Err(Error::Io { op: "fsync", .. })));
    assert_read_only(&mut logged, &fs, &before);
}

#[test]
fn a_failed_sync_due_makes_the_namespace_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let group = FsyncPolicy::Group { max_delay: Duration::from_millis(20), max_batch: 1000 };
    let mut logged = logged(&fs, dir.path(), group, 1 << 20);
    commit_n(&mut logged, 1);
    assert_eq!(logged.wal().synced_seq(), 0);
    let before = state(&logged.namespace());
    std::thread::sleep(Duration::from_millis(30));
    fs.inject(Call::Sync, Fault::Fail);
    assert!(matches!(logged.sync_due(), Err(Error::Io { op: "fsync", .. })));
    assert_read_only(&mut logged, &fs, &before);
}

#[test]
fn a_failed_rotation_is_not_applied_and_makes_the_namespace_read_only() {
    for call in [Call::Create, Call::Write, Call::Sync, Call::Rename, Call::SyncDir, Call::OpenAppend] {
        let dir = tempfile::tempdir().unwrap();
        let fs = TestFs::default();
        let mut logged = logged(&fs, dir.path(), FsyncPolicy::Always, MIN_SEGMENT_SIZE);
        // Commit until the next commit rotates: the segment is nearly full
        let mut i = 0;
        loop {
            logged.commit(&[upsert("a", Value::Int(i))]).unwrap();
            i += 1;
            let len = std::fs::metadata(logged.wal().segment_path()).unwrap().len();
            if len + 200 > MIN_SEGMENT_SIZE {
                break;
            }
        }
        let before = state(&logged.namespace());
        let (creates, rotations) = (fs.count(Call::Create), fs.count(Call::Rename));

        // A record large enough to need a new segment
        fs.inject(call, Fault::Fail);
        let big = Value::String("r".repeat(300));
        match logged.commit(&[upsert("b", big)]) {
            Err(Error::Io { .. }) => {}
            other => panic!("{:?}: {:?}", call, other),
        }
        assert_eq!(fs.state().failed, vec![call]);
        // The failure hit the rotation, not the record's own write or fsync
        assert_eq!(fs.count(Call::Create), creates + 1, "{:?}", call);
        if call != Call::Create && call != Call::Write && call != Call::Sync {
            assert_eq!(fs.count(Call::Rename), rotations + 1, "{:?}", call);
        }
        assert_read_only(&mut logged, &fs, &before);
        drop(logged);

        // The log holds exactly the acknowledged commits; a new segment
        // that got its header before the failure is empty
        let (records, end) = read_log(dir.path(), 1).unwrap();
        assert_eq!(records.len() as u64, before.2, "{:?}", call);
        assert!(end.torn().is_none());
        assert_eq!(state(&replay(records)), before);
        let last = segments(dir.path()).pop().unwrap();
        if matches!(call, Call::SyncDir | Call::OpenAppend) {
            assert_eq!(std::fs::metadata(last).unwrap().len(), SEGMENT_HEADER_LEN as u64);
        }
    }
}

#[test]
fn a_poisoned_namespace_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let wal = Wal::create_with(fs.clone(), dir.path(), WalOptions::default(), 1).unwrap();
    // A record the graph rejects poisons the namespace (a validated record
    // can't fail to apply through the public API, short of a bug)
    let mut ns = namespace();
    let bad = CommitRecord::new(1, Change::Data(vec![ironweaver_core::Op::RemoveNode { id: "x".into() }]));
    assert!(matches!(ns.replay(bad, None), Err(iwdb_engine::Error::ApplyFailed { .. })));
    let logged = LoggedNamespace::new(ns, wal).unwrap();
    let before = state(&logged.namespace());
    assert!(logged.read_only().is_some());
    let calls = fs.state().calls.len();
    assert!(matches!(logged.commit(&[upsert("a", Value::Int(0))]), Err(Error::ReadOnly { .. })));
    assert!(matches!(logged.commit_catalog(index("x")), Err(Error::ReadOnly { .. })));
    assert_eq!(fs.state().calls.len(), calls);
    assert_eq!(state(&logged.namespace()), before);
}

#[test]
fn a_failed_segment_creation_fails_the_writer() {
    for call in [Call::Create, Call::Write, Call::Sync, Call::Rename, Call::SyncDir, Call::OpenAppend] {
        let dir = tempfile::tempdir().unwrap();
        let fs = TestFs::default();
        fs.inject(call, Fault::Fail);
        assert!(matches!(Wal::create_with(fs.clone(), dir.path(), WalOptions::default(), 1), Err(Error::Io { .. })));
    }
}
