//! The WAL's on-disk format, version 3 (and the readers of versions 1 and 2):
//! segment headers, record frames and record payloads. The normative
//! description is `documentation/formats/wal.md`; this module is its
//! implementation.
//!
//! All integers are little endian. Nothing is aligned or padded.
//!
//! ```text
//! segment header (24 bytes, both versions)
//!   0  magic       [u8; 8]  "IWDBWAL\n"
//!   8  version     u32      1, 2 or 3 (FORMAT_VERSION is written)
//!  12  first_seq   u64      seq of the segment's first record (also the file name)
//!  20  crc         u32      CRC32C of bytes 0..20
//!
//! record frame, versions 2 and 3 (33 bytes, then the payload)
//!   0  len         u32      payload length, at most MAX_RECORD_LEN
//!   4  seq         u64
//!  12  synced_seq  u64      highest seq whose fsync had completed when this
//!                           frame was written (0: none); always < seq
//!  20  time        i64      commit time: microseconds since 1970-01-01 UTC
//!  28  kind        u8       KIND_DATA or KIND_CATALOG
//!  29  crc         u32      CRC32C of bytes 0..29 followed by the payload
//!  33  payload     [u8; len] version 3: postcard (Option<Keyed>, body)
//!                            versions 1, 2: postcard body
//!                            body: Vec<Op> (data) or CatalogChange (catalog)
//!
//! record frame, version 1 (25 bytes): the same without `time`
//!   0 len, 4 seq, 12 synced_seq, 20 kind, 21 crc (of bytes 0..21 and the payload), 25 payload
//! ```
//!
//! Version 3 (ADR 0015) prefixes the payload with the record's
//! idempotency key and result ([`iwdb_engine::Keyed`]), `None` (one
//! zero byte) for a commit without a key.

use iwdb_engine::{CatalogChange, Change, CommitRecord, DbRecord, Keyed};

use crate::Error;

/// The first 8 bytes of every segment.
pub const SEGMENT_MAGIC: [u8; 8] = *b"IWDBWAL\n";
/// The segment format this version writes.
pub const FORMAT_VERSION: u32 = 3;
/// The segment formats this version reads: version 1 (step 4, frames
/// without a commit time), version 2 (step 7, payloads without an
/// idempotency key) and version 3.
pub const READ_VERSIONS: [u32; 3] = [1, 2, FORMAT_VERSION];
/// Length of a segment header.
pub const SEGMENT_HEADER_LEN: usize = 24;
/// Length of a record frame before its payload, in the format this
/// version writes (versions 2 and 3).
pub const FRAME_HEADER_LEN: usize = 33;
/// Length of a version 1 frame header (no commit time).
pub const FRAME_HEADER_LEN_V1: usize = 25;
/// Largest record payload (64 MiB). A commit whose record is larger is
/// rejected before it is logged or applied ([`Error::RecordTooLarge`]); a
/// reader treats a larger length field as damage, so a corrupt length never
/// causes a large allocation.
pub const MAX_RECORD_LEN: u32 = 64 << 20;
/// Record kind of a data change ([`Change::Data`]).
pub const KIND_DATA: u8 = 1;
/// Record kind of a catalog change ([`Change::Catalog`]).
pub const KIND_CATALOG: u8 = 2;
/// Suffix of segment file names: `<first seq, 20 digits>.wal`.
pub const SEGMENT_SUFFIX: &str = ".wal";

/// Bytes of a segment header covered by its CRC.
const HEADER_CRC_AT: usize = 20;

/// Length of a frame header in segment format `version` (1, 2 or 3).
pub fn frame_header_len(version: u32) -> usize {
    if version == 1 { FRAME_HEADER_LEN_V1 } else { FRAME_HEADER_LEN }
}

/// The file name of the segment whose first record is `first_seq`:
/// zero-padded to 20 digits (all of `u64`), so names sort like seqs.
pub fn segment_name(first_seq: u64) -> String {
    format!("{:020}{}", first_seq, SEGMENT_SUFFIX)
}

/// The first seq of a segment file name, or `None` if the name isn't one.
/// Only the canonical form (exactly 20 digits, `.wal`) is accepted.
pub fn parse_segment_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(SEGMENT_SUFFIX)?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A segment header in the format this version writes.
pub fn encode_segment_header(first_seq: u64) -> [u8; SEGMENT_HEADER_LEN] {
    encode_segment_header_version(first_seq, FORMAT_VERSION)
}

/// A segment header of format `version` (tests and fixtures write older
/// versions with it).
pub fn encode_segment_header_version(first_seq: u64, version: u32) -> [u8; SEGMENT_HEADER_LEN] {
    let mut header = [0u8; SEGMENT_HEADER_LEN];
    header[0..8].copy_from_slice(&SEGMENT_MAGIC);
    header[8..12].copy_from_slice(&version.to_le_bytes());
    header[12..20].copy_from_slice(&first_seq.to_le_bytes());
    let crc = crc32c::crc32c(&header[..HEADER_CRC_AT]);
    header[HEADER_CRC_AT..].copy_from_slice(&crc.to_le_bytes());
    header
}

/// What a segment header says, or why it can't be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Header {
    /// A valid header of a segment in a format this reader knows.
    Valid { first_seq: u64, version: u32 },
    /// Too short, wrong magic or wrong checksum: damage.
    Damaged(Damage),
    /// A valid checksum, but a format version this reader doesn't know.
    UnsupportedVersion(u32),
}

pub(crate) fn decode_segment_header(bytes: &[u8]) -> Header {
    let Some(header) = bytes.get(..SEGMENT_HEADER_LEN) else {
        return Header::Damaged(Damage::Truncated);
    };
    let crc = u32_at(header, HEADER_CRC_AT);
    if header[0..8] != SEGMENT_MAGIC || crc32c::crc32c(&header[..HEADER_CRC_AT]) != crc {
        return Header::Damaged(Damage::BadHeader);
    }
    let version = u32_at(header, 8);
    if !READ_VERSIONS.contains(&version) {
        return Header::UnsupportedVersion(version);
    }
    Header::Valid { first_seq: u64_at(header, 12), version }
}

/// Encode a record's payload (in the format this version writes) and
/// kind. Fails if the record can't be encoded (never for records the commit
/// pipeline produces) or is larger than [`MAX_RECORD_LEN`]; nothing is
/// written then.
pub(crate) fn encode_payload(record: &CommitRecord) -> Result<(u8, Vec<u8>), Error> {
    encode_payload_version(record, FORMAT_VERSION)
}

/// Encode a record's payload in segment format `version` (tests and
/// fixtures write older versions with it; versions 1 and 2 drop the key).
pub fn encode_payload_version(record: &CommitRecord, version: u32) -> Result<(u8, Vec<u8>), Error> {
    let keyed = &record.keyed;
    let encoded = match (&record.change, version >= 3) {
        (Change::Data(ops), true) => postcard::to_allocvec(&(keyed, ops)).map(|p| (KIND_DATA, p)),
        (Change::Catalog(change), true) => postcard::to_allocvec(&(keyed, change)).map(|p| (KIND_CATALOG, p)),
        (Change::Data(ops), false) => postcard::to_allocvec(ops).map(|p| (KIND_DATA, p)),
        (Change::Catalog(change), false) => postcard::to_allocvec(change).map(|p| (KIND_CATALOG, p)),
    };
    let (kind, payload) = encoded.map_err(|e| Error::Encode { seq: record.seq, message: e.to_string() })?;
    if payload.len() > MAX_RECORD_LEN as usize {
        return Err(Error::RecordTooLarge { seq: record.seq, len: payload.len(), max: MAX_RECORD_LEN as usize });
    }
    Ok((kind, payload))
}

/// The fields of a frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub seq: u64,
    pub synced_seq: u64,
    /// The commit time, microseconds since 1970-01-01 UTC (not written in
    /// format 1).
    pub time: i64,
    pub kind: u8,
}

/// Append a whole frame (header and payload) of segment format `version`
/// to `out`. The payload must be at most [`MAX_RECORD_LEN`] bytes (the
/// writer's payloads come from `encode_payload`, which checks it). Public
/// for tests and fixtures, which write frames by hand.
pub fn encode_frame(out: &mut Vec<u8>, version: u32, header: FrameHeader, payload: &[u8]) {
    let start = out.len();
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&header.seq.to_le_bytes());
    out.extend_from_slice(&header.synced_seq.to_le_bytes());
    if version != 1 {
        out.extend_from_slice(&header.time.to_le_bytes());
    }
    out.push(header.kind);
    let crc = crc32c::crc32c_append(crc32c::crc32c(&out[start..]), payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
}

/// Why bytes where a segment header or a frame should be are not one. A
/// crash while writing produces these (a torn write); so does corruption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Damage {
    /// The file ends inside the segment header, a frame header or a payload.
    Truncated,
    /// The segment header has the wrong magic or checksum.
    BadHeader,
    /// A frame's length field is above [`MAX_RECORD_LEN`].
    BadLength,
    /// A frame's checksum doesn't match its bytes.
    Checksum,
}

impl std::fmt::Display for Damage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Damage::Truncated => "the file ends inside a header or record",
            Damage::BadHeader => "invalid segment header",
            Damage::BadLength => "record length above the limit",
            Damage::Checksum => "checksum mismatch",
        })
    }
}

/// A frame whose checksum matched, not decoded yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame<'a> {
    pub seq: u64,
    pub synced_seq: u64,
    /// The commit time (`None` in format 1).
    pub time: Option<i64>,
    pub kind: u8,
    pub payload: &'a [u8],
    /// Length of the whole frame, header included.
    pub len: usize,
    /// The segment format the frame was read in.
    pub version: u32,
}

/// Read the frame of segment format `version` at the start of `bytes`
/// (which must not be empty): `Err` if it is damaged. Checks framing and
/// checksum only, not the contents. O(frame length); allocates nothing.
pub(crate) fn read_frame(bytes: &[u8], version: u32) -> Result<Frame<'_>, Damage> {
    let header_len = frame_header_len(version);
    let Some(header) = bytes.get(..header_len) else {
        return Err(Damage::Truncated);
    };
    let len = u32_at(header, 0);
    if len > MAX_RECORD_LEN {
        return Err(Damage::BadLength);
    }
    let len = len as usize;
    let Some(payload) = bytes.get(header_len..header_len + len) else {
        return Err(Damage::Truncated);
    };
    let crc_at = header_len - 4;
    let crc = crc32c::crc32c_append(crc32c::crc32c(&header[..crc_at]), payload);
    if crc != u32_at(header, crc_at) {
        return Err(Damage::Checksum);
    }
    let time = (version != 1).then(|| u64_at(header, 20) as i64);
    Ok(Frame {
        seq: u64_at(header, 4),
        synced_seq: u64_at(header, 12),
        time,
        kind: header[crc_at - 1],
        payload,
        len: header_len + len,
        version,
    })
}

/// The seq a frame header at the start of `bytes` claims, without checking
/// anything else (used to skip most offsets cheaply when looking for
/// valid frames after damage). The seq is at the same offset in both
/// formats.
pub(crate) fn peek_seq(bytes: &[u8], version: u32) -> Option<u64> {
    bytes.get(..frame_header_len(version)).map(|h| u64_at(h, 4))
}

/// Why a frame with a valid checksum doesn't hold a valid record. The
/// writer never produces these, and a torn write can't either (except with
/// the 2^-32 chance of a matching checksum), so they are errors, never a
/// torn tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// A record kind this reader doesn't know (written by a newer version?).
    UnknownKind(u8),
    /// The payload doesn't decode as its kind, or has bytes left over.
    Undecodable(String),
    /// `synced_seq` is not below `seq`.
    SyncedSeq { seq: u64, synced_seq: u64 },
    /// Seq `u64::MAX`, which the log doesn't use (so that the next seq
    /// always exists).
    SeqOutOfRange,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid::UnknownKind(kind) => write!(f, "unknown record kind {} (written by a newer version?)", kind),
            Invalid::Undecodable(message) => write!(f, "the payload doesn't decode: {}", message),
            Invalid::SyncedSeq { seq, synced_seq } => {
                write!(f, "synced seq {} is not below the record's seq {}", synced_seq, seq)
            }
            Invalid::SeqOutOfRange => write!(f, "seq {} is out of range", u64::MAX),
        }
    }
}

/// Decode a checked frame into a record. Never panics; allocations are
/// bounded by the payload length (serde's collection pre-allocation is
/// capped), and `Value`'s serde bounds the nesting depth.
pub(crate) fn decode_record(frame: &Frame<'_>) -> Result<CommitRecord, Invalid> {
    if frame.synced_seq >= frame.seq {
        return Err(Invalid::SyncedSeq { seq: frame.seq, synced_seq: frame.synced_seq });
    }
    type Ops = Vec<ironweaver_core::Op<DbRecord, DbRecord>>;
    let (keyed, change) = match (frame.kind, frame.version >= 3) {
        (KIND_DATA, true) => {
            let (keyed, ops) = decode_exact::<(Option<Keyed>, Ops)>(frame.payload)?;
            (keyed, Change::Data(ops))
        }
        (KIND_CATALOG, true) => {
            let (keyed, change) = decode_exact::<(Option<Keyed>, CatalogChange)>(frame.payload)?;
            (keyed, Change::Catalog(change))
        }
        (KIND_DATA, false) => (None, Change::Data(decode_exact::<Ops>(frame.payload)?)),
        (KIND_CATALOG, false) => (None, Change::Catalog(decode_exact::<CatalogChange>(frame.payload)?)),
        (kind, _) => return Err(Invalid::UnknownKind(kind)),
    };
    Ok(CommitRecord { seq: frame.seq, change, keyed })
}

fn decode_exact<'a, T: serde::Deserialize<'a>>(payload: &'a [u8]) -> Result<T, Invalid> {
    match postcard::take_from_bytes::<T>(payload) {
        Ok((value, [])) => Ok(value),
        Ok((_, rest)) => Err(Invalid::Undecodable(format!("{} bytes left over", rest.len()))),
        Err(e) => Err(Invalid::Undecodable(e.to_string())),
    }
}

// Callers pass slices at least `at + 4` / `at + 8` long.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(buf)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwdb_engine::catalog::{AttrPath, IndexDef};
    use std::assert_matches;

    #[test]
    fn segment_names_sort_like_seqs() {
        assert_eq!(segment_name(1), "00000000000000000001.wal");
        assert_eq!(segment_name(u64::MAX), "18446744073709551615.wal");
        assert!(segment_name(9) < segment_name(10));
        assert_eq!(parse_segment_name(&segment_name(42)), Some(42));
        assert_eq!(parse_segment_name(&segment_name(u64::MAX)), Some(u64::MAX));
        for bad in ["1.wal", "0000000000000000001.wal", "00000000000000000001.wal.tmp", "9999999999999999999x.wal"] {
            assert_eq!(parse_segment_name(bad), None, "{}", bad);
        }
        // 20 digits above u64::MAX
        assert_eq!(parse_segment_name("99999999999999999999.wal"), None);
    }

    #[test]
    fn headers_round_trip_and_detect_damage() {
        let header = encode_segment_header(7);
        assert_eq!(decode_segment_header(&header), Header::Valid { first_seq: 7, version: 3 });
        let v1 = encode_segment_header_version(7, 1);
        assert_eq!(decode_segment_header(&v1), Header::Valid { first_seq: 7, version: 1 });
        assert_eq!(decode_segment_header(&header[..23]), Header::Damaged(Damage::Truncated));
        for bit in 0..SEGMENT_HEADER_LEN * 8 {
            let mut bad = header;
            bad[bit / 8] ^= 1 << (bit % 8);
            assert_eq!(decode_segment_header(&bad), Header::Damaged(Damage::BadHeader), "bit {}", bit);
        }
        for version in [0, 4] {
            let newer = encode_segment_header_version(7, version);
            assert_eq!(decode_segment_header(&newer), Header::UnsupportedVersion(version));
        }
    }

    fn header(seq: u64, synced_seq: u64, time: i64, kind: u8) -> FrameHeader {
        FrameHeader { seq, synced_seq, time, kind }
    }

    #[test]
    fn frames_round_trip_in_both_versions() {
        let index = IndexDef { path: AttrPath::new(["x"]).expect("path") };
        let mut record = CommitRecord::new(3, Change::Catalog(CatalogChange::CreateIndex(index)));
        record.keyed = Some(Keyed {
            key: iwdb_engine::IdempotencyKey::new("k").expect("key"),
            fingerprint: 7,
            edge_ids: vec![],
            versions: vec![],
        });
        for (version, time) in [(3, Some(-5)), (2, Some(-5)), (1, None)] {
            let (kind, payload) = encode_payload_version(&record, version).expect("encode");
            let record = CommitRecord { keyed: record.keyed.clone().filter(|_| version >= 3), ..record.clone() };
            let mut bytes = Vec::new();
            encode_frame(&mut bytes, version, header(3, 2, -5, kind), &payload);
            assert_eq!(bytes.len(), frame_header_len(version) + payload.len());
            let frame = read_frame(&bytes, version).expect("frame");
            assert_eq!(
                (frame.seq, frame.synced_seq, frame.time, frame.kind, frame.len),
                (3, 2, time, KIND_CATALOG, bytes.len())
            );
            assert_eq!(decode_record(&frame), Ok(record.clone()));
            assert_eq!(read_frame(&bytes[..bytes.len() - 1], version), Err(Damage::Truncated));
            assert_eq!(read_frame(&bytes[..frame_header_len(version) - 1], version), Err(Damage::Truncated));
            // The CRC covers every header field, the time included
            for at in 0..frame_header_len(version) - 4 {
                let mut bad = bytes.clone();
                bad[at] ^= 0x10;
                assert!(read_frame(&bad, version).is_err(), "version {} byte {}", version, at);
            }
        }
    }

    #[test]
    fn a_length_above_the_limit_is_damage_without_allocating() {
        for version in READ_VERSIONS {
            let mut bytes = Vec::new();
            encode_frame(&mut bytes, version, header(1, 0, 0, KIND_DATA), &[0]);
            bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert_eq!(read_frame(&bytes, version), Err(Damage::BadLength));
            bytes[0..4].copy_from_slice(&(MAX_RECORD_LEN).to_le_bytes());
            assert_eq!(read_frame(&bytes, version), Err(Damage::Truncated));
        }
    }

    #[test]
    fn checked_frames_with_invalid_contents_are_invalid() {
        let frame = |seq, synced_seq, kind, payload: &[u8]| {
            let mut bytes = Vec::new();
            encode_frame(&mut bytes, FORMAT_VERSION, header(seq, synced_seq, 0, kind), payload);
            decode_record(&read_frame(&bytes, FORMAT_VERSION).expect("frame"))
        };
        // No key, an empty op list
        assert_eq!(frame(1, 0, KIND_DATA, &[0, 0]), Ok(CommitRecord::new(1, Change::Data(vec![]))));
        assert_eq!(frame(1, 0, 0, &[0]), Err(Invalid::UnknownKind(0)));
        assert_eq!(frame(1, 0, 3, &[0]), Err(Invalid::UnknownKind(3)));
        assert_eq!(frame(1, 1, KIND_DATA, &[0]), Err(Invalid::SyncedSeq { seq: 1, synced_seq: 1 }));
        assert_matches!(frame(1, 0, KIND_DATA, &[0, 0, 0]), Err(Invalid::Undecodable(_)));
        // Format 3 needs the key's option byte
        assert_matches!(frame(1, 0, KIND_DATA, &[0]), Err(Invalid::Undecodable(_)));
        assert_matches!(frame(1, 0, KIND_DATA, &[]), Err(Invalid::Undecodable(_)));
        assert_matches!(frame(1, 0, KIND_CATALOG, &[9]), Err(Invalid::Undecodable(_)));
    }

    #[test]
    fn a_zeroed_frame_is_damage() {
        for version in READ_VERSIONS {
            assert_eq!(read_frame(&[0u8; 64], version), Err(Damage::Checksum));
        }
    }
}
