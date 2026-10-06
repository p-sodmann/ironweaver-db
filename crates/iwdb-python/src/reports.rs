//! Reports as plain dicts.

use std::path::Path;

use iwdb::import::{ExportReport, ImportReport, MergeReport};
use iwdb::{
    BackupReport, CheckpointOutcome, CommitTime, Finding, FsyncPolicy, IndexState, IndexStatus, Kind, MarkStatus,
    NamespaceStatus, RecoveryReport, RestoreReport, StoreRecovery, StoreStatus, VerifyReport,
};
use pyo3::IntoPyObjectExt;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList, PyType};

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

fn ns_recovery(py: Python<'_>, r: &RecoveryReport) -> PyResult<Py<PyAny>> {
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
            ("checkpoint", to(py, r.checkpoint)?),
            ("skipped_checkpoints", to(py, skipped)?),
            ("replayed", to(py, r.replayed)?),
            ("torn_tail", torn),
            ("seq", to(py, r.seq)?),
            ("finished_import", to(py, r.finished_import)?),
        ],
    )
}

/// What an import did.
pub fn import(py: Python<'_>, r: &ImportReport) -> PyResult<Py<PyAny>> {
    dict(
        py,
        vec![
            ("id", to(py, r.event.id)?),
            ("name", to(py, r.event.name.as_str())?),
            ("time", commit_time(py, Some(r.event.time))?),
            ("format", to(py, r.format.name())?),
            ("seq", to(py, r.seq)?),
            ("nodes", to(py, r.nodes)?),
            ("edges", to(py, r.edges)?),
            ("indexes", to(py, r.indexes.iter().map(|p| p.keys().to_vec()).collect::<Vec<_>>())?),
            ("dropped", to(py, r.dropped.clone())?),
            ("bytes_read", to(py, r.bytes_read)?),
            ("checkpoint_bytes", to(py, r.checkpoint_bytes)?),
        ],
    )
}

/// What a merge did.
pub fn merge(py: Python<'_>, r: &MergeReport) -> PyResult<Py<PyAny>> {
    dict(
        py,
        vec![
            ("format", to(py, r.format.name())?),
            ("nodes", to(py, r.nodes)?),
            ("edges", to(py, r.edges)?),
            ("created_indexes", to(py, r.created_indexes.iter().map(|p| p.keys().to_vec()).collect::<Vec<_>>())?),
            ("dropped", to(py, r.dropped.clone())?),
            ("bytes_read", to(py, r.bytes_read)?),
            ("commits", to(py, r.commits)?),
            ("first_seq", to(py, r.first_seq)?),
            ("last_seq", to(py, r.last_seq)?),
        ],
    )
}

/// What an export did.
pub fn export(py: Python<'_>, r: &ExportReport) -> PyResult<Py<PyAny>> {
    dict(
        py,
        vec![
            ("format", to(py, r.format.name())?),
            ("seq", to(py, r.seq)?),
            ("nodes", to(py, r.nodes)?),
            ("edges", to(py, r.edges)?),
            ("bytes", to(py, r.bytes)?),
        ],
    )
}

/// What recovery did: the fields of the `default` namespace's recovery,
/// the store's own, and `namespaces`.
pub fn recovery(py: Python<'_>, r: &StoreRecovery) -> PyResult<Py<PyAny>> {
    let base = ns_recovery(py, r)?;
    let out = base.bind(py).cast::<PyDict>()?.clone();
    out.set_item("created", r.created)?;
    out.set_item("upgraded_from", r.upgraded_from)?;
    out.set_item("removed_temp_files", r.removed_temp_files.len())?;
    out.set_item("removed_orphans", r.removed_orphans.clone())?;
    let namespaces = PyDict::new(py);
    for (name, ns) in &r.namespaces {
        namespaces.set_item(name, ns_recovery(py, ns)?)?;
    }
    out.set_item("namespaces", namespaces)?;
    Ok(out.into_any().unbind())
}

pub fn indexes(py: Python<'_>, list: &[IndexStatus]) -> PyResult<Py<PyAny>> {
    let out = PyList::empty(py);
    for i in list {
        let (state, scanned, total) = match &i.state {
            IndexState::Ready => ("ready", None, None),
            IndexState::Building { scanned, total } => ("building", Some(*scanned), Some(*total)),
        };
        out.append(dict(
            py,
            vec![
                ("path", to(py, i.path.keys().to_vec())?),
                ("state", to(py, state)?),
                ("declared", to(py, i.declared)?),
                ("unique", to(py, i.unique)?),
                ("scanned", to(py, scanned)?),
                ("total", to(py, total)?),
                ("entries", to(py, i.size.map(|s| s.entries))?),
                ("distinct_keys", to(py, i.size.map(|s| s.distinct_keys))?),
                ("memory_bytes", to(py, i.size.map(|s| s.memory_bytes))?),
            ],
        )?)?;
    }
    Ok(out.into_any().unbind())
}

pub fn namespace_status(py: Python<'_>, n: &NamespaceStatus) -> PyResult<Py<PyAny>> {
    dict(
        py,
        vec![
            ("id", to(py, n.id)?),
            ("name", to(py, n.name.clone())?),
            ("created", commit_time(py, Some(n.created))?),
            ("seq", to(py, n.seq)?),
            ("synced_seq", to(py, n.synced_seq)?),
            ("checkpoint", to(py, n.checkpoint)?),
            ("unsynced", to(py, n.unsynced)?),
            ("since_checkpoint", to(py, n.since_checkpoint)?),
            ("last_checkpoint", commit_time(py, n.last_checkpoint)?),
            ("read_only", to(py, n.read_only.clone())?),
            ("checkpoint_failure", to(py, n.checkpoint_failure.clone())?),
            ("nodes", to(py, n.nodes)?),
            ("edges", to(py, n.edges)?),
            ("memory_bytes", to(py, n.memory_bytes)?),
            ("constraints", to(py, n.constraints)?),
            ("indexes", indexes(py, &n.indexes)?.into_bound(py)),
            ("recovery", ns_recovery(py, &n.recovery)?.into_bound(py)),
            ("marks", marks(py, &n.marks)?.into_bound(py)),
        ],
    )
}

/// The marks as `{name: {"position": int, "seq": int}}`.
fn marks(py: Python<'_>, marks: &[MarkStatus]) -> PyResult<Py<PyAny>> {
    let out = PyDict::new(py);
    for m in marks {
        let entry = dict(py, vec![("position", to(py, m.position)?), ("seq", to(py, m.seq)?)])?;
        out.set_item(&m.name, entry)?;
    }
    Ok(out.into_any().unbind())
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
            ("catalog_failure", to(py, s.catalog_failure.clone())?),
            ("recovery", recovery(py, &s.recovery)?.into_bound(py)),
            ("namespaces", {
                let list = PyList::empty(py);
                for n in &s.namespaces {
                    list.append(namespace_status(py, n)?)?;
                }
                list.into_any()
            }),
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
    let namespaces = PyList::empty(py);
    for n in &r.namespaces {
        namespaces.append(dict(
            py,
            vec![
                ("id", to(py, n.id)?),
                ("name", to(py, n.name.clone())?),
                ("seq", to(py, n.seq)?),
                ("time", commit_time(py, n.time)?),
                ("checkpoints", to(py, n.checkpoints.clone())?),
                ("segments", to(py, n.segments.clone())?),
            ],
        )?)?;
    }
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
            ("namespaces", namespaces.into_any()),
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
            ("namespaces", {
                let list = PyList::empty(py);
                for n in &r.namespaces {
                    list.append(dict(
                        py,
                        vec![
                            ("id", to(py, n.id)?),
                            ("name", to(py, n.name.clone())?),
                            ("checkpoints", to(py, n.checkpoints)?),
                            ("checkpoints_checked", to(py, n.checkpoints_checked)?),
                            ("segments", to(py, n.segments)?),
                            ("records", to(py, n.records)?),
                            ("first_seq", to(py, n.first_seq)?),
                            ("last_seq", to(py, n.last_seq)?),
                            ("seq", to(py, n.seq)?),
                            ("time", commit_time(py, n.time)?),
                        ],
                    )?)?;
                }
                list.into_any()
            }),
        ],
    )
}

pub fn restore(py: Python<'_>, r: &RestoreReport) -> PyResult<Py<PyAny>> {
    let skipped: Vec<u64> = r.skipped_checkpoints.iter().map(|s| s.seq).collect();
    let namespaces = PyList::empty(py);
    for n in &r.namespaces {
        let skipped: Vec<u64> = n.skipped_checkpoints.iter().map(|s| s.seq).collect();
        namespaces.append(dict(
            py,
            vec![
                ("id", to(py, n.id)?),
                ("name", to(py, n.name.clone())?),
                ("seq", to(py, n.seq)?),
                ("time", commit_time(py, n.time)?),
                ("checkpoint", to(py, n.checkpoint)?),
                ("skipped_checkpoints", to(py, skipped)?),
                ("replayed", to(py, n.replayed)?),
                ("backup_segments", to(py, n.backup_segments)?),
                ("archive_segments", to(py, n.archive_segments)?),
            ],
        )?)?;
    }
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
            ("namespaces", namespaces.into_any()),
        ],
    )
}
