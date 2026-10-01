//! Reports as plain dicts.

use std::path::Path;

use iwdb::{
    BackupReport, CheckpointOutcome, CommitTime, Finding, FsyncPolicy, Kind, RecoveryReport, RestoreReport,
    StoreStatus, VerifyReport,
};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList, PyType};
use pyo3::IntoPyObjectExt;

static DATETIME: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static TIMEDELTA: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static TIMEZONE: PyOnceLock<Py<PyType>> = PyOnceLock::new();

/// A commit time as an aware `datetime` in UTC.
pub fn commit_time<'py>(py: Python<'py>, time: Option<CommitTime>) -> PyResult<Bound<'py, PyAny>> {
    let Some(time) = time else { return Ok(py.None().into_bound(py)) };
    let utc = TIMEZONE.import(py, "datetime", "timezone")?.getattr("utc")?;
    let epoch = DATETIME.import(py, "datetime", "datetime")?.call1((1970, 1, 1, 0, 0, 0, 0, utc))?;
    let delta = TIMEDELTA.import(py, "datetime", "timedelta")?.call1((0, 0, time.micros()))?;
    epoch.call_method1("__add__", (delta,))
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

fn dict<'py>(py: Python<'py>, items: Vec<(&str, Bound<'py, PyAny>)>) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for (key, value) in items {
        dict.set_item(key, value)?;
    }
    Ok(dict.into_any().unbind())
}

fn to<'py, T: IntoPyObject<'py>>(py: Python<'py>, value: T) -> PyResult<Bound<'py, PyAny>> {
    value.into_bound_py_any(py)
}

pub fn recovery(py: Python<'_>, r: &RecoveryReport) -> PyResult<Py<PyAny>> {
    let torn = match &r.torn_tail {
        Some(t) => dict(
            py,
            vec![
                ("path", to(py, path(&t.path))?),
                ("valid_len", to(py, t.valid_len)?),
                ("file_len", to(py, t.file_len)?),
                ("discarded_frames", to(py, t.discarded_frames)?),
            ],
        )?
        .into_bound(py),
        None => py.None().into_bound(py),
    };
    let skipped: Vec<u64> = r.skipped_checkpoints.iter().map(|s| s.seq).collect();
    dict(
        py,
        vec![
            ("created", to(py, r.created)?),
            ("upgraded_from", to(py, r.upgraded_from)?),
            ("checkpoint", to(py, r.checkpoint)?),
            ("skipped_checkpoints", to(py, skipped)?),
            ("replayed", to(py, r.replayed)?),
            ("torn_tail", torn),
            ("removed_temp_files", to(py, r.removed_temp_files.len())?),
            ("seq", to(py, r.seq)?),
        ],
    )
}

pub fn store_status(py: Python<'_>, s: &StoreStatus) -> PyResult<Py<PyAny>> {
    let fsync = match s.fsync {
        FsyncPolicy::Always => "always",
        FsyncPolicy::Group { .. } => "group",
        FsyncPolicy::Off => "off",
    };
    dict(
        py,
        vec![
            ("seq", to(py, s.seq)?),
            ("synced_seq", to(py, s.synced_seq)?),
            ("checkpoint", to(py, s.checkpoint)?),
            ("read_only", to(py, s.read_only.clone())?),
            ("checkpoint_failure", to(py, s.checkpoint_failure.clone())?),
            ("history", to(py, s.history.to_string())?),
            ("fsync", to(py, fsync)?),
            ("archive", to(py, s.archive.as_deref().map(path))?),
            ("recovery", recovery(py, &s.recovery)?.into_bound(py)),
        ],
    )
}

pub fn checkpoint(py: Python<'_>, o: &CheckpointOutcome) -> PyResult<Py<PyAny>> {
    dict(
        py,
        vec![
            ("seq", to(py, o.seq)?),
            ("written", to(py, o.written)?),
            ("removed_checkpoints", to(py, o.removed_checkpoints.clone())?),
            ("removed_segments", to(py, o.removed_segments.clone())?),
        ],
    )
}

pub fn backup(py: Python<'_>, r: &BackupReport) -> PyResult<Py<PyAny>> {
    dict(
        py,
        vec![
            ("path", to(py, path(&r.path))?),
            ("seq", to(py, r.seq)?),
            ("time", commit_time(py, r.time)?),
            ("history", to(py, r.history.to_string())?),
            ("checkpoints", to(py, r.checkpoints.clone())?),
            ("segments", to(py, r.segments.clone())?),
            ("bytes", to(py, r.bytes)?),
        ],
    )
}

fn findings<'py>(py: Python<'py>, list: &[Finding]) -> PyResult<Bound<'py, PyAny>> {
    let out = PyList::empty(py);
    for f in list {
        out.append(dict(
            py,
            vec![("path", to(py, f.path.as_deref().map(path))?), ("message", to(py, f.message.clone())?)],
        )?)?;
    }
    Ok(out.into_any())
}

pub fn verify(py: Python<'_>, r: &VerifyReport) -> PyResult<Py<PyAny>> {
    let kind = match r.kind {
        Kind::DataDir => "data directory",
        Kind::Backup => "backup",
        Kind::Archive => "archive",
    };
    dict(
        py,
        vec![
            ("ok", to(py, r.is_ok())?),
            ("path", to(py, path(&r.path))?),
            ("kind", to(py, kind)?),
            ("version", to(py, r.version)?),
            ("history", to(py, r.history.map(|h| h.to_string()))?),
            ("problems", findings(py, &r.problems)?),
            ("notes", findings(py, &r.notes)?),
            ("checkpoints", to(py, r.checkpoints)?),
            ("checkpoints_checked", to(py, r.checkpoints_checked)?),
            ("segments", to(py, r.segments)?),
            ("records", to(py, r.records)?),
            ("first_seq", to(py, r.first_seq)?),
            ("last_seq", to(py, r.last_seq)?),
            ("seq", to(py, r.seq)?),
            ("time", commit_time(py, r.time)?),
        ],
    )
}

pub fn restore(py: Python<'_>, r: &RestoreReport) -> PyResult<Py<PyAny>> {
    let skipped: Vec<u64> = r.skipped_checkpoints.iter().map(|s| s.seq).collect();
    dict(
        py,
        vec![
            ("path", to(py, path(&r.path))?),
            ("seq", to(py, r.seq)?),
            ("time", commit_time(py, r.time)?),
            ("history", to(py, r.history.to_string())?),
            ("source_history", to(py, r.source_history.map(|h| h.to_string()))?),
            ("checkpoint", to(py, r.checkpoint)?),
            ("skipped_checkpoints", to(py, skipped)?),
            ("replayed", to(py, r.replayed)?),
            ("backup_segments", to(py, r.backup_segments)?),
            ("archive_segments", to(py, r.archive_segments)?),
        ],
    )
}
