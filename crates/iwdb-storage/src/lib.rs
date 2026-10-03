//! Ironweaver DB storage: the write-ahead log, the data directory,
//! checkpoints and recovery, backups, WAL archiving, restore and verify.
//!
//! - [`Wal`]: appends [`CommitRecord`](iwdb_engine::CommitRecord)s to
//!   segment files, fsynced per [`FsyncPolicy`], and fails into a read-only
//!   state on any I/O error.
//! - [`WalReader`] / [`read_log`]: iterate the records from a `seq`,
//!   telling a torn tail (the clean end of the log) from corruption.
//! - [`format`]: the on-disk format (`documentation/formats/wal.md`).
//! - [`io`]: the file operations behind a trait, the seam for fault
//!   injection; `failpoint` (feature `failpoints`) puts failpoints on it.
//! - [`LoggedNamespace`]: a namespace whose commits are logged before they
//!   are applied.
//! - [`layout`]: the data directory, its marker and lock
//!   (`documentation/formats/data-dir.md`).
//! - [`checkpoint`]: checkpoint files and the [`Checkpointer`].
//! - [`recover`]: open a data directory and rebuild its namespace.
//! - [`backup`]: a consistent copy of a data directory up to a synced seq,
//!   with a manifest (`documentation/formats/backup.md`).
//! - [`archive`]: continuous WAL archiving before the checkpointer removes
//!   segments (`documentation/formats/archive.md`).
//! - [`restore`](mod@restore): a new data directory at a seq or time from a backup
//!   and/or an archive.
//! - [`verify`](mod@verify): every file and invariant, checked without writing.
//! - [`inspect`]: a quick, read-only look at a directory.
//! - [`HistoryId`] and [`CommitTime`]: which history a directory holds, and
//!   when a commit was appended (ADR 0009, ADR 0010).
//!
//! The guarantees of each fsync policy are in `documentation/guarantees.md`.

/// Single-namespace convenience: field access to the `default` namespace's
/// part of a report (an empty one if there is none).
macro_rules! default_deref {
    ($report:ty, $part:ty, $field:ident, |$n:ident| $name:expr) => {
        impl std::ops::Deref for $report {
            type Target = $part;
            fn deref(&self) -> &$part {
                static EMPTY: std::sync::OnceLock<$part> = std::sync::OnceLock::new();
                self.$field
                    .iter()
                    .find(|$n| $name == $crate::namespaces::DEFAULT_NAME)
                    .unwrap_or_else(|| EMPTY.get_or_init(Default::default))
            }
        }
    };
}
pub(crate) use default_deref;

pub mod archive;
pub mod backup;
pub mod checkpoint;
mod error;
#[cfg(feature = "failpoints")]
pub mod failpoint;
pub mod format;
mod history;
mod inspect;
pub mod io;
pub mod layout;
mod logged;
pub mod namespaces;
mod reader;
mod recovery;
pub mod restore;
pub mod verify;
mod writer;

pub use backup::{BackupReport, NamespaceBackup};
pub use checkpoint::{CheckpointOutcome, Checkpointer, SkippedCheckpoint};
pub use error::Error;
pub use history::HistoryId;
pub use inspect::{inspect, DirStatus, NamespaceFiles};
pub use iwdb_engine::CommitTime;
pub use logged::{BuildProgress, LockStats, LoggedNamespace, Wait, BUILD_CHUNK};
pub use reader::{
    list_segments, read_log, read_segment, segment_prefix, LogEnd, SegmentEnd, TornTail, WalReader,
    MAX_SEGMENT_FILE_LEN,
};
pub use recovery::{
    read_namespace, recover, start_namespace, CutTail, ReadNamespace, Recovered, RecoveredNamespace, RecoveryReport,
    StoreRecovery,
};
pub use restore::{restore, NamespaceRestore, RestoreReport, RestoreSources, RestoreTarget};
pub use verify::{verify, Finding, Kind, NamespaceVerify, VerifyReport};
pub use writer::{FsyncPolicy, Wal, WalOptions, DEFAULT_SEGMENT_SIZE, MAX_SEGMENT_SIZE, MIN_SEGMENT_SIZE};
