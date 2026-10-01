//! Operations on directories that don't need an open store: status,
//! verify and restore.

use std::path::Path;

use iwdb_engine::catalog::NamespaceName;
use iwdb_storage::archive::{verify_archive, ARCHIVE_MARKER_NAME};
use iwdb_storage::io::{LogFs, StdFs};
use iwdb_storage::{inspect, DirStatus, Error, Kind, RestoreReport, RestoreSources, RestoreTarget, VerifyReport};

use crate::{Store, StoreOptions, StoreStatus, NAMESPACE};

fn namespace() -> Result<NamespaceName, Error> {
    Ok(NamespaceName::new(NAMESPACE).map_err(iwdb_engine::Error::from)?)
}

/// Verify a data directory, a backup or a WAL archive without changing
/// anything: every checksum, every checkpoint, the WAL from its first
/// segment, replay against the newer checkpoints, and the invariants
/// ([`iwdb_storage::verify`](fn@iwdb_storage::verify) and [`verify_archive`] list them, ADR 0011).
///
/// On a data directory or backup it takes a shared lock:
/// [`Error::Locked`] while a store has it open. Damage is reported in
/// [`VerifyReport::problems`], not as an error.
pub fn verify(dir: &Path) -> Result<VerifyReport, Error> {
    if dir.join(ARCHIVE_MARKER_NAME).exists() {
        return verify_archive(dir);
    }
    iwdb_storage::verify(dir, &namespace()?)
}

/// Restore a store into `dest`, a new or empty directory, at `target`:
/// from a backup, a WAL archive, or both (point-in-time recovery). The
/// result is a data directory with a new history id and one checkpoint at
/// the target seq, which [`Store::open`](crate::Store::open) opens
/// directly; its next commit is the target seq + 1. The sources are only
/// read. See [`iwdb_storage::restore`](fn@iwdb_storage::restore) for the steps, the errors and what
/// an interrupted restore leaves (ADR 0009).
///
/// A restored store has a new history: give it a new archive directory.
pub fn restore(dest: &Path, sources: &RestoreSources, target: RestoreTarget) -> Result<RestoreReport, Error> {
    restore_with(&StdFs, dest, sources, target)
}

/// [`restore`] through the file operations `fs` (tests inject faults with
/// it).
pub fn restore_with<F: LogFs>(
    fs: &F,
    dest: &Path,
    sources: &RestoreSources,
    target: RestoreTarget,
) -> Result<RestoreReport, Error> {
    let report = iwdb_storage::restore(fs, sources, target, dest, &namespace()?)?;
    log::info!(
        "restored '{}' to seq {} (history {}, from checkpoint {:?} and {} WAL records)",
        dest.display(),
        report.seq,
        report.history,
        report.checkpoint,
        report.replayed
    );
    Ok(report)
}

/// What [`status`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// What the files say (after recovery, if the store was opened).
    pub files: DirStatus,
    /// The store as it opened, for a data directory that no store had open.
    pub store: Option<StoreStatus>,
}

/// The status of a data directory, a backup or a WAL archive. A data
/// directory that no store has open is **opened** with `options` (which
/// runs recovery: it may cut a torn tail, remove temporary files and
/// upgrade a layout 1 directory) and closed again without a checkpoint;
/// the store's status says what recovery did. One that a store has open
/// (`files.in_use`), a backup and an archive are only read.
///
/// `options.create_if_missing` is ignored: status never creates a store.
pub fn status(dir: &Path, options: StoreOptions) -> Result<Status, Error> {
    let files = inspect(dir)?;
    if files.kind != Kind::DataDir || files.in_use {
        return Ok(Status { files, store: None });
    }
    let mut options = options;
    options.create_if_missing = false;
    options.checkpoint.background = false;
    options.checkpoint.on_close = false;
    let store = Store::open(dir, options)?;
    let status = store.status();
    drop(store);
    Ok(Status { files: inspect(dir)?, store: Some(status) })
}
