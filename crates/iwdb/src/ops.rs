//! Operations on directories that don't need an open store: verify and
//! restore.

use std::path::Path;

use iwdb_engine::catalog::NamespaceName;
use iwdb_storage::archive::{verify_archive, ARCHIVE_MARKER_NAME};
use iwdb_storage::io::{LogFs, StdFs};
use iwdb_storage::{Error, RestoreReport, RestoreSources, RestoreTarget, VerifyReport};

use crate::NAMESPACE;

fn namespace() -> Result<NamespaceName, Error> {
    Ok(NamespaceName::new(NAMESPACE).map_err(iwdb_engine::Error::from)?)
}

/// Verify a data directory, a backup or a WAL archive without changing
/// anything: every checksum, every checkpoint, the WAL from its first
/// segment, replay against the newer checkpoints, and the invariants
/// ([`iwdb_storage::verify`] and [`verify_archive`] list them, ADR 0011).
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
/// read. See [`iwdb_storage::restore`] for the steps, the errors and what
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
