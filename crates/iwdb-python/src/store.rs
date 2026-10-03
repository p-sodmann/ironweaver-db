//! `iwdb.Store`: translations to the `Database`
//! trait, served by `iwdb::Embedded` (design rule 8), and to
//! `iwdb::Store` for what the trait doesn't cover (backups, checkpoints,
//! syncs, the store's status). See `documentation/python-api.md`.

use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use iwdb::{
    AttrPath, CatalogChange, CheckpointOptions, CommitOptions, CommitResult, Constraint, ConstraintKind, EdgeId,
    Embedded, Error, FsyncPolicy, IdempotencyKey, IndexDef, Label, NAMESPACE, NamespaceResult, QueryConfig, Store,
    StoreOptions, Target, WalOptions,
};
use iwdb_query::exec::block_on;
use iwdb_query::{Database, LimitConfig, QueryOptions};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};

use crate::convert::from_attrs;
use crate::errors::{closed, guard, invalid, query_to_py, to_py, value_error};
use crate::namespace::PyNamespace;
use crate::reports;
use crate::transaction::PyTransaction;

/// A store, shared between Python threads, served as a `Database`
/// (`iwdb::Embedded`). Calls hold the read side of the lock (with the GIL
/// released) while they run; `close` takes the write side, so it waits for
/// calls in progress, and leaves `None`.
#[pyclass(module = "iwdb", name = "Store", frozen)]
pub struct PyStore {
    inner: RwLock<Option<Embedded>>,
    path: PathBuf,
}

/// The embedded database's config: the default limits, and no cap on
/// timeouts (`timeout=inf` means none; ADR 0020).
fn query_config() -> QueryConfig {
    QueryConfig {
        limits: LimitConfig { max_timeout: Duration::MAX, ..LimitConfig::default() },
        ..QueryConfig::default()
    }
}

impl PyStore {
    /// Run `f` on the open store with the GIL released (for what the
    /// `Database` trait doesn't cover).
    pub fn with<R: Send>(&self, py: Python<'_>, f: impl FnOnce(&Store) -> Result<R, Error> + Send) -> PyResult<R> {
        let result = py.detach(|| {
            let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            inner.as_ref().map(|db| f(db.store()))
        });
        match result {
            Some(result) => result.map_err(to_py),
            None => Err(closed()),
        }
    }

    /// Run `f` on the `Database` with the GIL released.
    pub fn query<R: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&Embedded) -> Result<R, iwdb_query::Error> + Send,
    ) -> PyResult<R> {
        let result = py.detach(|| {
            let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            inner.as_ref().map(f)
        });
        match result {
            Some(result) => result.map_err(query_to_py),
            None => Err(closed()),
        }
    }
}

fn fsync_policy(fsync: &str, max_delay: f64, max_batch: u32) -> PyResult<FsyncPolicy> {
    Ok(match fsync {
        "always" => FsyncPolicy::Always,
        "group" => FsyncPolicy::Group { max_delay: seconds(max_delay, "group_max_delay")?, max_batch },
        "off" => FsyncPolicy::Off,
        other => return Err(value_error(format!("fsync must be 'always', 'group' or 'off', not '{}'", other))),
    })
}

/// Commit options with an optional idempotency key.
pub(crate) fn commit_options(idempotency_key: Option<String>) -> PyResult<CommitOptions> {
    let idempotency_key = idempotency_key.map(IdempotencyKey::new).transpose().map_err(|e| invalid(e.to_string()))?;
    Ok(CommitOptions { idempotency_key })
}

/// Read options: `min_seq`, and `timeout` in seconds (`None`: the default;
/// `inf`: none).
pub(crate) fn read_options(min_seq: Option<u64>, timeout: Option<f64>) -> PyResult<QueryOptions> {
    let timeout = match timeout {
        Some(t) if t == f64::INFINITY => Some(Duration::MAX),
        Some(t) => Some(seconds(t, "timeout")?),
        None => None,
    };
    Ok(QueryOptions { min_seq, timeout, ..QueryOptions::default() })
}

pub(crate) fn seconds(s: f64, what: &str) -> PyResult<Duration> {
    Duration::try_from_secs_f64(s).map_err(|_| value_error(format!("{} must be a number of seconds, at least 0", what)))
}

/// An attribute path: a `str` (one key) or a list of `str`.
pub(crate) fn attr_path(path: &Bound<'_, PyAny>) -> PyResult<AttrPath> {
    let keys: Vec<String> = if path.is_instance_of::<PyString>() {
        vec![path.extract()?]
    } else {
        path.extract().map_err(|_| PyTypeError::new_err("a path is a str or a list of str"))?
    };
    AttrPath::new(keys).map_err(|e| invalid(e.to_string()))
}

pub(crate) fn constraint(kind: &str, label: &str, path: &Bound<'_, PyAny>) -> PyResult<Constraint> {
    let kind = match kind {
        "unique" => ConstraintKind::Unique,
        "required" => ConstraintKind::Required,
        other => return Err(value_error(format!("a constraint is 'unique' or 'required', not '{}'", other))),
    };
    let label = Label::new(label).map_err(|e| invalid(e.to_string()))?;
    Ok(Constraint { kind, label, path: attr_path(path)? })
}

/// A commit's result as a dict.
pub fn commit_result(py: Python<'_>, result: &CommitResult) -> PyResult<Py<PyAny>> {
    let (nodes, edges) = (PyDict::new(py), PyDict::new(py));
    for (target, version) in &result.versions {
        match target {
            Target::Node(id) => nodes.set_item(id, version)?,
            Target::Edge(id) => edges.set_item(id.0, version)?,
        }
    }
    let versions = PyDict::new(py);
    versions.set_item("nodes", nodes)?;
    versions.set_item("edges", edges)?;
    let dict = PyDict::new(py);
    dict.set_item("seq", result.seq)?;
    dict.set_item("edge_ids", PyList::new(py, result.edge_ids.iter().map(|e| e.0))?)?;
    dict.set_item("versions", versions)?;
    dict.set_item("time", reports::commit_time(py, result.time)?)?;
    dict.set_item("deduplicated", result.deduplicated)?;
    Ok(dict.into_any().unbind())
}

/// The result of a namespace operation as a dict.
fn namespace_result(py: Python<'_>, r: &NamespaceResult) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item("id", r.event.id)?;
    dict.set_item("name", r.event.name.as_str())?;
    dict.set_item("time", reports::commit_time(py, Some(r.event.time))?)?;
    dict.set_item("event", r.event.seq)?;
    dict.set_item("deduplicated", r.deduplicated)?;
    Ok(dict.into_any().unbind())
}

#[pymethods]
impl PyStore {
    /// Open (and create) the store in the directory `path`, and recover it.
    #[staticmethod]
    #[pyo3(signature = (
        path, *, create_if_missing = true, fsync = "always", group_max_delay = 0.01, group_max_batch = 64,
        segment_size = 64 << 20, checkpoint_wal_size = Some(256 << 20), checkpoint_interval = Some(300.0),
        checkpoint_on_close = true, checkpoint_keep = 2, checkpoint_background = true, archive = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn open(
        py: Python<'_>,
        path: PathBuf,
        create_if_missing: bool,
        fsync: &str,
        group_max_delay: f64,
        group_max_batch: u32,
        segment_size: u64,
        checkpoint_wal_size: Option<u64>,
        checkpoint_interval: Option<f64>,
        checkpoint_on_close: bool,
        checkpoint_keep: usize,
        checkpoint_background: bool,
        archive: Option<PathBuf>,
    ) -> PyResult<Self> {
        guard(|| {
            let options = StoreOptions {
                wal: WalOptions { fsync: fsync_policy(fsync, group_max_delay, group_max_batch)?, segment_size },
                checkpoint: CheckpointOptions {
                    wal_size: checkpoint_wal_size,
                    interval: checkpoint_interval.map(|s| seconds(s, "checkpoint_interval")).transpose()?,
                    on_close: checkpoint_on_close,
                    keep: checkpoint_keep,
                    background: checkpoint_background,
                },
                create_if_missing,
                archive,
            };
            let store = py.detach(|| Store::open(&path, options)).map_err(to_py)?;
            let db = Embedded::new(store, query_config()).map_err(query_to_py)?;
            Ok(PyStore { inner: RwLock::new(Some(db)), path })
        })
    }

    /// Stop the background threads, fsync the WALs, checkpoint (if
    /// `checkpoint_on_close`) and release the lock. Waits for calls in
    /// progress on other threads. Closing a closed store does nothing.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        guard(|| {
            let result = py.detach(|| {
                let db = self.inner.write().unwrap_or_else(PoisonError::into_inner).take();
                db.map(Embedded::close)
            });
            result.transpose().map(drop).map_err(to_py)
        })
    }

    /// Whether the store is closed.
    #[getter]
    fn closed(&self) -> bool {
        self.inner.read().unwrap_or_else(PoisonError::into_inner).is_none()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (_exc_type = None, _exc = None, _traceback = None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<Bound<'_, PyAny>>,
        _exc: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self) -> String {
        let state = if self.closed() { "closed" } else { "open" };
        format!("<iwdb.Store '{}' ({})>", self.path.display(), state)
    }

    // ---- namespaces ----

    /// Create a namespace. With `idempotency_key`, a retry returns the
    /// original event.
    #[pyo3(signature = (name, *, idempotency_key = None))]
    fn create_namespace(&self, py: Python<'_>, name: &str, idempotency_key: Option<String>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let key = idempotency_key.map(IdempotencyKey::new).transpose().map_err(|e| invalid(e.to_string()))?;
            let name = name.to_owned();
            let result = self.query(py, move |db| block_on(db.create_namespace(&name, key)))?;
            namespace_result(py, &result)
        })
    }

    /// Drop a namespace and its data. `"default"` can't be dropped.
    #[pyo3(signature = (name, *, idempotency_key = None))]
    fn drop_namespace(&self, py: Python<'_>, name: &str, idempotency_key: Option<String>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let key = idempotency_key.map(IdempotencyKey::new).transpose().map_err(|e| invalid(e.to_string()))?;
            let name = name.to_owned();
            let result = self.query(py, move |db| block_on(db.drop_namespace(&name, key)))?;
            namespace_result(py, &result)
        })
    }

    /// The namespaces, sorted by name.
    fn namespaces(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let list = self.query(py, |db| block_on(db.namespaces()))?;
            let out = PyList::empty(py);
            for info in list {
                let dict = PyDict::new(py);
                dict.set_item("id", info.id)?;
                dict.set_item("name", info.name.as_str())?;
                dict.set_item("created", reports::commit_time(py, Some(info.created))?)?;
                out.append(dict)?;
            }
            Ok(out.into_any().unbind())
        })
    }

    /// A handle on the namespace `name`.
    fn namespace(slf: Py<Self>, py: Python<'_>, name: &str) -> PyResult<PyNamespace> {
        guard(|| {
            let owned = name.to_owned();
            let id = slf.get().with(py, move |s| s.namespace(&owned).map(|ns| ns.id()))?;
            Ok(PyNamespace { store: slf, name: name.to_owned(), id })
        })
    }

    // ---- the default namespace ----

    /// A transaction on `"default"`: mutations committed as one, when its
    /// `with` block ends or by `commit()`.
    /// With `idempotency_key`, the commit applies at most once (a retry
    /// returns the original result).
    #[pyo3(signature = (*, idempotency_key = None))]
    fn transaction(slf: Py<Self>, idempotency_key: Option<String>) -> PyResult<PyTransaction> {
        PyTransaction::new(slf, NAMESPACE.to_owned(), idempotency_key)
    }

    /// The node `id` of `"default"` as a dict, or `None`; with `min_seq`,
    /// after waiting (at most `timeout` seconds) until that commit is applied.
    #[pyo3(signature = (id, *, min_seq = None, timeout = None))]
    fn node(
        &self,
        py: Python<'_>,
        id: &str,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Option<Py<PyAny>>> {
        self.node_in(py, NAMESPACE, id, min_seq, timeout)
    }

    /// The edge `id` as a dict, or `None`; `min_seq` and `timeout` as for
    /// `node`.
    #[pyo3(signature = (id, *, min_seq = None, timeout = None))]
    fn edge(&self, py: Python<'_>, id: u64, min_seq: Option<u64>, timeout: Option<f64>) -> PyResult<Option<Py<PyAny>>> {
        self.edge_in(py, NAMESPACE, id, min_seq, timeout)
    }

    /// Wait until commit `seq` is applied (at most `timeout` seconds);
    /// returns the namespace's seq.
    #[pyo3(signature = (seq, *, timeout = None))]
    fn wait_for_seq(&self, py: Python<'_>, seq: u64, timeout: Option<f64>) -> PyResult<u64> {
        self.wait_in(py, NAMESPACE, seq, timeout)
    }

    /// The seq of the last commit (0: none).
    fn seq(&self, py: Python<'_>) -> PyResult<u64> {
        guard(|| self.with(py, |s| Ok(s.seq())))
    }

    /// The highest seq known to be durable; `None` under `fsync="off"`
    /// until an explicit `sync()`.
    fn synced_seq(&self, py: Python<'_>) -> PyResult<Option<u64>> {
        guard(|| self.with(py, |s| Ok(s.status().synced_seq)))
    }

    /// Why `"default"` is read-only, or `None`.
    fn read_only(&self, py: Python<'_>) -> PyResult<Option<String>> {
        guard(|| self.with(py, |s| Ok(s.read_only())))
    }

    /// The history id (32 hex digits).
    fn history(&self, py: Python<'_>) -> PyResult<String> {
        guard(|| self.with(py, |s| Ok(s.history().to_string())))
    }

    /// The store's state at a glance.
    fn status(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let status = self.with(py, |s| Ok(s.status()))?;
            reports::store_status(py, &status)
        })
    }

    /// The catalog of `"default"`: indexes and constraints; `min_seq` and
    /// `timeout` as for `node`.
    #[pyo3(signature = (*, min_seq = None, timeout = None))]
    fn catalog(&self, py: Python<'_>, min_seq: Option<u64>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        self.catalog_in(py, NAMESPACE, min_seq, timeout)
    }

    /// Every index of `"default"` with its state.
    fn indexes(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.indexes_in(py, NAMESPACE)
    }

    /// Declare an index on `path` (a commit of its own).
    #[pyo3(signature = (path, *, idempotency_key = None))]
    fn create_index(
        &self,
        py: Python<'_>,
        path: &Bound<'_, PyAny>,
        idempotency_key: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::CreateIndex(IndexDef { path: attr_path(path)? });
        self.commit_catalog(py, NAMESPACE, change, idempotency_key)
    }

    /// Drop the index on `path` (a commit of its own).
    #[pyo3(signature = (path, *, idempotency_key = None))]
    fn drop_index(
        &self,
        py: Python<'_>,
        path: &Bound<'_, PyAny>,
        idempotency_key: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::DropIndex(IndexDef { path: attr_path(path)? });
        self.commit_catalog(py, NAMESPACE, change, idempotency_key)
    }

    /// Add a `"unique"` or `"required"` constraint on the nodes with
    /// `label` at `path` (a commit of its own).
    #[pyo3(signature = (kind, label, path, *, idempotency_key = None))]
    fn add_constraint(
        &self,
        py: Python<'_>,
        kind: &str,
        label: &str,
        path: &Bound<'_, PyAny>,
        idempotency_key: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::AddConstraint(constraint(kind, label, path)?);
        self.commit_catalog(py, NAMESPACE, change, idempotency_key)
    }

    /// Drop a constraint (a commit of its own).
    #[pyo3(signature = (kind, label, path, *, idempotency_key = None))]
    fn drop_constraint(
        &self,
        py: Python<'_>,
        kind: &str,
        label: &str,
        path: &Bound<'_, PyAny>,
        idempotency_key: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::DropConstraint(constraint(kind, label, path)?);
        self.commit_catalog(py, NAMESPACE, change, idempotency_key)
    }

    /// Fsync every commit of `"default"` so far.
    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        guard(|| self.with(py, Store::sync))
    }

    /// Checkpoint every commit of `"default"` so far and cut its WAL.
    fn checkpoint(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.checkpoint_in(py, NAMESPACE)
    }

    /// Checkpoint every namespace: `{name: outcome}`.
    fn checkpoint_all(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let outcomes = self.with(py, Store::checkpoint_all)?;
            let dict = PyDict::new(py);
            for (name, outcome) in outcomes {
                dict.set_item(name, reports::checkpoint(py, &outcome)?)?;
            }
            Ok(dict.into_any().unbind())
        })
    }

    /// An online backup of every namespace into `dest`, a new or empty
    /// directory.
    fn backup(&self, py: Python<'_>, dest: PathBuf) -> PyResult<Py<PyAny>> {
        guard(|| {
            let report = self.with(py, move |s| s.backup(&dest))?;
            reports::backup(py, &report)
        })
    }
}

impl PyStore {
    /// Run `f` on the namespace `name` with the GIL released.
    pub(crate) fn with_ns<R: Send>(
        &self,
        py: Python<'_>,
        name: &str,
        f: impl FnOnce(&iwdb::Ns<'_, iwdb::StdFs>) -> Result<R, Error> + Send,
    ) -> PyResult<R> {
        let name = name.to_owned();
        self.with(py, move |s| f(&s.namespace(&name)?))
    }

    pub(crate) fn node_in(
        &self,
        py: Python<'_>,
        ns: &str,
        id: &str,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Option<Py<PyAny>>> {
        guard(|| {
            let (ns, id) = (ns.to_owned(), id.to_owned());
            let options = read_options(min_seq, timeout)?;
            let mut nodes = self.query(py, move |db| block_on(db.get_nodes(&ns, vec![id], options)))?.value;
            let Some(node) = nodes.pop().flatten() else { return Ok(None) };
            let dict = PyDict::new(py);
            dict.set_item("id", &node.id)?;
            dict.set_item("labels", PyList::new(py, &node.labels)?)?;
            dict.set_item("attr", from_attrs(py, &node.attr)?)?;
            dict.set_item("meta", from_attrs(py, &node.meta)?)?;
            dict.set_item("version", node.version)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }

    pub(crate) fn edge_in(
        &self,
        py: Python<'_>,
        ns: &str,
        id: u64,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Option<Py<PyAny>>> {
        guard(|| {
            let ns = ns.to_owned();
            let options = read_options(min_seq, timeout)?;
            let mut edges = self.query(py, move |db| block_on(db.get_edges(&ns, vec![EdgeId(id)], options)))?.value;
            let Some(edge) = edges.pop().flatten() else { return Ok(None) };
            let dict = PyDict::new(py);
            dict.set_item("id", edge.id.0)?;
            dict.set_item("from", &edge.from)?;
            dict.set_item("to", &edge.to)?;
            dict.set_item("type", &edge.ty)?;
            dict.set_item("attr", from_attrs(py, &edge.attr)?)?;
            dict.set_item("meta", from_attrs(py, &edge.meta)?)?;
            dict.set_item("version", edge.version)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }

    pub(crate) fn wait_in(&self, py: Python<'_>, ns: &str, seq: u64, timeout: Option<f64>) -> PyResult<u64> {
        guard(|| {
            let ns = ns.to_owned();
            let options = read_options(None, timeout)?;
            self.query(py, move |db| block_on(db.wait_for_seq(&ns, seq, options)))
        })
    }

    pub(crate) fn catalog_in(
        &self,
        py: Python<'_>,
        ns: &str,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let options = read_options(min_seq, timeout)?;
            let catalog = self.query(py, move |db| block_on(db.catalog(&ns, options)))?.value;
            let indexes = PyList::empty(py);
            for index in catalog.indexes() {
                indexes.append(PyList::new(py, index.path.keys())?)?;
            }
            let constraints = PyList::empty(py);
            for c in catalog.constraints() {
                let dict = PyDict::new(py);
                let kind = match c.kind {
                    ConstraintKind::Unique => "unique",
                    ConstraintKind::Required => "required",
                };
                dict.set_item("kind", kind)?;
                dict.set_item("label", c.label.as_str())?;
                dict.set_item("path", PyList::new(py, c.path.keys())?)?;
                constraints.append(dict)?;
            }
            let dict = PyDict::new(py);
            dict.set_item("indexes", indexes)?;
            dict.set_item("constraints", constraints)?;
            Ok(dict.into_any().unbind())
        })
    }

    pub(crate) fn indexes_in(&self, py: Python<'_>, ns: &str) -> PyResult<Py<PyAny>> {
        guard(|| {
            let status = self.status_of(py, ns)?;
            reports::indexes(py, &status.indexes)
        })
    }

    pub(crate) fn checkpoint_in(&self, py: Python<'_>, ns: &str) -> PyResult<Py<PyAny>> {
        guard(|| {
            let outcome = self.with_ns(py, ns, |n| n.checkpoint())?;
            reports::checkpoint(py, &outcome)
        })
    }

    pub(crate) fn commit_catalog(
        &self,
        py: Python<'_>,
        ns: &str,
        change: CatalogChange,
        key: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let options = commit_options(key)?;
            let result = self.query(py, move |db| block_on(db.commit_catalog(&ns, change, options)))?;
            commit_result(py, &result)
        })
    }

    pub(crate) fn status_of(&self, py: Python<'_>, ns: &str) -> PyResult<iwdb::NamespaceStatus> {
        let ns = ns.to_owned();
        self.query(py, move |db| block_on(db.namespace_status(&ns)))
    }
}
