//! The storage layer's error type.

use std::io;
use std::path::PathBuf;

use crate::format::{Damage, Invalid};

/// Errors raised by the storage layer.
///
/// Corrupt data on disk is always an error (or, at the end of the log, a
/// reported torn tail), never a panic.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An error from the commit pipeline (the commit is rejected; see
    /// [`iwdb_engine::Error`]).
    #[error(transparent)]
    Engine(#[from] iwdb_engine::Error),

    // Writing
    /// An I/O operation failed. When it happened while writing or syncing
    /// the log, the log is now failed (read-only until reopened) and the
    /// commit was not applied; its outcome is unknown (see
    /// `documentation/guarantees.md`). For checkpoints and recovery, see
    /// the operation that returned it.
    #[error("{op} failed on '{}': {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The log (or the namespace using it) failed earlier and accepts no
    /// more writes until it is reopened (step 5 recovers it).
    #[error("read-only until reopened, after an earlier failure: {cause}")]
    ReadOnly { cause: String },
    /// The commit's record is larger than [`MAX_RECORD_LEN`](crate::format::MAX_RECORD_LEN).
    /// Nothing was written or applied, and the log stays usable.
    #[error("commit {seq} needs a WAL record of {len} bytes, more than the limit of {max}")]
    RecordTooLarge { seq: u64, len: usize, max: usize },
    /// The commit's record can't be encoded (not produced by records of the
    /// commit pipeline). Nothing was written or applied.
    #[error("commit {seq} can't be encoded: {message}")]
    Encode { seq: u64, message: String },
    /// A record was appended out of order: its `seq` is not the log's next.
    /// Nothing was written.
    #[error("WAL record has seq {found}, but the log's next seq is {expected}")]
    OutOfOrder { expected: u64, found: u64 },
    /// Invalid [`WalOptions`](crate::WalOptions).
    #[error("invalid WAL options: {0}")]
    InvalidOptions(String),
    /// The log directory holds a segment at or after the seq the writer
    /// would start at, with records in it.
    #[error("the log already has records from seq {first_seq} on in '{}', but the writer starts at {next_seq}", path.display())]
    LogAhead { next_seq: u64, first_seq: u64, path: PathBuf },

    /// The log's last segment has a torn tail, which recovery must truncate
    /// (at `valid_len`) before a writer continues the log.
    #[error("WAL segment '{}' has a torn tail at offset {valid_len}; recover the log before writing", path.display())]
    TornTail { path: PathBuf, valid_len: u64 },

    // Reading
    /// Damage that is not a torn tail: in a segment other than the last
    /// one, or followed by a record that proves the damaged one had been
    /// synced (see `documentation/formats/wal.md`). Records after it are
    /// never skipped silently.
    #[error("WAL segment '{}' is corrupt at offset {offset} ({damage}), and valid data follows", path.display())]
    Corrupt { path: PathBuf, offset: u64, damage: Damage },
    /// A record with a valid checksum that isn't a valid record.
    #[error("invalid WAL record at offset {offset} of '{}': {invalid}", path.display())]
    InvalidRecord { path: PathBuf, offset: u64, invalid: Invalid },
    /// A record, or a segment's first record, doesn't have the next seq
    /// (a gap or a repeat).
    #[error("WAL record at offset {offset} of '{}' has seq {found}, expected {expected}", path.display())]
    SeqMismatch { path: PathBuf, offset: u64, expected: u64, found: u64 },
    /// A segment's header disagrees with its file name, or it starts at
    /// seq 0.
    #[error("WAL segment '{}' has first seq {found} in its header, expected {expected}", path.display())]
    HeaderMismatch { path: PathBuf, expected: u64, found: u64 },
    /// A segment written in a format version this reader doesn't know.
    #[error("WAL segment '{}' has format version {version}, this reader knows {}", path.display(), crate::format::FORMAT_VERSION)]
    UnsupportedVersion { path: PathBuf, version: u32 },
    /// A segment file larger than the writer ever makes one.
    #[error("WAL segment '{}' has {len} bytes, more than a segment can have", path.display())]
    SegmentTooLarge { path: PathBuf, len: u64 },
    /// The log starts after the requested seq: records are missing.
    #[error("the WAL starts at seq {first_seq}, reading from {from} needs earlier records")]
    MissingRecords { from: u64, first_seq: u64 },
    /// The log ends before the requested seq: records are missing.
    #[error("the WAL ends before seq {from} (its next seq is {next_seq})")]
    LogEndsBefore { from: u64, next_seq: u64 },
}

impl Error {
    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Error::Io { op, path: path.into(), source }
    }
}
