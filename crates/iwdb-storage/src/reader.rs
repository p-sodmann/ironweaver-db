//! Reading the log: [`WalReader`] iterates the records of a log directory
//! from a given `seq`, and [`read_segment`] decodes one segment's bytes.
//!
//! What the reader tells apart (see `documentation/formats/wal.md`):
//!
//! - **A torn tail**: damage (a truncated or checksum-failing header or
//!   frame) in the last segment, not followed by a valid frame that proves
//!   the damaged record had been synced. This is the clean end of the log.
//!   It is reported in [`SegmentEnd`], so that recovery (step 5) can
//!   truncate the segment there.
//! - **Corruption** ([`Error::Corrupt`]): damage in any other segment, or
//!   damage followed by a valid frame whose `synced_seq` shows that the
//!   damaged record had been synced. Never skipped.
//! - **Invalid records** (a valid checksum, but the wrong `seq`, an unknown
//!   kind or an undecodable payload): always errors.

use std::fs;
use std::path::{Path, PathBuf};

use iwdb_engine::CommitRecord;

use crate::format::{self, Damage, Header, Invalid, FRAME_HEADER_LEN, MAX_RECORD_LEN, SEGMENT_HEADER_LEN};
use crate::writer::MAX_SEGMENT_SIZE;
use crate::Error;

/// The largest segment file the writer can produce: a full segment plus one
/// record of the largest size (a segment rotates before a record that
/// doesn't fit, unless it is empty). Larger files are rejected before they
/// are read.
pub const MAX_SEGMENT_FILE_LEN: u64 =
    MAX_SEGMENT_SIZE + (SEGMENT_HEADER_LEN + FRAME_HEADER_LEN) as u64 + MAX_RECORD_LEN as u64;

/// Where the valid part of a segment ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentEnd {
    pub path: PathBuf,
    /// The first seq of the segment (its name and header).
    pub first_seq: u64,
    /// The seq after the segment's last valid record (`first_seq` if it
    /// has none).
    pub next_seq: u64,
    /// Length of the valid prefix: the header and all complete records.
    /// Recovery truncates the file to this length if it is torn.
    pub valid_len: u64,
    /// Length of the file.
    pub file_len: u64,
    /// Set if the segment has a torn tail: its header or a record at
    /// `valid_len` is incomplete or damaged.
    pub torn: Option<TornTail>,
}

/// A torn tail at the end of the last segment, from offset
/// [`SegmentEnd::valid_len`] to the end of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TornTail {
    /// What was found at `valid_len`.
    pub damage: Damage,
    /// Valid frames found after the damage and discarded with it. Their
    /// `synced_seq` shows they were written before the damaged record was
    /// synced, so an OS crash may legitimately have lost the damaged one
    /// before them (out-of-order write-back under the `group` or `off`
    /// fsync policy). Always 0 for a log written with `always`.
    pub discarded_frames: u64,
}

/// Where the log ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEnd {
    /// The seq the next record must have.
    pub next_seq: u64,
    /// The last segment, or `None` if the log has no segments.
    pub last_segment: Option<SegmentEnd>,
}

impl LogEnd {
    /// The torn tail of the last segment, if any.
    pub fn torn(&self) -> Option<&TornTail> {
        self.last_segment.as_ref().and_then(|s| s.torn.as_ref())
    }
}

/// Decoding state within one segment, apart from its bytes.
#[derive(Debug)]
struct Cursor {
    first_seq: u64,
    last: bool,
    pos: usize,
    next_seq: u64,
    end: Option<(usize, Option<TornTail>)>,
}

impl Cursor {
    /// Check the header. A damaged header is a torn tail at offset 0 in the
    /// last segment, if no valid frame follows it.
    fn new(path: &Path, bytes: &[u8], first_seq: u64, last: bool) -> Result<Self, Error> {
        let mut cursor = Cursor { first_seq, last, pos: SEGMENT_HEADER_LEN, next_seq: first_seq, end: None };
        match format::decode_segment_header(bytes) {
            Header::Valid { first_seq: found } if found == first_seq => {}
            Header::Valid { first_seq: found } => {
                return Err(Error::HeaderMismatch { path: path.into(), expected: first_seq, found })
            }
            Header::UnsupportedVersion(version) => {
                return Err(Error::UnsupportedVersion { path: path.into(), version })
            }
            // The header is synced before any record is written, so any
            // valid frame after it proves corruption
            Header::Damaged(damage) => {
                cursor.pos = 0;
                cursor.damaged(path, bytes, damage, 0)?;
            }
        }
        Ok(cursor)
    }

    fn next(&mut self, path: &Path, bytes: &[u8]) -> Result<Option<CommitRecord>, Error> {
        if self.end.is_some() {
            return Ok(None);
        }
        let Some(rest) = bytes.get(self.pos..).filter(|rest| !rest.is_empty()) else {
            self.end = Some((self.pos, None));
            return Ok(None);
        };
        let frame = match format::read_frame(rest) {
            Ok(frame) => frame,
            Err(damage) => {
                // A frame written after this one was synced proves it was
                // durable: corruption, not a torn write
                self.damaged(path, bytes, damage, self.next_seq)?;
                return Ok(None);
            }
        };
        let offset = self.pos as u64;
        if frame.seq != self.next_seq {
            return Err(Error::SeqMismatch { path: path.into(), offset, expected: self.next_seq, found: frame.seq });
        }
        if frame.seq == u64::MAX {
            return Err(Error::InvalidRecord { path: path.into(), offset, invalid: Invalid::SeqOutOfRange });
        }
        let record = format::decode_record(&frame).map_err(|invalid| Error::InvalidRecord {
            path: path.into(),
            offset,
            invalid,
        })?;
        self.pos += frame.len;
        self.next_seq = frame.seq + 1;
        Ok(Some(record))
    }

    /// Handle damage at `self.pos`: an error unless this is the last
    /// segment and no valid frame after it has `synced_seq >= proof`.
    fn damaged(&mut self, path: &Path, bytes: &[u8], damage: Damage, proof: u64) -> Result<(), Error> {
        let offset = self.pos;
        if !self.last {
            return Err(Error::Corrupt { path: path.into(), offset: offset as u64, damage });
        }
        let mut discarded_frames = 0;
        for frame in frames_after(bytes, offset, self.next_seq) {
            if frame.synced_seq >= proof {
                return Err(Error::Corrupt { path: path.into(), offset: offset as u64, damage });
            }
            discarded_frames += 1;
        }
        self.end = Some((offset, Some(TornTail { damage, discarded_frames })));
        Ok(())
    }

    /// Where the segment ends; call once `next` has returned `None`.
    fn segment_end(&self, path: &Path, file_len: usize) -> SegmentEnd {
        let (valid_len, torn) = self.end.clone().unwrap_or((self.pos, None));
        SegmentEnd {
            path: path.into(),
            first_seq: self.first_seq,
            next_seq: self.next_seq,
            valid_len: valid_len as u64,
            file_len: file_len as u64,
            torn,
        }
    }
}

/// Valid frames (by checksum) that start after `damage` and claim a seq a
/// record after the damaged one could have: between `lowest` and `lowest`
/// plus the number of frames that fit into the rest of the file. The seq
/// window rejects almost every offset before any checksum is computed, so
/// this is O(bytes) for anything but crafted input.
fn frames_after(bytes: &[u8], damage: usize, lowest: u64) -> impl Iterator<Item = format::Frame<'_>> {
    let highest = lowest.saturating_add((bytes.len() / (FRAME_HEADER_LEN + 1)) as u64);
    let mut pos = damage + 1;
    std::iter::from_fn(move || {
        while pos + FRAME_HEADER_LEN <= bytes.len() {
            let rest = &bytes[pos..];
            let in_window = format::peek_seq(rest).is_some_and(|seq| (lowest..=highest).contains(&seq));
            if in_window {
                if let Ok(frame) = format::read_frame(rest) {
                    pos += frame.len;
                    return Some(frame);
                }
            }
            pos += 1;
        }
        None
    })
}

/// Decode one segment from its bytes: its records and where its valid
/// part ends. `first_seq` comes from the file name; `last` says whether it
/// is the last segment of the log (only the last one can have a torn
/// tail). Never panics on arbitrary bytes; O(bytes) time and memory for
/// the records.
///
/// This is the reader [`WalReader`] uses per segment, exposed for fuzzing
/// and for `verify` (step 7).
pub fn read_segment(
    path: &Path,
    bytes: &[u8],
    first_seq: u64,
    last: bool,
) -> Result<(Vec<CommitRecord>, SegmentEnd), Error> {
    let mut cursor = Cursor::new(path, bytes, first_seq, last)?;
    let mut records = Vec::new();
    while let Some(record) = cursor.next(path, bytes)? {
        records.push(record);
    }
    Ok((records, cursor.segment_end(path, bytes.len())))
}

/// Read a segment file, refusing files larger than [`MAX_SEGMENT_FILE_LEN`].
fn read_file(path: &Path) -> Result<Vec<u8>, Error> {
    let len = fs::metadata(path).map_err(|e| Error::io("stat", path, e))?.len();
    if len > MAX_SEGMENT_FILE_LEN {
        return Err(Error::SegmentTooLarge { path: path.into(), len });
    }
    fs::read(path).map_err(|e| Error::io("read", path, e))
}

/// Check a whole segment file, like [`read_segment`], without keeping its
/// records.
pub(crate) fn read_segment_file(path: &Path, first_seq: u64, last: bool) -> Result<SegmentEnd, Error> {
    let bytes = read_file(path)?;
    let mut cursor = Cursor::new(path, &bytes, first_seq, last)?;
    while cursor.next(path, &bytes)?.is_some() {}
    Ok(cursor.segment_end(path, bytes.len()))
}

/// The segments of a log directory, sorted by first seq. Files whose name
/// isn't a segment name (such as a `.tmp` file left by an interrupted
/// rotation) are ignored.
pub fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>, Error> {
    let mut segments = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
        let entry = entry.map_err(|e| Error::io("list", dir, e))?;
        if let Some(first_seq) = entry.file_name().to_str().and_then(format::parse_segment_name) {
            segments.push((first_seq, entry.path()));
        }
    }
    segments.sort_unstable();
    Ok(segments)
}

/// Iterates the records of a log directory in `seq` order, from a given
/// `seq` on, checking every record on the way.
///
/// - Records before `from` in the first segment read are checked but not
///   returned. Segments that end before `from` are not read.
/// - Each segment is read into memory whole (at most
///   [`MAX_SEGMENT_FILE_LEN`] bytes); records are decoded one at a time.
/// - After the iterator returns `None`, [`end`](Self::end) says where the
///   log ends and whether its last segment has a torn tail.
/// - After an error, the iterator returns `None` and `end` stays `None`.
/// - A bounded reader ([`open_until`](Self::open_until)) stops after a
///   given record, without looking at anything after it.
#[derive(Debug)]
pub struct WalReader {
    from: u64,
    /// The last seq to return (`u64::MAX`: read to the end of the log).
    until: u64,
    /// The seq of the next record to return.
    wanted: u64,
    segments: Vec<(u64, PathBuf)>,
    /// Index of the next segment to open.
    next_segment: usize,
    current: Option<(Vec<u8>, Cursor)>,
    /// The seq the next segment must start with (after the first one).
    next_seq: Option<u64>,
    end: Option<LogEnd>,
    done: bool,
}

enum Step {
    Record(CommitRecord),
    End(LogEnd),
    /// A bounded reader returned its last record.
    Stopped,
}

impl WalReader {
    /// A reader of the log in `dir`, returning records with `seq >= from`
    /// (`from` 0 is read as 1). Fails with [`Error::MissingRecords`] if
    /// the log starts after `from`.
    pub fn open(dir: &Path, from: u64) -> Result<Self, Error> {
        Self::open_until(dir, from, u64::MAX)
    }

    /// A reader of the records `from ..= until` of the log in `dir`
    /// (`from` 0 is read as 1): a log that another thread is appending to
    /// can be read up to a record known to be complete.
    ///
    /// It stops right after returning record `until`, and never decodes
    /// the bytes after it, so a frame being written after it is never
    /// mistaken for damage. [`end`](Self::end) stays `None` then, because
    /// the end of the log was not read. If `until < from` it returns
    /// nothing. Fails with [`Error::MissingRecords`] if the log starts after
    /// `from`, and (from the iterator) with [`Error::LogEndsBefore`] naming
    /// `until` if the log ends before record `until`.
    pub fn open_until(dir: &Path, from: u64, until: u64) -> Result<Self, Error> {
        let from = from.max(1);
        let mut segments = list_segments(dir)?;
        // Start at the last segment that starts at or before `from`
        if let Some(&(first_seq, _)) = segments.first() {
            let start = segments.partition_point(|(seq, _)| *seq <= from);
            if start == 0 {
                return Err(Error::MissingRecords { from, first_seq });
            }
            segments.drain(..start - 1);
        }
        Ok(WalReader {
            from,
            until,
            wanted: from,
            segments,
            next_segment: 0,
            current: None,
            next_seq: None,
            end: None,
            done: false,
        })
    }

    /// Where the log ends, once the iterator has returned `None` without
    /// an error.
    pub fn end(&self) -> Option<&LogEnd> {
        self.end.as_ref()
    }

    fn step(&mut self) -> Result<Step, Error> {
        if self.wanted > self.until {
            return Ok(Step::Stopped);
        }
        if self.segments.is_empty() {
            return self.finish(LogEnd { next_seq: self.from, last_segment: None });
        }
        loop {
            if let Some((bytes, cursor)) = &mut self.current {
                let path = &self.segments[self.next_segment - 1].1;
                while let Some(record) = cursor.next(path, bytes)? {
                    if record.seq >= self.from {
                        self.wanted = record.seq + 1;
                        return Ok(Step::Record(record));
                    }
                }
                if self.next_segment == self.segments.len() {
                    if cursor.next_seq < self.from {
                        return Err(Error::LogEndsBefore { from: self.from, next_seq: cursor.next_seq });
                    }
                    let last_segment = Some(cursor.segment_end(path, bytes.len()));
                    let end = LogEnd { next_seq: cursor.next_seq, last_segment };
                    return self.finish(end);
                }
                self.next_seq = Some(cursor.next_seq);
                self.current = None;
            }
            self.open_next()?;
        }
    }

    /// The end of the log: an error for a bounded reader that hasn't
    /// reached its last record.
    fn finish(&self, end: LogEnd) -> Result<Step, Error> {
        if self.until != u64::MAX && end.next_seq <= self.until {
            return Err(Error::LogEndsBefore { from: self.until, next_seq: end.next_seq });
        }
        Ok(Step::End(end))
    }

    fn open_next(&mut self) -> Result<(), Error> {
        let (first_seq, path) = &self.segments[self.next_segment];
        let last = self.next_segment + 1 == self.segments.len();
        if let Some(expected) = self.next_seq {
            if *first_seq != expected {
                return Err(Error::SeqMismatch { path: path.clone(), offset: 0, expected, found: *first_seq });
            }
        }
        let bytes = read_file(path)?;
        let cursor = Cursor::new(path, &bytes, *first_seq, last)?;
        self.current = Some((bytes, cursor));
        self.next_segment += 1;
        Ok(())
    }
}

impl Iterator for WalReader {
    type Item = Result<CommitRecord, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Step::Record(record)) => Some(Ok(record)),
            Ok(Step::End(end)) => {
                self.end = Some(end);
                self.done = true;
                None
            }
            Ok(Step::Stopped) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Read the whole log in `dir` from `from` on: its records and where it
/// ends. Holds all records in memory; recovery iterates a [`WalReader`]
/// instead.
pub fn read_log(dir: &Path, from: u64) -> Result<(Vec<CommitRecord>, LogEnd), Error> {
    let mut reader = WalReader::open(dir, from)?;
    let mut records = Vec::new();
    loop {
        match reader.step()? {
            Step::Record(record) => records.push(record),
            Step::End(end) => return Ok((records, end)),
            // An unbounded reader stops only at u64::MAX, which no record has
            Step::Stopped => return Err(Error::LogEndsBefore { from: u64::MAX, next_seq: reader.wanted }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{encode_frame, encode_segment_header, KIND_CATALOG, KIND_DATA};
    use iwdb_engine::Change;
    use proptest::prelude::*;

    fn path() -> PathBuf {
        PathBuf::from("test.wal")
    }

    /// A segment starting at `first_seq` with the given frames
    /// (`seq`, `synced_seq`, `kind`, payload).
    fn segment(first_seq: u64, frames: &[(u64, u64, u8, &[u8])]) -> Vec<u8> {
        let mut bytes = encode_segment_header(first_seq).to_vec();
        for (seq, synced_seq, kind, payload) in frames {
            encode_frame(&mut bytes, *seq, *synced_seq, *kind, payload);
        }
        bytes
    }

    const EMPTY: &[u8] = &[0];

    #[test]
    fn records_must_follow_each_other() {
        let ok = segment(5, &[(5, 4, KIND_DATA, EMPTY), (6, 5, KIND_DATA, EMPTY)]);
        let (records, end) = read_segment(&path(), &ok, 5, true).expect("read");
        assert_eq!(records.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![5, 6]);
        assert_eq!((end.next_seq, end.torn), (7, None));
        assert_eq!(records[0].change, Change::Data(vec![]));

        for (bytes, expected, found) in [
            (segment(5, &[(6, 4, KIND_DATA, EMPTY)]), 5, 6),
            (segment(5, &[(5, 4, KIND_DATA, EMPTY), (5, 4, KIND_DATA, EMPTY)]), 6, 5),
            (segment(5, &[(5, 4, KIND_DATA, EMPTY), (7, 4, KIND_DATA, EMPTY)]), 6, 7),
        ] {
            match read_segment(&path(), &bytes, 5, true) {
                Err(Error::SeqMismatch { expected: e, found: f, .. }) => assert_eq!((e, f), (expected, found)),
                other => panic!("{:?}", other),
            }
        }
    }

    #[test]
    fn a_checked_but_invalid_last_record_is_an_error_not_a_tail() {
        for (kind, payload) in [(9, EMPTY), (KIND_DATA, &[1u8][..]), (KIND_CATALOG, &[0xff, 0xff][..])] {
            let bytes = segment(1, &[(1, 0, KIND_DATA, EMPTY), (2, 1, kind, payload)]);
            assert!(matches!(read_segment(&path(), &bytes, 1, true), Err(Error::InvalidRecord { .. })));
        }
        let bytes = segment(u64::MAX, &[(u64::MAX, 0, KIND_DATA, EMPTY)]);
        assert!(matches!(
            read_segment(&path(), &bytes, u64::MAX, true),
            Err(Error::InvalidRecord { invalid: Invalid::SeqOutOfRange, .. })
        ));
    }

    #[test]
    fn a_damaged_header_is_a_torn_tail_only_without_records_after_it() {
        let mut bytes = segment(1, &[]);
        bytes[0] ^= 1;
        let (records, end) = read_segment(&path(), &bytes, 1, true).expect("read");
        assert!(records.is_empty());
        assert_eq!((end.valid_len, end.next_seq), (0, 1));
        assert_eq!(end.torn.map(|t| t.damage), Some(Damage::BadHeader));
        assert!(matches!(read_segment(&path(), &bytes, 1, false), Err(Error::Corrupt { offset: 0, .. })));

        let mut bytes = segment(1, &[(1, 0, KIND_DATA, EMPTY)]);
        bytes[0] ^= 1;
        assert!(matches!(read_segment(&path(), &bytes, 1, true), Err(Error::Corrupt { offset: 0, .. })));
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

        /// Arbitrary bytes as a segment: never a panic.
        #[test]
        fn arbitrary_segments_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512), last: bool) {
            let _ = read_segment(&path(), &bytes, 1, last);
        }

        /// Arbitrary bytes after a valid header: never a panic; a torn
        /// tail's valid part is a prefix of the input.
        #[test]
        fn arbitrary_frames_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512), last: bool) {
            let mut segment = encode_segment_header(1).to_vec();
            segment.extend_from_slice(&bytes);
            if let Ok((records, end)) = read_segment(&path(), &segment, 1, last) {
                prop_assert!(end.valid_len <= end.file_len);
                prop_assert_eq!(end.next_seq, 1 + records.len() as u64);
            }
        }

        /// Arbitrary payloads in frames with valid checksums (which random
        /// bytes almost never reach): the payload decoder never panics.
        #[test]
        fn arbitrary_payloads_never_panic(payload in proptest::collection::vec(any::<u8>(), 0..256), kind in 0u8..4) {
            let bytes = segment(1, &[(1, 0, kind, &payload)]);
            if let Ok((records, _)) = read_segment(&path(), &bytes, 1, true) {
                prop_assert_eq!(crate::format::encode_payload(&records[0]).map(|(k, _)| k).ok(), Some(kind));
            }
        }
    }
}
