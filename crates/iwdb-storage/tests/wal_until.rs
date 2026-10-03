//! The bounded reader (`WalReader::open_until`), which the checkpointer
//! uses on a log that the writer is still appending to: it stops
//! after its last record and never looks at the bytes after it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use common::{namespace, segments, upsert};
use ironweaver_core::Value;
use iwdb_storage::format::{FrameHeader, FORMAT_VERSION};
use iwdb_storage::{Error, FsyncPolicy, LoggedNamespace, Wal, WalOptions, WalReader, MIN_SEGMENT_SIZE};

/// A log with records 1..=n in small segments.
fn log(n: i64) -> (tempfile::TempDir, LoggedNamespace) {
    let dir = tempfile::tempdir().unwrap();
    let options = WalOptions { fsync: FsyncPolicy::Always, segment_size: MIN_SEGMENT_SIZE };
    let logged = LoggedNamespace::new(namespace(), Wal::create(dir.path(), options, 1).unwrap()).unwrap();
    for i in 0..n {
        logged.commit(&[upsert(&format!("n{}", i % 7), Value::from(format!("{:0>100}", i)))]).unwrap();
    }
    (dir, logged)
}

/// A record frame of the current format.
fn encode_frame(out: &mut Vec<u8>, seq: u64, synced_seq: u64, kind: u8, payload: &[u8]) {
    let header = FrameHeader { seq, synced_seq, time: 0, kind };
    iwdb_storage::format::encode_frame(out, FORMAT_VERSION, header, payload);
}

fn seqs(reader: WalReader) -> Vec<u64> {
    reader.map(|r| r.unwrap().seq).collect()
}

#[test]
fn reads_a_range_and_stops() {
    let (dir, _logged) = log(40);
    assert!(segments(dir.path()).len() > 3);
    let mut reader = WalReader::open_until(dir.path(), 3, 27).unwrap();
    assert_eq!(reader.by_ref().map(|r| r.unwrap().seq).collect::<Vec<_>>(), (3..=27).collect::<Vec<_>>());
    assert!(reader.end().is_none(), "the end of the log was not read");
    assert_eq!(seqs(WalReader::open_until(dir.path(), 40, 40).unwrap()), vec![40]);
    assert_eq!(seqs(WalReader::open_until(dir.path(), 0, 1).unwrap()), vec![1]);
    // An empty range reads nothing
    assert!(seqs(WalReader::open_until(dir.path(), 10, 9).unwrap()).is_empty());
    // Unbounded is the same as open
    assert_eq!(seqs(WalReader::open_until(dir.path(), 38, u64::MAX).unwrap()), vec![38, 39, 40]);
}

#[test]
fn a_range_past_the_end_of_the_log_is_an_error() {
    let (dir, _logged) = log(5);
    let results: Vec<_> = WalReader::open_until(dir.path(), 4, 6).unwrap().collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].as_ref().unwrap().seq, 4);
    assert_eq!(results[1].as_ref().unwrap().seq, 5);
    assert!(matches!(results[2], Err(Error::LogEndsBefore { from: 6, next_seq: 6 })));

    let empty = tempfile::tempdir().unwrap();
    let results: Vec<_> = WalReader::open_until(empty.path(), 1, 1).unwrap().collect();
    assert!(matches!(results[..], [Err(Error::LogEndsBefore { from: 1, next_seq: 1 })]));
    assert!(seqs(WalReader::open_until(empty.path(), 1, 0).unwrap()).is_empty());
}

/// Bytes after the last record read are never decoded: a frame being
/// written, or even damage that an unbounded reader reports as corruption.
#[test]
fn never_looks_past_its_last_record() {
    let (dir, logged) = log(10);
    let last = segments(dir.path()).pop().unwrap();
    drop(logged);

    // Half a frame for record 11, as if the writer were in the middle of it
    let mut frame = Vec::new();
    encode_frame(&mut frame, 11, 10, 1, &[0; 40]);
    let mut file = OpenOptions::new().append(true).open(&last).unwrap();
    file.write_all(&frame[..30]).unwrap();
    assert_eq!(seqs(WalReader::open_until(dir.path(), 1, 10).unwrap()), (1..=10).collect::<Vec<_>>());
    let mut reader = WalReader::open(dir.path(), 1).unwrap();
    assert_eq!(reader.by_ref().count(), 10);
    assert!(reader.end().unwrap().torn().is_some(), "an unbounded reader sees a torn tail");

    // Then a valid frame proving that record 11 was synced: corruption for an
    // unbounded reader, invisible to one that stops at 10
    let mut proof = Vec::new();
    encode_frame(&mut proof, 12, 11, 1, &[0]);
    file.write_all(&proof).unwrap();
    assert_eq!(seqs(WalReader::open_until(dir.path(), 5, 10).unwrap()), (5..=10).collect::<Vec<_>>());
    assert!(WalReader::open(dir.path(), 1).unwrap().any(|r| matches!(r, Err(Error::Corrupt { .. }))));
}

/// A reader thread reads up to the writer's `synced_seq` over and over while
/// the writer appends and rotates: every read succeeds and returns exactly
/// the requested records.
#[test]
fn reads_up_to_the_synced_seq_while_the_writer_appends() {
    let (dir, logged) = log(1);
    let logged = Arc::new(Mutex::new(logged));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (logged, stop) = (logged.clone(), stop.clone());
        thread::spawn(move || {
            for i in 0..400 {
                logged.lock().unwrap().commit(&[upsert(&format!("n{}", i % 5), Value::Int(i))]).unwrap();
            }
            stop.store(true, Ordering::SeqCst);
        })
    };
    let mut reads = 0;
    // At least two reads, however fast the writer is (it may finish before
    // the second one under load)
    while !stop.load(Ordering::SeqCst) || reads < 2 {
        let until = logged.lock().unwrap().wal().synced_seq();
        let from = until.saturating_sub(30).max(1);
        assert_eq!(seqs(WalReader::open_until(dir.path(), from, until).unwrap()), (from..=until).collect::<Vec<_>>());
        reads += 1;
    }
    writer.join().unwrap();
    assert!(reads > 1);
}
