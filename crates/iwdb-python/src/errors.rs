//! One Python exception per kind of error, each an `iwdb.Error`, with the
//! Rust message as its text (`documentation/python-api.md`,
//! "Exceptions").

use iwdb::Error;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::PyErr;

/// The exception classes, as Python sees them (`iwdb.Error`, ...).
pub mod exc {
    use pyo3::create_exception;
    use pyo3::exceptions::PyException;

    create_exception!(iwdb, Error, PyException, "Every error of Ironweaver DB.");
    create_exception!(
        iwdb,
        ConflictError,
        Error,
        "A version conflict: an expected_version didn't match. Nothing changed."
    );
    create_exception!(iwdb, ConstraintError, Error, "A unique or required constraint was violated. Nothing changed.");
    create_exception!(
        iwdb,
        NotFoundError,
        Error,
        "A mutation addressed a node or edge that doesn't exist. Nothing changed."
    );
    create_exception!(
        iwdb,
        InvalidError,
        Error,
        "An invalid request: a commit, options, or a directory that isn't a store."
    );
    create_exception!(
        iwdb,
        ReadOnlyError,
        Error,
        "The store is read-only after a failed WAL write or fsync, until reopened."
    );
    create_exception!(iwdb, LockedError, Error, "Another store has the directory (or archive) open.");
    create_exception!(iwdb, IoError, Error, "A file operation failed. A commit's outcome is then unknown.");
    create_exception!(iwdb, CorruptError, Error, "Damaged data: the WAL, a checkpoint, a marker or a manifest.");
    create_exception!(iwdb, ClosedError, Error, "The store is closed.");
    create_exception!(iwdb, InternalError, Error, "A bug: a Rust panic outside the commit path.");
}

use exc::{
    ClosedError, ConflictError, ConstraintError, CorruptError, InternalError, InvalidError, IoError, LockedError,
    NotFoundError, ReadOnlyError,
};

/// Register the exceptions in the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("Error", py.get_type::<exc::Error>())?;
    m.add("ConflictError", py.get_type::<ConflictError>())?;
    m.add("ConstraintError", py.get_type::<ConstraintError>())?;
    m.add("NotFoundError", py.get_type::<NotFoundError>())?;
    m.add("InvalidError", py.get_type::<InvalidError>())?;
    m.add("ReadOnlyError", py.get_type::<ReadOnlyError>())?;
    m.add("LockedError", py.get_type::<LockedError>())?;
    m.add("IoError", py.get_type::<IoError>())?;
    m.add("CorruptError", py.get_type::<CorruptError>())?;
    m.add("ClosedError", py.get_type::<ClosedError>())?;
    m.add("InternalError", py.get_type::<InternalError>())?;
    Ok(())
}

/// The Python exception for an error of the store.
pub fn to_py(error: Error) -> PyErr {
    let message = error.to_string();
    match &error {
        Error::Engine(engine) => engine_error(engine, message),
        Error::ReadOnly { .. } => ReadOnlyError::new_err(message),
        Error::Locked { .. } => LockedError::new_err(message),
        Error::Io { .. } | Error::CheckpointsDisabled { .. } => IoError::new_err(message),
        Error::InvalidOptions(_) => InvalidError::new_err(message),
        Error::Corrupt { .. }
        | Error::InvalidRecord { .. }
        | Error::SeqMismatch { .. }
        | Error::HeaderMismatch { .. }
        | Error::SegmentTooLarge { .. }
        | Error::TornTail { .. }
        | Error::InvalidCheckpoint { .. }
        | Error::NoUsableCheckpoint { .. }
        | Error::ReplayFailed { .. }
        | Error::InvalidDataDir { .. }
        | Error::InvalidManifest { .. }
        | Error::ArchiveConflict { .. }
        | Error::LogEndsBefore { .. }
        | Error::MissingRecords { .. }
        | Error::UnsupportedVersion { .. } => CorruptError::new_err(message),
        // Requests the state doesn't allow: not a store, a backup, an
        // interrupted restore, a full destination, another history, a
        // record too large, a restore target that isn't there
        _ => InvalidError::new_err(message),
    }
}

fn engine_error(error: &iwdb_engine::Error, message: String) -> PyErr {
    use iwdb_engine::Error as E;
    match error {
        E::Conflict { .. } | E::NoMatchingEdge { .. } => ConflictError::new_err(message),
        E::ConstraintViolation { .. } => ConstraintError::new_err(message),
        E::NotFound { .. } => NotFoundError::new_err(message),
        E::Poisoned | E::ApplyFailed { .. } => ReadOnlyError::new_err(message),
        _ => InvalidError::new_err(message),
    }
}

/// Run `f`, turning a panic into `iwdb.InternalError`. The commit path
/// aborts the process on a panic before this sees it (ADR 0008).
pub fn guard<T>(f: impl FnOnce() -> PyResult<T>) -> PyResult<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            Err(InternalError::new_err(format!("internal error (a panic, please report it): {}", message)))
        }
    }
}

/// A `ValueError` with `message`.
pub fn value_error(message: impl Into<String>) -> PyErr {
    PyValueError::new_err(message.into())
}

/// `iwdb.ClosedError`.
pub fn closed() -> PyErr {
    ClosedError::new_err("the store is closed")
}

/// `iwdb.InvalidError` with `message`.
pub fn invalid(message: impl Into<String>) -> PyErr {
    InvalidError::new_err(message.into())
}
