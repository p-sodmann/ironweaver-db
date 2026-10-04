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
    /// more writes until it is reopened.
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
    /// The change stream was asked for a seq whose WAL segment is no
    /// longer retained (ADR 0031): the oldest retained seq is `first_seq`.
    #[error("seq {from} is no longer retained; the oldest retained seq is {first_seq}")]
    NotRetained { from: u64, first_seq: u64 },

    // Data directory (step 5, `documentation/formats/data-dir.md`)
    /// Another store, in this process or another one, has the data
    /// directory open (it holds the lock on `path`).
    #[error("the data directory is in use: '{}' is locked by another store", path.display())]
    Locked { path: PathBuf },
    /// The directory is not an Ironweaver DB data directory: its marker is
    /// missing or isn't ours, and it holds other files. Nothing was changed.
    #[error("'{}' is not an Ironweaver DB data directory: {reason}", path.display())]
    NotADataDir { path: PathBuf, reason: String },
    /// A data directory that is damaged: an unreadable marker, or a missing
    /// `wal/` or `checkpoints/` directory.
    #[error("the data directory '{}' is damaged: {reason}", path.display())]
    InvalidDataDir { path: PathBuf, reason: String },
    /// The directory is a backup (it has a `BACKUP` manifest). A store
    /// doesn't open a backup, so that the backup stays as it was and two
    /// stores never continue one history: restore it instead.
    #[error("'{}' is a backup; restore it into a new directory instead of opening it", path.display())]
    IsBackup { path: PathBuf },
    /// The directory holds an interrupted restore (a `RESTORING` file).
    /// Remove it and restore again.
    #[error("'{}' holds an interrupted restore; remove it and restore again", path.display())]
    InterruptedRestore { path: PathBuf },
    /// The data directory has a layout version this version doesn't know
    /// (written by a newer Ironweaver DB).
    #[error("the data directory '{}' has layout version {version}, this version knows {}", path.display(), crate::layout::LAYOUT_VERSION)]
    UnsupportedLayout { path: PathBuf, version: u32 },

    // Backup, archiving and restore (step 7, ADR 0009)
    /// A backup or restore needs a missing or empty destination directory;
    /// this one holds files. Nothing was written.
    #[error("'{}' exists and isn't empty; a backup or restore needs a new or empty directory", path.display())]
    DestinationNotEmpty { path: PathBuf },
    /// A backup's manifest (`BACKUP`) can't be read: damaged, truncated,
    /// or of a newer version.
    #[error("the backup manifest '{}' is invalid: {reason}", path.display())]
    InvalidManifest { path: PathBuf, reason: String },

    /// The WAL archive belongs to another history (a restored store, or
    /// another store): archive into a new directory. Nothing was written.
    #[error("the WAL archive '{}' belongs to history {found}, not to this store's {expected}; use a new archive directory", path.display())]
    ArchiveMismatch { path: PathBuf, expected: crate::HistoryId, found: crate::HistoryId },
    /// The archive holds a segment of the same name with other contents
    /// (another history, or damage). It is left as it is, and the WAL
    /// segment is not removed.
    #[error("the archive's '{}' differs from the WAL segment of the same name (another history, or damage)", path.display())]
    ArchiveConflict { path: PathBuf },
    /// The directory is not a WAL archive: no marker and other files, or a
    /// damaged or newer marker.
    #[error("'{}' is not a WAL archive: {reason}", path.display())]
    NotAnArchive { path: PathBuf, reason: String },
    /// A restore's backup and archive belong to different histories (or
    /// the backup's history is unknown, layout 1).
    #[error("the backup's history {backup} and the archive's {archive} differ; they can't be combined")]
    HistoryMismatch { backup: String, archive: String },

    /// A restore to a time found no record with a commit time at or
    /// before it in the sources' WAL (`first`: the earliest commit time
    /// there, if any).
    #[error("no commit at or before {time} in the WAL to restore from (the earliest is {})", first.map_or("none".to_owned(), |t| t.to_string()))]
    NoCommitAtOrBefore { time: crate::CommitTime, first: Option<crate::CommitTime> },

    // Checkpoints and recovery (step 5)
    /// A checkpoint file that can't be loaded: a checksum or format error,
    /// or content that doesn't match its name or namespace. Recovery skips
    /// it and falls back to an older checkpoint.
    #[error("checkpoint '{}' can't be loaded: {reason}", path.display())]
    InvalidCheckpoint { path: PathBuf, reason: String },
    /// Recovery found no checkpoint it can start from: the newest one that
    /// loads (or an empty namespace, seq 0, if none does) needs the WAL
    /// from seq `from`, but the WAL starts at `first_seq`. The segments it
    /// would need were deleted after newer checkpoints covered them.
    /// Nothing was changed.
    #[error(
        "no usable checkpoint: recovery needs the WAL from seq {from}, but it starts at {first_seq} (checkpoints skipped: {})",
        crate::checkpoint::describe(skipped)
    )]
    NoUsableCheckpoint { from: u64, first_seq: u64, skipped: Vec<crate::SkippedCheckpoint> },
    /// Replaying a logged record failed during recovery or in the
    /// checkpointer (`ApplyFailed`, which includes `GraphError::Internal`).
    /// Recovery stops and changes nothing; the log is not truncated.
    #[error("replaying WAL record {seq} failed: {source}")]
    ReplayFailed {
        seq: u64,
        #[source]
        source: iwdb_engine::Error,
    },
    /// Checkpoints are disabled until the store is reopened, after a
    /// failure that leaves the durability of a checkpoint or a deletion
    /// unknown (a failed directory fsync is never retried). Commits are
    /// not affected; the WAL just isn't cut.
    #[error("checkpoints are disabled until the store is reopened, after an earlier failure: {cause}")]
    CheckpointsDisabled { cause: String },

    // Namespaces (step 9, ADR 0017)
    /// The store's namespace log can't be read: missing, corrupt (damage
    /// that isn't a torn tail), of a newer version, or events that don't
    /// follow from each other.
    #[error("the namespace log '{}' is invalid: {reason}", path.display())]
    InvalidNamespaceLog { path: PathBuf, reason: String },
    /// A namespace with this name exists already.
    #[error("namespace '{name}' exists already")]
    NamespaceExists { name: String },
    /// There is no namespace with this name.
    #[error("no namespace '{name}'")]
    NoSuchNamespace { name: String },
    /// The namespace was dropped while the request ran (or waited): its
    /// data is gone, and so is its history.
    #[error("namespace '{name}' was dropped")]
    NamespaceDropped { name: String },
    /// A namespace the log lists has a directory that is missing or
    /// incomplete: files were removed by hand, or the disk lost them.
    #[error("namespace '{name}' (id {id}) is damaged: {reason}")]
    NamespaceDamaged { id: u64, name: String, reason: String },

    /// An import was refused: the file is invalid, or the graph it holds
    /// breaks an invariant of the database (ADR 0033). Nothing was created.
    #[error("invalid import: {reason}")]
    InvalidImport { reason: String },

    /// A restore to a seq with several namespaces to restore: seqs belong
    /// to one namespace each, so the restore must name which one (`only`).
    #[error("a restore to a seq must select one namespace; the restore would hold {}", namespaces.join(", "))]
    AmbiguousTarget { namespaces: Vec<String> },

    // Requests (step 8)
    /// A request didn't finish within its timeout: waiting for a `min_seq`,
    /// or a job cancelled at its deadline. Nothing changed.
    #[error("{what} timed out after {after:?}")]
    Timeout { what: String, after: std::time::Duration },
    /// The caller cancelled the request (its `cancel::Token`).
    #[error("the request was cancelled")]
    Cancelled,
    /// A read-your-writes seq of another history: a store restored since
    /// (a restore starts a new history, ADR 0009), or another store. The
    /// seq says nothing about this store's state.
    #[error("the seq belongs to history {given}, but the store holds history {store}")]
    OtherHistory { given: crate::HistoryId, store: crate::HistoryId },
}

impl Error {
    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Error::Io { op, path: path.into(), source }
    }
}
