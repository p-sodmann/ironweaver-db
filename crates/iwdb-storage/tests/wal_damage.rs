//! Truncated and bit-flipped tails are detected and
//! treated as the end of the log; damage followed by durable data, damage
//! in an earlier segment and seq gaps are errors, never skipped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use common::{frame_ends, namespace, segments, upsert};
use ironweaver_core::{Attrs, Value};
use iwdb_engine::catalog::{AttrPath, IndexDef};
use iwdb_engine::{CatalogChange, CommitRecord, Mutation};
use iwdb_storage::format::{Damage, SEGMENT_HEADER_LEN};
use iwdb_storage::{Error, FsyncPolicy, LoggedNamespace, MIN_SEGMENT_SIZE, Wal, WalOptions, read_log, read_segment};
use proptest::prelude::*;

/// Write `n` commits (data and catalog) with `fsync` and return the
/// records as read back.
fn write_log(dir: &Path, fsync: FsyncPolicy, segment_size: u64, n: i64) -> Vec<CommitRecord> {
    let wal = Wal::create(dir, WalOptions { fsync, segment_size }, 1).unwrap();
    let logged = LoggedNamespace::new(namespace(), wal).unwrap();
    for i in 0..n {
        if i % 4 == 3 {
            let path = AttrPath::new([format!("k{}", i)]).unwrap();
            logged.commit_catalog(CatalogChange::CreateIndex(IndexDef { path })).unwrap();
        } else {
            let edge = Mutation::AddEdge {
                from: "a".into(),
                to: "a".into(),
                ty: Some("T".into()),
                attr: Attrs::new(),
                meta: Attrs::new(),
            };
            logged.commit(&[upsert("a", Value::String("v".repeat(i as usize))), edge]).unwrap();
        }
    }
    drop(logged);
    let (records, end) = read_log(dir, 1).unwrap();
    assert_eq!(records.len() as i64, n);
    assert!(end.torn().is_none());
    records
}

/// A one-segment log written with `always`: its bytes and records.
fn one_segment(n: i64) -> (Vec<u8>, Vec<CommitRecord>) {
    let dir = tempfile::tempdir().unwrap();
    let records = write_log(dir.path(), FsyncPolicy::Always, 1 << 20, n);
    let [segment] = &segments(dir.path())[..] else { panic!("one segment expected") };
    (fs::read(segment).unwrap(), records)
}

fn path() -> PathBuf {
    PathBuf::from("00000000000000000001.wal")
}

#[test]
fn truncation_at_every_offset_ends_the_log_before_the_cut_record() {
    let (bytes, records) = one_segment(12);
    let ends = frame_ends(&bytes);
    for cut in 0..=bytes.len() {
        let (read, end) = read_segment(&path(), &bytes[..cut], 1, true).unwrap();
        // Records whose frame ends at or before the cut
        let complete = ends.iter().filter(|e| **e <= cut).count().saturating_sub(1);
        assert_eq!(read, records[..complete], "cut at {}", cut);
        let valid_len = if cut < SEGMENT_HEADER_LEN { 0 } else { ends[complete] };
        assert_eq!(end.valid_len as usize, valid_len, "cut at {}", cut);
        assert_eq!(end.next_seq, complete as u64 + 1);
        assert_eq!(end.file_len as usize, cut);
        if cut == valid_len && cut >= SEGMENT_HEADER_LEN {
            assert_eq!(end.torn, None, "cut at {}", cut);
        } else {
            let torn = end.torn.unwrap();
            assert_eq!((torn.damage, torn.discarded_frames), (Damage::Truncated, 0), "cut at {}", cut);
        }
    }
}

#[test]
fn every_bit_flip_is_a_torn_tail_in_the_last_record_and_an_error_before_it() {
    let (bytes, records) = one_segment(8);
    let ends = frame_ends(&bytes);
    for bit in 0..bytes.len() * 8 {
        let mut damaged = bytes.clone();
        damaged[bit / 8] ^= 1 << (bit % 8);
        let at = bit / 8;
        // The damaged part: the header (0) or record i (i + 1)
        let part = ends.iter().position(|e| at < *e).unwrap();
        let start = if part == 0 { 0 } else { ends[part - 1] };
        let result = read_segment(&path(), &damaged, 1, true);
        if part == ends.len() - 1 {
            // The last record: a torn tail, the records before it are intact
            let (read, end) = result.unwrap();
            assert_eq!(read, records[..part - 1], "bit {}", bit);
            assert_eq!(end.valid_len as usize, start);
            assert_eq!(end.torn.unwrap().discarded_frames, 0);
        } else {
            // The header or an earlier record: valid, synced records follow
            match result {
                Err(Error::Corrupt { offset, .. }) => assert_eq!(offset as usize, start, "bit {}", bit),
                other => panic!("bit {}: {:?}", bit, other.map(|(r, e)| (r.len(), e))),
            }
        }
    }
}

#[test]
fn garbage_after_the_log_is_a_torn_tail() {
    let (bytes, records) = one_segment(5);
    for tail in [vec![0u8; 4096], vec![0xff; 100], (0..=255).collect::<Vec<u8>>(), bytes[..100].to_vec()] {
        let mut damaged = bytes.clone();
        damaged.extend_from_slice(&tail);
        let (read, end) = read_segment(&path(), &damaged, 1, true).unwrap();
        assert_eq!(read, records);
        assert_eq!(end.valid_len as usize, bytes.len());
        assert!(end.torn.is_some());
    }
    // Complete records with the wrong seq are never a tail: a repeat
    let mut repeated = bytes.clone();
    repeated.extend_from_slice(&bytes[SEGMENT_HEADER_LEN..]);
    assert!(matches!(
        read_segment(&path(), &repeated, 1, true),
        Err(Error::SeqMismatch { expected: 6, found: 1, offset, .. }) if offset as usize == bytes.len()
    ));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Any damage (several flipped bytes, a cut, zeroed ranges): the reader
    /// never panics, and what it returns is a prefix of the log.
    #[test]
    fn any_damage_yields_a_prefix_or_an_error(
        flips in proptest::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 0..4),
        zero in proptest::option::of((any::<prop::sample::Index>(), 1usize..64)),
        cut in any::<prop::sample::Index>(),
    ) {
        static LOG: OnceLock<(Vec<u8>, Vec<CommitRecord>)> = OnceLock::new();
        let (bytes, records) = LOG.get_or_init(|| one_segment(6));
        let mut damaged = bytes.clone();
        for (at, mask) in flips {
            damaged[at.index(bytes.len())] ^= mask;
        }
        if let Some((at, len)) = zero {
            let at = at.index(bytes.len());
            let end = (at + len).min(bytes.len());
            damaged[at..end].fill(0);
        }
        damaged.truncate(cut.index(bytes.len() + 1));
        if let Ok((read, end)) = read_segment(&path(), &damaged, 1, true) {
            prop_assert_eq!(&read[..], &records[..read.len()]);
            prop_assert!(end.valid_len <= end.file_len);
            prop_assert_eq!(damaged[..end.valid_len as usize] == bytes[..end.valid_len as usize], true);
        }
    }
}

/// A log of several segments in a temporary directory.
fn multi_segment() -> (tempfile::TempDir, Vec<CommitRecord>, Vec<PathBuf>) {
    let dir = tempfile::tempdir().unwrap();
    let records = write_log(dir.path(), FsyncPolicy::Always, MIN_SEGMENT_SIZE, 80);
    let segments = segments(dir.path());
    assert!(segments.len() >= 4, "{} segments", segments.len());
    (dir, records, segments)
}

fn modify(path: &Path, f: impl FnOnce(&mut Vec<u8>)) {
    let mut bytes = fs::read(path).unwrap();
    f(&mut bytes);
    fs::write(path, bytes).unwrap();
}

#[test]
fn damage_in_an_earlier_segment_is_an_error() {
    // A flipped byte in a record, a cut record, a damaged header, an empty file
    let damages: [fn(&mut Vec<u8>); 4] =
        [|b| b[SEGMENT_HEADER_LEN + 30] ^= 0x10, |b| b.truncate(b.len() - 3), |b| b[3] ^= 1, |b| b.clear()];
    for (i, damage) in damages.iter().enumerate() {
        let (dir, _, segments) = multi_segment();
        modify(&segments[1], damage);
        match read_log(dir.path(), 1) {
            Err(Error::Corrupt { path, .. }) => assert_eq!(path, segments[1], "damage {}", i),
            other => panic!("damage {}: {:?}", i, other.map(|(r, _)| r.len())),
        }
    }
}

#[test]
fn a_torn_last_segment_ends_the_log() {
    let (dir, records, segments) = multi_segment();
    let last = segments.last().unwrap();
    let first_seq = iwdb_storage::list_segments(dir.path()).unwrap().last().unwrap().0;
    let before = records.iter().filter(|r| r.seq < first_seq).count();
    let len = fs::metadata(last).unwrap().len();
    let ends = frame_ends(&fs::read(last).unwrap());
    for cut in [0, 5, SEGMENT_HEADER_LEN as u64, SEGMENT_HEADER_LEN as u64 + 7, len - 1] {
        let (dir, _, segments) = multi_segment();
        let last = segments.last().unwrap();
        fs::OpenOptions::new().write(true).open(last).unwrap().set_len(cut).unwrap();
        let (read, end) = read_log(dir.path(), 1).unwrap();
        let complete = ends.iter().filter(|e| **e as u64 <= cut).count().saturating_sub(1);
        assert_eq!(read, records[..before + complete], "cut at {}", cut);
        let segment = end.last_segment.unwrap();
        assert_eq!(&segment.path, last);
        // Torn unless the cut is at a frame boundary
        assert_eq!(segment.torn.is_none(), ends.contains(&(cut as usize)), "cut at {}", cut);
        assert_eq!(end.next_seq, first_seq + complete as u64);
    }
}

#[test]
fn gaps_and_mismatches_between_segments_are_errors() {
    // A missing middle segment
    let (dir, _, segments) = multi_segment();
    fs::remove_file(&segments[2]).unwrap();
    assert!(matches!(read_log(dir.path(), 1), Err(Error::SeqMismatch { offset: 0, .. })));

    // A segment whose name doesn't match its header
    let (dir, _, segments) = multi_segment();
    let renamed = dir.path().join("00000000000000000999.wal");
    fs::rename(segments.last().unwrap(), &renamed).unwrap();
    match read_log(dir.path(), 1) {
        Err(Error::SeqMismatch { .. }) => {}
        other => panic!("{:?}", other.map(|(r, _)| r.len())),
    }
    assert!(matches!(
        read_segment(&renamed, &fs::read(&renamed).unwrap(), 999, true),
        Err(Error::HeaderMismatch { expected: 999, .. })
    ));

    // A copy of a segment under a later name: records repeat
    let (dir, records, segments) = multi_segment();
    let next = records.last().unwrap().seq + 1;
    let copy = dir.path().join(format!("{:020}.wal", next));
    fs::copy(&segments[0], &copy).unwrap();
    assert!(matches!(read_log(dir.path(), 1), Err(Error::HeaderMismatch { .. })));

    // Files that aren't segments are ignored (e.g. a rotation's temporary file)
    let (dir, records, _) = multi_segment();
    fs::write(dir.path().join("00000000000000000999.wal.tmp"), b"partial").unwrap();
    fs::write(dir.path().join("notes.txt"), b"x").unwrap();
    assert_eq!(read_log(dir.path(), 1).unwrap().0, records);
}

#[test]
fn a_huge_segment_file_is_rejected_before_it_is_read() {
    let (dir, _, segments) = multi_segment();
    let file = fs::OpenOptions::new().write(true).open(segments.last().unwrap()).unwrap();
    file.set_len(iwdb_storage::MAX_SEGMENT_FILE_LEN + 1).unwrap();
    assert!(matches!(read_log(dir.path(), 1), Err(Error::SegmentTooLarge { .. })));
}

/// Group commit: records written before an fsync can reach the disk in any
/// order in an OS crash, so damage followed by records that were never
/// synced is a torn tail; damage followed by a record written after the
/// damaged one was synced is corruption.
#[test]
fn group_commit_tolerates_out_of_order_loss_of_unsynced_records_only() {
    // max_batch 3: records 1-3 carry synced_seq 0 (record 3's append syncs
    // 1-3), records 4-6 carry 3, records 7-9 carry 6
    let group = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 3 };
    let dir = tempfile::tempdir().unwrap();
    let records = write_log(dir.path(), group, 1 << 20, 9);
    let [segment] = &segments(dir.path())[..] else { panic!("one segment expected") };
    let bytes = fs::read(segment).unwrap();
    let ends = frame_ends(&bytes);
    // Zero record `seq` (as if its page never reached the disk)
    let lose = |bytes: &mut Vec<u8>, seq: usize| bytes[ends[seq - 1]..ends[seq]].fill(0);

    // Record 5 lost, 6 survived (both unsynced when the crash happened, so 7
    // and later must not exist)
    let mut crash = bytes[..ends[6]].to_vec();
    lose(&mut crash, 5);
    let (read, end) = read_segment(segment, &crash, 1, true).unwrap();
    assert_eq!(read, records[..4]);
    let torn = end.torn.unwrap();
    assert_eq!((end.valid_len as usize, torn.damage, torn.discarded_frames), (ends[4], Damage::Checksum, 1));

    // Record 5 damaged, but record 7 says 4-6 were synced: corruption
    let mut damaged = bytes[..ends[7]].to_vec();
    lose(&mut damaged, 5);
    assert!(
        matches!(read_segment(segment, &damaged, 1, true), Err(Error::Corrupt { offset, .. }) if offset as usize == ends[4])
    );

    // Record 6 damaged, 7 and 8 written after 4-6 were synced: corruption
    let mut damaged = bytes.clone();
    lose(&mut damaged, 6);
    assert!(matches!(read_segment(segment, &damaged, 1, true), Err(Error::Corrupt { .. })));

    // Record 8 lost, 9 survived: both unsynced (synced_seq 6)
    let mut crash = bytes.clone();
    lose(&mut crash, 8);
    let (read, end) = read_segment(segment, &crash, 1, true).unwrap();
    assert_eq!((read.len(), end.torn.unwrap().discarded_frames), (7, 1));

    // With `always`, every later record proves the damaged one was synced
    let (bytes, _) = one_segment(4);
    let ends = frame_ends(&bytes);
    let mut damaged = bytes.clone();
    damaged[ends[2]..ends[3]].fill(0);
    assert!(matches!(read_segment(segment, &damaged, 1, true), Err(Error::Corrupt { .. })));
}
