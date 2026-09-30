//! Ironweaver DB storage: the write-ahead log (step 4), the data
//! directory, checkpoints and recovery (step 5). Backup follows in step 7.
//!
//! - [`Wal`]: appends [`CommitRecord`](iwdb_engine::CommitRecord)s to
//!   segment files, fsynced per [`FsyncPolicy`], and fails into a read-only
//!   state on any I/O error.
//! - [`WalReader`] / [`read_log`]: iterate the records from a `seq`,
//!   telling a torn tail (the clean end of the log) from corruption.
//! - [`format`]: the on-disk format (`documentation/formats/wal.md`).
//! - [`io`]: the file operations behind a trait, the seam for fault
//!   injection.
//! - [`LoggedNamespace`]: a namespace whose commits are logged before they
//!   are applied.
//! - [`layout`]: the data directory, its marker and lock
//!   (`documentation/formats/data-dir.md`).
//! - [`checkpoint`]: checkpoint files and the [`Checkpointer`].
//! - [`recover`]: open a data directory and rebuild its namespace.
//!
//! The guarantees of each fsync policy are in `documentation/guarantees.md`.

pub mod checkpoint;
mod error;
pub mod format;
pub mod io;
pub mod layout;
mod logged;
mod reader;
mod recovery;
mod writer;

pub use checkpoint::{CheckpointOutcome, Checkpointer, SkippedCheckpoint};
pub use error::Error;
pub use logged::LoggedNamespace;
pub use reader::{
    list_segments, read_log, read_segment, LogEnd, SegmentEnd, TornTail, WalReader, MAX_SEGMENT_FILE_LEN,
};
pub use recovery::{recover, CutTail, Recovered, RecoveryReport};
pub use writer::{FsyncPolicy, Wal, WalOptions, DEFAULT_SEGMENT_SIZE, MAX_SEGMENT_SIZE, MIN_SEGMENT_SIZE};
