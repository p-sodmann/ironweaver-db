//! The change stream of the embedded store (ADR 0031): a consumer resumes
//! after restarts without gaps or duplicates and rebuilds the namespace
//! from the events; retention keeps segments for it; and after an OS crash
//! under group commit no streamed commit is taken back.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwdb::{
    BatchLimits, CheckpointOptions, Error, FsyncPolicy, LogFs, Namespace, ReadOptions, Store, StoreOptions, WalOptions,
    WalRetention,
};
use iwdb_engine::CommitRecord;
use iwdb_engine::testutil::workload::{Step, Stream};
use iwdb_storage::MIN_SEGMENT_SIZE;
use iwdb_storage::failpoint::{Call, FailFs};
use support::{reference, state};

fn options(fsync: FsyncPolicy, retention: WalRetention) -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync, segment_size: MIN_SEGMENT_SIZE },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: true, keep: 1, background: false },
        create_if_missing: true,
        archive: None,
        retention,
        memory: Default::default(),
    }
}

/// Commit the next `n` steps of the workload (failing steps commit
/// nothing).
fn commit_steps<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, steps: &mut Stream, n: usize)
where
    F::File: Send,
{
    for step in steps.take(n) {
        let _ = match step {
            Step::Tx(mutations) => store.commit(&mutations).map(drop),
            Step::Catalog(change) => store.commit_catalog(change).map(drop),
        };
    }
}

/// A consumer of the change stream that keeps a copy of the namespace,
/// and remembers where it is (as it would store it with its copy).
struct Consumer {
    next_seq: u64,
    seen: Vec<u64>,
    mirror: Namespace,
}

impl Consumer {
    fn new() -> Self {
        Consumer { next_seq: 1, seen: Vec::new(), mirror: reference() }
    }

    /// Read up to `batches` batches of up to `size` commits.
    fn follow<F: LogFs + Clone + Send + Sync + 'static>(&mut self, store: &Store<F>, batches: usize, size: usize)
    where
        F::File: Send,
    {
        let limits = BatchLimits { max_records: size, max_bytes: usize::MAX };
        let ns = store.namespace(iwdb::NAMESPACE).unwrap();
        for _ in 0..batches {
            let batch = ns.changes(self.next_seq, limits, false, &ReadOptions::default()).unwrap();
            if batch.records.is_empty() {
                return;
            }
            for r in batch.records {
                self.seen.push(r.record.seq);
                let record = CommitRecord::new(r.record.seq, r.record.change);
                self.mirror.replay(record, r.time).unwrap();
            }
            self.next_seq = batch.next_seq;
        }
    }
}

#[test]
fn a_consumer_resumes_after_restarts_without_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let retention = WalRetention { records: 100_000, age: None };
    let mut steps = Stream::new(13);
    let mut consumer = Consumer::new();
    for round in 0..6 {
        let store = Store::open(dir.path(), options(FsyncPolicy::Always, retention)).unwrap();
        commit_steps(&store, &mut steps, 40);
        // Part of the way, in batches of different sizes
        consumer.follow(&store, 3, 5 + round);
        store.checkpoint().unwrap();
        consumer.follow(&store, 2, 3);
        if round % 2 == 0 {
            store.close().unwrap();
        } else {
            // Like a crash of the process: nothing is lost under `always`
            drop(store);
        }
    }
    let store = Store::open(dir.path(), options(FsyncPolicy::Always, retention)).unwrap();
    consumer.follow(&store, usize::MAX, 50);
    let seq = store.seq();
    assert!(seq > 150, "the workload committed: {}", seq);
    assert_eq!(consumer.seen, (1..=seq).collect::<Vec<_>>(), "every commit once, in order");
    let (graph, catalog, mirror_seq, _) = state(&consumer.mirror);
    let (expected_graph, expected_catalog, expected_seq, _) = store.read(state);
    assert_eq!((graph, catalog, mirror_seq), (expected_graph, expected_catalog, expected_seq));
    store.close().unwrap();
}

fn first_retained(store: &Store) -> u64 {
    let ns = store.namespace(iwdb::NAMESPACE).unwrap();
    let all = BatchLimits { max_records: 1, max_bytes: usize::MAX };
    ns.changes(store.seq(), all, false, &ReadOptions::default()).unwrap().first_seq
}

/// What a retention should keep after a checkpoint.
#[derive(Clone, Copy, Debug)]
enum Keeps {
    /// Every segment: seq 1 is still there.
    All,
    /// The segments holding the last `n` commits, not much more.
    Last(u64),
    /// Only what the checkpoint needs: the last segment or so.
    Nothing,
}

#[test]
fn retention_keeps_segments_that_checkpoints_no_longer_need() {
    let hour = Some(Duration::from_secs(3600));
    let cases = [
        (WalRetention::default(), Keeps::Nothing),
        (WalRetention { records: 30, age: None }, Keeps::Last(30)),
        (WalRetention { records: 1_000, age: None }, Keeps::All),
        (WalRetention { records: 0, age: hour }, Keeps::All),
        (WalRetention { records: 0, age: Some(Duration::ZERO) }, Keeps::Nothing),
    ];
    for (retention, keeps) in cases {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), options(FsyncPolicy::Always, retention)).unwrap();
        commit_steps(&store, &mut Stream::new(5), 200);
        store.checkpoint().unwrap();
        let (seq, first) = (store.seq(), first_retained(&store));
        let ns = store.namespace(iwdb::NAMESPACE).unwrap();
        let limits = BatchLimits { max_records: usize::MAX, max_bytes: usize::MAX };
        let from_one = ns.changes(1, limits, false, &ReadOptions::default());
        let what = format!("{:?}: first {} of {}", keeps, first, seq);
        match keeps {
            Keeps::All => {
                assert_eq!(first, 1, "{}", what);
                assert_eq!(from_one.unwrap().records.len() as u64, seq, "{}", what);
            }
            Keeps::Last(n) => {
                // 1 KiB segments hold a few commits each
                assert!(first > 1 && first <= seq + 1 - n && first > seq - 2 * n, "{}", what);
                assert!(matches!(from_one, Err(Error::NotRetained { from: 1, .. })), "{}", what);
            }
            Keeps::Nothing => {
                assert!(first > seq - 10, "{}", what);
                let error = from_one.unwrap_err();
                assert!(matches!(error, Error::NotRetained { from: 1, first_seq } if first_seq == first), "{}", what);
            }
        }
        store.close().unwrap();
    }
}

/// The largest length each WAL file was fsynced at.
type Synced = Arc<Mutex<HashMap<PathBuf, u64>>>;

fn recording_fs() -> (FailFs, Synced) {
    let fs = FailFs::new();
    let synced: Synced = Arc::default();
    let log = synced.clone();
    fs.set_hook(Some(Arc::new(move |call, path: &Path| {
        if call == Call::Sync
            && let Ok(meta) = std::fs::metadata(path)
        {
            let mut log = log.lock().unwrap();
            let len = log.entry(path.to_path_buf()).or_default();
            *len = (*len).max(meta.len());
        }
    })));
    (fs, synced)
}

/// An OS crash: each WAL file loses what was written after its last fsync.
/// Returns the bytes lost.
fn os_crash(dir: &Path, synced: &HashMap<PathBuf, u64>) -> u64 {
    let mut lost = 0;
    let wal = dir.join("ns").read_dir().unwrap().map(|e| e.unwrap().path().join("wal"));
    for segment in wal.flat_map(|w| w.read_dir().unwrap().map(|e| e.unwrap().path()).collect::<Vec<_>>()) {
        let len = std::fs::metadata(&segment).unwrap().len();
        let durable = synced.get(&segment).copied().unwrap_or(0).min(len);
        if durable < len {
            std::fs::OpenOptions::new().write(true).open(&segment).unwrap().set_len(durable).unwrap();
            lost += len - durable;
        }
    }
    lost
}

/// Under group commit, acknowledged commits wait for their fsync before
/// they are streamed. A crash then loses unsynced commits, and the store
/// gives their seqs to new commits; a consumer never saw the lost ones, so
/// what it saw is still the log's prefix.
#[test]
fn an_os_crash_under_group_commit_takes_back_no_streamed_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, synced) = recording_fs();
    let group = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 1_000_000 };
    let mut steps = Stream::new(29);
    let mut consumer = Consumer::new();
    let applied = {
        let store = Store::open_with(fs.clone(), dir.path(), options(group, WalRetention::default())).unwrap();
        commit_steps(&store, &mut steps, 30);
        store.sync().unwrap();
        commit_steps(&store, &mut steps, 30);
        consumer.follow(&store, usize::MAX, 7);
        // Rotations fsynced some segments, but the last commits wait
        let ns = store.namespace(iwdb::NAMESPACE).unwrap();
        assert!(ns.streamable_seq() < store.seq(), "some commits aren't synced yet");
        assert_eq!(consumer.next_seq, ns.streamable_seq() + 1, "the consumer has everything streamable");
        store.seq()
    };
    // The process and the OS crash: no close, unsynced bytes are gone
    assert!(os_crash(dir.path(), &synced.lock().unwrap()) > 0, "the crash lost something");
    let seen: Vec<_> = {
        let store = Store::open(dir.path(), options(group, WalRetention::default())).unwrap();
        assert!(store.seq() < applied, "commits were lost: {} of {}", store.seq(), applied);
        assert!(store.seq() >= consumer.next_seq - 1, "the crash lost a streamed commit");
        // New commits reuse the lost seqs, with other content
        commit_steps(&store, &mut Stream::new(31), 40);
        store.sync().unwrap();
        let mut after = Consumer::new();
        after.follow(&store, usize::MAX, 100);
        assert!(after.seen.len() as u64 >= applied, "the lost seqs were reused");
        // The commits the consumer saw before the crash are unchanged
        let ns = store.namespace(iwdb::NAMESPACE).unwrap();
        let all = BatchLimits { max_records: usize::MAX, max_bytes: usize::MAX };
        let events = ns.changes(1, all, false, &ReadOptions::default()).unwrap().records;
        events.into_iter().take(consumer.seen.len()).map(|r| (r.record.seq, r.record.change)).collect()
    };
    // Replaying what the consumer saw and what is there now agrees
    let mut replay = reference();
    for (seq, change) in seen {
        replay.replay(CommitRecord::new(seq, change), None).unwrap();
    }
    assert_eq!(state(&replay).0, state(&consumer.mirror).0);
    assert_eq!(replay.seq(), consumer.mirror.seq());
}
