//! Options of a [`Store`](crate::Store).

use std::path::PathBuf;
use std::time::Duration;

use iwdb_storage::WalOptions;

/// Options of [`Store::open`](crate::Store::open).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreOptions {
    /// The WAL: fsync policy (default `always`) and segment size.
    pub wal: WalOptions,
    pub checkpoint: CheckpointOptions,
    /// Create the data directory if it is missing or empty (default true).
    /// If false, opening a directory without a marker fails.
    pub create_if_missing: bool,
    /// Continuous WAL archiving (default off): a directory that receives
    /// every WAL segment before the checkpointer removes it, durably
    /// (`documentation/formats/archive.md`). It is created if missing, and
    /// must belong to the store's history: a restored store needs a new
    /// one (`Error::ArchiveMismatch`). With a backup, it allows restoring
    /// to any later seq it holds.
    pub archive: Option<PathBuf>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            wal: WalOptions::default(),
            checkpoint: CheckpointOptions::default(),
            create_if_missing: true,
            archive: None,
        }
    }
}

/// When checkpoints are written, and how many are kept.
///
/// A checkpoint bounds recovery time (only the WAL after it is replayed)
/// and WAL size (segments it covers are deleted). The triggers are checked
/// by a background thread, which checkpoints without blocking commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointOptions {
    /// Checkpoint when the WAL has grown by this many bytes since the last
    /// checkpoint (default 256 MiB; `None`: never by size).
    pub wal_size: Option<u64>,
    /// Checkpoint when this much time has passed since the last one and
    /// something was committed (default 5 minutes; `None`: never by time).
    pub interval: Option<Duration>,
    /// Checkpoint on [`Store::close`](crate::Store::close) (default true),
    /// so the next open replays nothing.
    pub on_close: bool,
    /// How many checkpoints to keep (at least 1, default 2). The WAL is
    /// kept from the oldest one on, so recovery can fall back to it if a
    /// newer one is damaged.
    pub keep: usize,
    /// Run the size and time triggers in a background thread (default
    /// true). Without it, only [`Store::checkpoint`](crate::Store::checkpoint)
    /// and close write checkpoints.
    pub background: bool,
}

impl Default for CheckpointOptions {
    fn default() -> Self {
        CheckpointOptions {
            wal_size: Some(256 << 20),
            interval: Some(Duration::from_secs(300)),
            on_close: true,
            keep: 2,
            background: true,
        }
    }
}

impl CheckpointOptions {
    pub(crate) fn check(&self) -> Result<(), iwdb_storage::Error> {
        if self.keep == 0 {
            return Err(iwdb_storage::Error::InvalidOptions("at least one checkpoint must be kept".into()));
        }
        if self.interval == Some(Duration::ZERO) {
            return Err(iwdb_storage::Error::InvalidOptions("the checkpoint interval can't be zero".into()));
        }
        Ok(())
    }
}
