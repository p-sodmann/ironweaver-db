//! Operations on directories that don't need an open store: verify (and,
//! below, restore).

use std::path::Path;

use iwdb_engine::catalog::NamespaceName;
use iwdb_storage::{Error, VerifyReport};

use crate::NAMESPACE;

fn namespace() -> Result<NamespaceName, Error> {
    Ok(NamespaceName::new(NAMESPACE).map_err(iwdb_engine::Error::from)?)
}

/// Verify a data directory or a backup without changing anything: every
/// checksum, every checkpoint, the WAL from its first segment, replay
/// against the newer checkpoints, and the invariants
/// ([`iwdb_storage::verify`] lists them, ADR 0011).
///
/// It takes a shared lock: [`Error::Locked`] while a store has the
/// directory open. Damage is reported in [`VerifyReport::problems`], not as
/// an error.
pub fn verify(dir: &Path) -> Result<VerifyReport, Error> {
    iwdb_storage::verify(dir, &namespace()?)
}
