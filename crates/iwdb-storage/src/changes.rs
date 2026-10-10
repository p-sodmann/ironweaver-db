//! Reading the WAL from a seq for the change stream (ADR 0031): an
//! [`OffsetIndex`] per namespace, and [`WalRetention`], which keeps segments
//! for the stream after a checkpoint covers them.
//!
//! [`WalReader`](crate::WalReader) reads a segment whole and checks every
//! record before the one asked for, which suits recovery but costs up to a
//! segment per call for a consumer that asks for the next few records again
//! and again. [`OffsetIndex::read`] reads only up to a seq that is known to
//! be complete and synced (the streamable seq), so it never meets a frame
//! being written and never has to tell a torn tail from corruption: every
//! damaged byte it reads is an error. It starts at the offset of a nearby
//! seq it has seen before, and remembers one offset every [`MARK_EVERY`]
//! seqs, plus where the last read stopped.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use iwdb_engine::{CommitRecord, CommitTime};

use crate::format::{self, Damage, Header, MAX_RECORD_LEN, SEGMENT_HEADER_LEN};
use crate::{Error, list_segments};

/// The index remembers the offset of every seq divisible by this.
pub const MARK_EVERY: u64 = 64;

/// How long the WAL is kept for the change stream after checkpoints no
/// longer need it (ADR 0031). A segment is deleted only when the
/// checkpoints and the retention both allow it; the default keeps nothing
/// extra.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalRetention {
    /// Keep the segments that hold one of the last `records` commits (as of
    /// the checkpoint that removes segments).
    pub records: u64,
    /// Keep the segments that hold a commit younger than this. A segment's
    /// age is that of the first commit after it, so a segment is kept until
    /// all of its commits are older. Segments of WAL format 1, which has no
    /// commit times, are kept by `records` only.
    pub age: Option<Duration>,
}

/// One record read by [`OffsetIndex::read`].
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeRecord {
    pub record: CommitRecord,
    /// The commit time (`None` in WAL format 1).
    pub time: Option<CommitTime>,
    /// The size of its WAL payload.
    pub payload_len: usize,
}

/// What [`OffsetIndex::read`] returns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChangeBatch {
    /// The records from the seq asked for, in seq order, without gaps.
    pub records: Vec<ChangeRecord>,
    /// The seq after the last record returned (the seq asked for if there
    /// is none).
    pub next_seq: u64,
    /// The oldest seq in the WAL directory when it was listed.
    pub first_seq: u64,
}

/// How much one [`OffsetIndex::read`] returns: it stops after
/// `max_records` records or once the payloads add up to `max_bytes`, and
/// returns at least one record if there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchLimits {
    pub max_records: usize,
    pub max_bytes: usize,
}

/// Known frame offsets of one segment.
#[derive(Debug, Default)]
struct Offsets {
    /// Seqs divisible by [`MARK_EVERY`] -> offset.
    marks: BTreeMap<u64, u64>,
    /// Where the last read in this segment stopped.
    frontier: Option<(u64, u64)>,
}

impl Offsets {
    /// The known offset of the greatest seq at or before `seq`.
    fn before(&self, seq: u64) -> Option<(u64, u64)> {
        let mark = self.marks.range(..=seq).next_back().map(|(s, o)| (*s, *o));
        let frontier = self.frontier.filter(|(s, _)| *s <= seq);
        mark.max(frontier)
    }
}

/// Frame offsets in the WAL segments of one namespace, learned while
/// reading, so that a read at a seq starts near it (see the module docs).
/// Shared by the readers of a namespace; the lock is held only to look up
/// and record offsets, never during I/O.
///
/// Memory: 16 bytes per [`MARK_EVERY`] records of retained WAL. Entries of
/// segments that are gone are dropped at the next read.
#[derive(Debug, Default)]
pub struct OffsetIndex {
    segments: Mutex<BTreeMap<u64, Offsets>>,
}

impl OffsetIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// The records `from ..= until` of the log in `dir` (`from` 0 is read
    /// as 1), at most `limits` of them; nothing if `until < from`. `until`
    /// must be a seq whose record and all before it are completely written
    /// (the streamable seq): the reader never looks past it.
    ///
    /// Errors: [`Error::NotRetained`] if `from` is older than the oldest
    /// segment, or a segment it needs was deleted meanwhile; for damage,
    /// which can't be a torn tail at or below `until`,
    /// [`Error::Corrupt`], [`Error::InvalidRecord`],
    /// [`Error::SeqMismatch`], [`Error::HeaderMismatch`] or
    /// [`Error::UnsupportedVersion`]; [`Error::LogEndsBefore`] if the log
    /// ends before `until`; [`Error::Io`].
    pub fn read(&self, dir: &Path, from: u64, until: u64, limits: BatchLimits) -> Result<ChangeBatch, Error> {
        let from = from.max(1);
        let segments = list_segments(dir)?;
        self.prune(&segments);
        let Some(&(first_seq, _)) = segments.first() else {
            if from > until {
                return Ok(ChangeBatch { records: Vec::new(), next_seq: from, first_seq: from });
            }
            return Err(Error::NotRetained { from, first_seq: until.saturating_add(1) });
        };
        if from < first_seq {
            return Err(Error::NotRetained { from, first_seq });
        }
        let mut batch = ChangeBatch { records: Vec::new(), next_seq: from, first_seq };
        if from > until {
            return Ok(batch);
        }
        let start = segments.partition_point(|(seq, _)| *seq <= from) - 1;
        let mut bytes = 0;
        let mut expected_first = None;
        for (i, (segment_seq, path)) in segments.iter().enumerate().skip(start) {
            if let Some(expected) = expected_first
                && *segment_seq != expected
            {
                return Err(Error::SeqMismatch { path: path.clone(), offset: 0, expected, found: *segment_seq });
            }
            let mut file = match crate::io::open_unless_removed(path, |p| File::open(p)) {
                Ok(Some(file)) => file,
                // The checkpointer deleted it after the listing
                Ok(None) => return Err(self.gone(dir, batch.next_seq)),
                Err(e) => return Err(Error::io("open", path, e)),
            };
            let version = segment_version(&mut file, path, *segment_seq)?;
            let (mut seq, mut offset) =
                self.lookup(*segment_seq, batch.next_seq).unwrap_or((*segment_seq, SEGMENT_HEADER_LEN as u64));
            file.seek(SeekFrom::Start(offset)).map_err(|e| Error::io("seek", path, e))?;
            let mut reader = BufReader::with_capacity(64 << 10, file);
            let mut marks = Vec::new();
            let full = loop {
                if seq > until {
                    break true;
                }
                if batch.records.len() >= limits.max_records || (!batch.records.is_empty() && bytes >= limits.max_bytes)
                {
                    break true;
                }
                let Some(frame) = next_frame(&mut reader, path, offset, version)? else {
                    break false;
                };
                if frame.seq != seq {
                    return Err(Error::SeqMismatch { path: path.clone(), offset, expected: seq, found: frame.seq });
                }
                if seq % MARK_EVERY == 0 {
                    marks.push((seq, offset));
                }
                if seq >= batch.next_seq {
                    let record = frame.decode(path, offset)?;
                    bytes += frame.payload_len;
                    batch.records.push(ChangeRecord {
                        record,
                        time: frame.time.map(CommitTime),
                        payload_len: frame.payload_len,
                    });
                    batch.next_seq = seq + 1;
                }
                offset += frame.len as u64;
                seq += 1;
            };
            self.remember(*segment_seq, marks, (seq, offset));
            if full {
                return Ok(batch);
            }
            // The end of this segment: the next one starts at `seq`
            if i + 1 == segments.len() {
                return Err(Error::LogEndsBefore { from: until, next_seq: seq });
            }
            expected_first = Some(seq);
        }
        Ok(batch)
    }

    /// The error for a segment deleted between the listing and the read.
    fn gone(&self, dir: &Path, from: u64) -> Error {
        match list_segments(dir) {
            Ok(segments) => {
                let first_seq = segments.first().map_or(from.saturating_add(1), |(s, _)| *s);
                Error::NotRetained { from, first_seq: first_seq.max(from.saturating_add(1)) }
            }
            Err(e) => e,
        }
    }

    fn lookup(&self, segment: u64, seq: u64) -> Option<(u64, u64)> {
        let segments = self.segments.lock().unwrap_or_else(PoisonError::into_inner);
        segments.get(&segment)?.before(seq)
    }

    fn remember(&self, segment: u64, marks: Vec<(u64, u64)>, frontier: (u64, u64)) {
        let mut segments = self.segments.lock().unwrap_or_else(PoisonError::into_inner);
        let offsets = segments.entry(segment).or_default();
        offsets.marks.extend(marks);
        offsets.frontier = Some(frontier);
    }

    /// Drop the entries of segments that are no longer listed.
    fn prune(&self, listed: &[(u64, std::path::PathBuf)]) {
        let mut segments = self.segments.lock().unwrap_or_else(PoisonError::into_inner);
        segments.retain(|seq, _| listed.binary_search_by_key(seq, |(s, _)| *s).is_ok());
    }
}

/// Read and check a segment header; returns its format version.
fn segment_version(file: &mut File, path: &Path, first_seq: u64) -> Result<u32, Error> {
    let mut header = [0; SEGMENT_HEADER_LEN];
    read_exact(file, &mut header, path, 0)?;
    match format::decode_segment_header(&header) {
        Header::Valid { first_seq: found, version } if found == first_seq => Ok(version),
        Header::Valid { first_seq: found, .. } => {
            Err(Error::HeaderMismatch { path: path.into(), expected: first_seq, found })
        }
        Header::UnsupportedVersion(version) => Err(Error::UnsupportedVersion { path: path.into(), version }),
        Header::Damaged(damage) => Err(Error::Corrupt { path: path.into(), offset: 0, damage }),
    }
}

/// `read_exact`, with a short read as [`Damage::Truncated`] at `offset`.
fn read_exact(reader: &mut impl Read, buf: &mut [u8], path: &Path, offset: u64) -> Result<(), Error> {
    reader.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => Error::Corrupt { path: path.into(), offset, damage: Damage::Truncated },
        _ => Error::io("read", path, e),
    })
}

/// A checked frame, with its bytes.
struct Frame {
    seq: u64,
    time: Option<i64>,
    /// The whole frame's length.
    len: usize,
    payload_len: usize,
    bytes: Vec<u8>,
    version: u32,
}

impl Frame {
    fn decode(&self, path: &Path, offset: u64) -> Result<CommitRecord, Error> {
        // Checked when it was read: this can't fail
        let frame = format::read_frame(&self.bytes, self.version).map_err(|damage| Error::Corrupt {
            path: path.into(),
            offset,
            damage,
        })?;
        format::decode_record(&frame).map_err(|invalid| Error::InvalidRecord { path: path.into(), offset, invalid })
    }
}

/// The next frame at `offset`, checked; `None` at the end of the file.
/// Every damage is an error ([`Error::Corrupt`]): the caller reads only
/// complete frames.
fn next_frame(reader: &mut impl Read, path: &Path, offset: u64, version: u32) -> Result<Option<Frame>, Error> {
    let header_len = format::frame_header_len(version);
    let mut bytes = vec![0; header_len];
    // The end of the file at a frame boundary is the end of the segment
    let mut filled = 0;
    while filled < header_len {
        match reader.read(&mut bytes[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(Error::Corrupt { path: path.into(), offset, damage: Damage::Truncated }),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::io("read", path, e)),
        }
    }
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if len > MAX_RECORD_LEN {
        return Err(Error::Corrupt { path: path.into(), offset, damage: Damage::BadLength });
    }
    bytes.resize(header_len + len as usize, 0);
    read_exact(reader, &mut bytes[header_len..], path, offset)?;
    let frame =
        format::read_frame(&bytes, version).map_err(|damage| Error::Corrupt { path: path.into(), offset, damage })?;
    let (seq, time, len) = (frame.seq, frame.time, frame.len);
    Ok(Some(Frame { seq, time, len, payload_len: len - header_len, bytes, version }))
}

/// The commit time of the first record of a segment: `None` if it has no
/// record yet or is in WAL format 1 (no times). Used by [`WalRetention`]'s
/// age. Damage is an error.
pub(crate) fn first_record_time(path: &Path, first_seq: u64) -> Result<Option<CommitTime>, Error> {
    let mut file = File::open(path).map_err(|e| Error::io("open", path, e))?;
    let version = segment_version(&mut file, path, first_seq)?;
    let mut reader = BufReader::new(file);
    let frame = next_frame(&mut reader, path, SEGMENT_HEADER_LEN as u64, version)?;
    Ok(frame.and_then(|f| f.time).map(CommitTime))
}
