//! The change stream's WAL reader (`OffsetIndex`, ADR 0031): every range
//! reads like the full reader, cold and warm; limits; seqs that are no
//! longer retained; damage in the range; the streamable seq.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::assert_matches;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{frame_ends, namespace, segments, upsert};
use ironweaver_core::Value;
use iwdb_engine::CommitRecord;
use iwdb_storage::changes::MARK_EVERY;
use iwdb_storage::{
    BatchLimits, Error, FsyncPolicy, LoggedNamespace, MIN_SEGMENT_SIZE, OffsetIndex, Wait, Wal, WalOptions, read_log,
};

const ALL: BatchLimits = BatchLimits { max_records: usize::MAX, max_bytes: usize::MAX };

/// Write `n` commits of varying size into small segments.
fn write_log(dir: &Path, n: usize) -> Vec<CommitRecord> {
    let wal = Wal::create(dir, WalOptions { fsync: FsyncPolicy::Off, segment_size: MIN_SEGMENT_SIZE }, 1).unwrap();
    let logged = LoggedNamespace::new(namespace(), wal).unwrap();
    for i in 0..n {
        logged.commit(&[upsert(&format!("n{}", i % 7), Value::String("x".repeat(i % 50)))]).unwrap();
    }
    logged.sync().unwrap();
    drop(logged);
    read_log(dir, 1).unwrap().0
}

fn seqs(batch: &iwdb_storage::ChangeBatch) -> Vec<u64> {
    batch.records.iter().map(|r| r.record.seq).collect()
}

#[test]
fn every_range_reads_like_the_full_reader() {
    let dir = tempfile::tempdir().unwrap();
    let records = write_log(dir.path(), 300);
    assert!(segments(dir.path()).len() > 10, "several segments");
    let warm = OffsetIndex::new();
    for from in [1, 2, 63, 64, 65, 100, 128, 200, 299, 300] {
        for until in [from - 1, from, from + 1, from + 70, 300] {
            let until = until.min(300);
            let cold = OffsetIndex::new().read(dir.path(), from, until, ALL).unwrap();
            let hot = warm.read(dir.path(), from, until, ALL).unwrap();
            let expected: Vec<_> = records[(from - 1) as usize..until as usize].to_vec();
            let read: Vec<_> = cold.records.iter().map(|r| r.record.clone()).collect();
            assert_eq!(read, expected, "{}..={}", from, until);
            assert_eq!(hot, cold, "{}..={}", from, until);
            assert_eq!(cold.next_seq, until.max(from - 1) + 1);
            assert_eq!(cold.first_seq, 1);
            assert!(cold.records.iter().all(|r| r.time.is_some() && r.payload_len > 0));
        }
    }
}

#[test]
fn a_tailing_reader_reads_in_batches_without_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let records = write_log(dir.path(), 2 * MARK_EVERY as usize + 10);
    let index = OffsetIndex::new();
    let mut next = 1;
    let mut read = Vec::new();
    let limits = BatchLimits { max_records: 7, max_bytes: usize::MAX };
    loop {
        let batch = index.read(dir.path(), next, records.len() as u64, limits).unwrap();
        if batch.records.is_empty() {
            break;
        }
        assert!(batch.records.len() <= 7);
        next = batch.next_seq;
        read.extend(batch.records.into_iter().map(|r| r.record));
    }
    assert_eq!(read, records);
}

#[test]
fn the_byte_limit_returns_at_least_one_record() {
    let dir = tempfile::tempdir().unwrap();
    write_log(dir.path(), 20);
    let index = OffsetIndex::new();
    let one = index.read(dir.path(), 5, 20, BatchLimits { max_records: 100, max_bytes: 1 }).unwrap();
    assert_eq!(seqs(&one), [5]);
    let some = index.read(dir.path(), 5, 20, BatchLimits { max_records: 100, max_bytes: 200 }).unwrap();
    let bytes: usize = some.records.iter().map(|r| r.payload_len).sum();
    assert!(some.records.len() > 1 && some.records.len() < 16, "{}", some.records.len());
    assert!(bytes >= 200 && bytes - some.records.last().unwrap().payload_len < 200);
}

#[test]
fn seqs_before_the_first_segment_are_not_retained() {
    let dir = tempfile::tempdir().unwrap();
    write_log(dir.path(), 100);
    let index = OffsetIndex::new();
    index.read(dir.path(), 1, 100, ALL).unwrap();
    let all = iwdb_storage::list_segments(dir.path()).unwrap();
    for (_, path) in &all[..3] {
        fs::remove_file(path).unwrap();
    }
    let first_seq = all[3].0;
    let error = index.read(dir.path(), 1, 100, ALL).unwrap_err();
    assert_matches!(error, Error::NotRetained { from: 1, first_seq: f } if f == first_seq);
    assert_matches!(index.read(dir.path(), first_seq - 1, 100, ALL), Err(Error::NotRetained { .. }));
    let batch = index.read(dir.path(), first_seq, 100, ALL).unwrap();
    assert_eq!(batch.first_seq, first_seq);
    assert_eq!(batch.records.len() as u64, 101 - first_seq);
}

#[test]
fn damage_inside_the_range_is_corruption_and_reads_before_it_work() {
    let dir = tempfile::tempdir().unwrap();
    write_log(dir.path(), 40);
    let all = iwdb_storage::list_segments(dir.path()).unwrap();
    let (first, path) = &all[1];
    let mut bytes = fs::read(path).unwrap();
    let ends = frame_ends(&bytes);
    // A byte in the payload of the segment's second record
    bytes[ends[1] + 40] ^= 0x20;
    fs::write(path, &bytes).unwrap();
    let index = OffsetIndex::new();
    let error = index.read(dir.path(), 1, 40, ALL).unwrap_err();
    assert_matches!(error, Error::Corrupt { offset, .. } if offset == ends[1] as u64);
    assert_eq!(index.read(dir.path(), 1, *first, ALL).unwrap().records.len() as u64, *first);
    // A truncated segment in the range, too
    fs::write(path, &bytes[..ends[1] + 5]).unwrap();
    assert_matches!(index.read(dir.path(), *first, 40, ALL), Err(Error::Corrupt { .. }));
}

#[test]
fn a_range_past_the_end_of_the_log_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    write_log(dir.path(), 10);
    let error = OffsetIndex::new().read(dir.path(), 5, 11, ALL).unwrap_err();
    assert_matches!(error, Error::LogEndsBefore { from: 11, next_seq: 11 });
}

#[test]
fn the_streamable_seq_waits_for_the_group_fsync() {
    let dir = tempfile::tempdir().unwrap();
    let fsync = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 1000 };
    let wal = Wal::create(dir.path(), WalOptions { fsync, segment_size: MIN_SEGMENT_SIZE }, 1).unwrap();
    let logged = Arc::new(LoggedNamespace::new(namespace(), wal).unwrap());
    logged.commit(&[upsert("a", Value::Int(1))]).unwrap();
    logged.commit(&[upsert("a", Value::Int(2))]).unwrap();
    assert_eq!((logged.seq(), logged.streamable_seq()), (2, 0));
    let soon = Some(Instant::now() + Duration::from_millis(30));
    assert_eq!(logged.wait_for_streamable(1, soon, &|| false), Wait::TimedOut(0));

    let syncer = {
        let logged = logged.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            logged.sync().unwrap();
        })
    };
    let later = Some(Instant::now() + Duration::from_secs(10));
    assert_eq!(logged.wait_for_streamable(2, later, &|| false), Wait::Reached(2));
    syncer.join().unwrap();
    // `always` makes every applied commit streamable
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::create(dir.path(), WalOptions::default(), 1).unwrap();
    let logged = LoggedNamespace::new(namespace(), wal).unwrap();
    logged.commit(&[upsert("a", Value::Int(1))]).unwrap();
    assert_eq!(logged.streamable_seq(), 1);
}

#[test]
fn off_streams_what_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::create(dir.path(), WalOptions { fsync: FsyncPolicy::Off, ..WalOptions::default() }, 1).unwrap();
    let logged = LoggedNamespace::new(namespace(), wal).unwrap();
    logged.commit(&[upsert("a", Value::Int(1))]).unwrap();
    assert_eq!((logged.wal().synced_seq(), logged.streamable_seq()), (0, 1));
}
