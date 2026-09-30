//! Ironweaver DB storage: the write-ahead log (step 4). Checkpoints,
//! recovery and backup follow in steps 5 and 7.
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
//!
//! The guarantees of each fsync policy are in `documentation/guarantees.md`.

mod error;
pub mod format;
pub mod io;
mod logged;
mod reader;
mod writer;

pub use error::Error;
pub use logged::LoggedNamespace;
pub use reader::{
    list_segments, read_log, read_segment, LogEnd, SegmentEnd, TornTail, WalReader, MAX_SEGMENT_FILE_LEN,
};
pub use writer::{FsyncPolicy, Wal, WalOptions, DEFAULT_SEGMENT_SIZE, MAX_SEGMENT_SIZE, MIN_SEGMENT_SIZE};
