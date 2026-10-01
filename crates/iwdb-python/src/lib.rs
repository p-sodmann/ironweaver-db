//! Python bindings of the embedded store (`import iwdb`): PyO3, built with
//! maturin (`pyproject.toml`). The API is a contract shared with the remote
//! client of step 14 (`documentation/python-api.md`, ADR 0013).
//!
//! The bindings only translate (design rule 8): Python values to
//! `iwdb` types and back, errors to exceptions. Every call that does I/O
//! or may wait releases the GIL. Python lives only in this crate (design
//! rule 1).

mod convert;
mod errors;
mod reports;
mod store;

use std::path::PathBuf;

use iwdb::{CommitTime, Error, RestoreSources, RestoreTarget};
use pyo3::prelude::*;

use crate::errors::{guard, invalid, to_py, value_error};

/// Check a data directory, backup or archive without changing anything.
#[pyfunction]
fn verify(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
    guard(|| {
        let report = py.detach(|| iwdb::verify(&path)).map_err(to_py)?;
        reports::verify(py, &report)
    })
}

/// Restore into `dest` from a backup and/or an archive, to `seq`, to the
/// last commit at or before `time` (an aware datetime), or to the latest.
#[pyfunction]
#[pyo3(signature = (dest, *, backup = None, archive = None, seq = None, time = None, namespaces = None))]
fn restore(
    py: Python<'_>,
    dest: PathBuf,
    backup: Option<PathBuf>,
    archive: Option<PathBuf>,
    seq: Option<u64>,
    time: Option<&Bound<'_, PyAny>>,
    namespaces: Option<Vec<String>>,
) -> PyResult<Py<PyAny>> {
    guard(|| {
        let target = match (seq, time) {
            (Some(_), Some(_)) => return Err(value_error("restore takes seq or time, not both")),
            (Some(seq), None) => RestoreTarget::Seq(seq),
            (None, Some(time)) => {
                let value = convert::to_value(time, 1)?;
                let iwdb::Value::DateTime(t) = value else {
                    return Err(value_error("time must be a datetime.datetime"));
                };
                if t.offset.is_none() {
                    return Err(value_error("time must be an aware datetime (with a UTC offset)"));
                }
                RestoreTarget::Time(CommitTime(t.micros))
            }
            (None, None) => RestoreTarget::Latest,
        };
        let sources = RestoreSources { backup, archive };
        let only: Option<Vec<iwdb::NamespaceName>> = namespaces
            .map(|list| {
                list.into_iter().map(|n| iwdb::NamespaceName::new(n).map_err(|e| invalid(e.to_string()))).collect()
            })
            .transpose()?;
        let report =
            py.detach(|| iwdb::restore_namespaces(&dest, &sources, target, only.as_deref())).map_err(|e| match e {
                // The sources don't reach the target: a request, not damage
                Error::LogEndsBefore { .. }
                | Error::MissingRecords { .. }
                | Error::NoCommitAtOrBefore { .. }
                | Error::AmbiguousTarget { .. } => invalid(e.to_string()),
                other => to_py(other),
            })?;
        reports::restore(py, &report)
    })
}

/// Panic inside the bindings (tests only): it must raise
/// `iwdb.InternalError` and leave the interpreter running.
#[pyfunction]
fn _panic_for_tests() -> PyResult<()> {
    guard(|| panic!("a test panic"))
}

#[pymodule]
fn _iwdb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<store::PyStore>()?;
    m.add_class::<store::PyNamespace>()?;
    m.add_class::<store::PyTransaction>()?;
    m.add_function(wrap_pyfunction!(verify, m)?)?;
    m.add_function(wrap_pyfunction!(restore, m)?)?;
    m.add_function(wrap_pyfunction!(_panic_for_tests, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    errors::register(m)
}
