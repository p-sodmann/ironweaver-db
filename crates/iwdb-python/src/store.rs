//! `iwdb.Store` and `iwdb.Transaction`: translations to `iwdb::Store`
//! (design rule 8; `documentation/python-api.md`).

use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use iwdb::{
    AttrPath, CatalogChange, CheckpointOptions, CommitResult, Constraint, ConstraintKind, EdgeId, EdgeKey, Error,
    FsyncPolicy, IndexDef, Label, Mutation, Store, StoreOptions, Target, WalOptions,
};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};

use crate::convert::{from_attrs, to_attrs, to_value};
use crate::errors::{closed, guard, invalid, to_py, value_error};
use crate::reports;

/// A store, shared between Python threads. Calls hold the read side of the
/// lock (with the GIL released) while they run; `close` takes the write
/// side, so it waits for calls in progress, and leaves `None`.
#[pyclass(module = "iwdb", name = "Store", frozen)]
pub struct PyStore {
    inner: RwLock<Option<Store>>,
    path: PathBuf,
}

impl PyStore {
    /// Run `f` on the open store with the GIL released.
    pub fn with<R: Send>(&self, py: Python<'_>, f: impl FnOnce(&Store) -> Result<R, Error> + Send) -> PyResult<R> {
        let result = py.detach(|| {
            let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            inner.as_ref().map(f)
        });
        match result {
            Some(result) => result.map_err(to_py),
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

fn seconds(s: f64, what: &str) -> PyResult<Duration> {
    Duration::try_from_secs_f64(s).map_err(|_| value_error(format!("{} must be a number of seconds, at least 0", what)))
}

/// An attribute path: a `str` (one key) or a list of `str`.
fn attr_path(path: &Bound<'_, PyAny>) -> PyResult<AttrPath> {
    let keys: Vec<String> = if path.is_instance_of::<PyString>() {
        vec![path.extract()?]
    } else {
        path.extract().map_err(|_| PyTypeError::new_err("a path is a str or a list of str"))?
    };
    AttrPath::new(keys).map_err(|e| invalid(e.to_string()))
}

fn constraint(kind: &str, label: &str, path: &Bound<'_, PyAny>) -> PyResult<Constraint> {
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
            Ok(PyStore { inner: RwLock::new(Some(store)), path })
        })
    }

    /// Stop the background threads, fsync the WAL, checkpoint (if
    /// `checkpoint_on_close`) and release the lock. Waits for calls in
    /// progress on other threads. Closing a closed store does nothing.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        guard(|| {
            let result = py.detach(|| {
                let store = self.inner.write().unwrap_or_else(PoisonError::into_inner).take();
                store.map(Store::close)
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

    /// A transaction: mutations committed as one, when its `with` block
    /// ends or by `commit()`.
    fn transaction(slf: Py<Self>) -> PyTransaction {
        PyTransaction { store: slf, mutations: Vec::new(), edges: 0, result: None, done: false }
    }

    /// The node `id` as a dict, or `None`.
    fn node(&self, py: Python<'_>, id: &str) -> PyResult<Option<Py<PyAny>>> {
        guard(|| {
            let id = id.to_owned();
            let Some(node) = self.with(py, move |s| Ok(s.node(&id)))? else { return Ok(None) };
            let dict = PyDict::new(py);
            dict.set_item("id", &node.id)?;
            dict.set_item("labels", PyList::new(py, &node.labels)?)?;
            dict.set_item("attr", from_attrs(py, &node.attr)?)?;
            dict.set_item("meta", from_attrs(py, &node.meta)?)?;
            dict.set_item("version", node.version)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }

    /// The edge `id` as a dict, or `None`.
    fn edge(&self, py: Python<'_>, id: u64) -> PyResult<Option<Py<PyAny>>> {
        guard(|| {
            let Some(edge) = self.with(py, move |s| Ok(s.edge(EdgeId(id))))? else { return Ok(None) };
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

    /// The seq of the last commit (0: none).
    fn seq(&self, py: Python<'_>) -> PyResult<u64> {
        guard(|| self.with(py, |s| Ok(s.seq())))
    }

    /// The highest seq known to be durable; `None` under `fsync="off"`
    /// until an explicit `sync()`.
    fn synced_seq(&self, py: Python<'_>) -> PyResult<Option<u64>> {
        guard(|| self.with(py, |s| Ok(s.status().synced_seq)))
    }

    /// Why the store is read-only, or `None`.
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

    /// The catalog: indexes and constraints.
    fn catalog(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let catalog = self.with(py, |s| Ok(s.catalog()))?;
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

    /// Declare an index on `path` (a commit of its own).
    fn create_index(&self, py: Python<'_>, path: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::CreateIndex(IndexDef { path: attr_path(path)? });
        self.commit_catalog(py, change)
    }

    /// Drop the index on `path` (a commit of its own).
    fn drop_index(&self, py: Python<'_>, path: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::DropIndex(IndexDef { path: attr_path(path)? });
        self.commit_catalog(py, change)
    }

    /// Add a `"unique"` or `"required"` constraint on the nodes with
    /// `label` at `path` (a commit of its own).
    fn add_constraint(&self, py: Python<'_>, kind: &str, label: &str, path: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::AddConstraint(constraint(kind, label, path)?);
        self.commit_catalog(py, change)
    }

    /// Drop a constraint (a commit of its own).
    fn drop_constraint(&self, py: Python<'_>, kind: &str, label: &str, path: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::DropConstraint(constraint(kind, label, path)?);
        self.commit_catalog(py, change)
    }

    /// Fsync every commit so far.
    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        guard(|| self.with(py, Store::sync))
    }

    /// Checkpoint every commit so far and cut the WAL.
    fn checkpoint(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let outcome = self.with(py, Store::checkpoint)?;
            reports::checkpoint(py, &outcome)
        })
    }

    /// An online backup into `dest`, a new or empty directory.
    fn backup(&self, py: Python<'_>, dest: PathBuf) -> PyResult<Py<PyAny>> {
        guard(|| {
            let report = self.with(py, move |s| s.backup(&dest))?;
            reports::backup(py, &report)
        })
    }
}

impl PyStore {
    fn commit_catalog(&self, py: Python<'_>, change: CatalogChange) -> PyResult<Py<PyAny>> {
        guard(|| {
            let result = self.with(py, move |s| s.commit_catalog(change))?;
            commit_result(py, &result)
        })
    }
}

/// A transaction: mutations collected in Python, committed as one
/// `Store::commit`.
#[pyclass(module = "iwdb", name = "Transaction")]
pub struct PyTransaction {
    store: Py<PyStore>,
    mutations: Vec<Mutation>,
    /// Mutations that produce an edge id so far.
    edges: usize,
    result: Option<Py<PyAny>>,
    done: bool,
}

/// A node (`str`) or edge (`int`) id.
fn target(target: &Bound<'_, PyAny>) -> PyResult<Target> {
    if target.is_instance_of::<PyString>() {
        Ok(Target::Node(target.extract()?))
    } else if let Ok(id) = target.extract::<u64>() {
        Ok(Target::Edge(EdgeId(id)))
    } else {
        Err(PyTypeError::new_err("a target is a node id (str) or an edge id (int)"))
    }
}

impl PyTransaction {
    fn push(&mut self, mutation: Mutation) -> PyResult<()> {
        if self.done {
            return Err(invalid("the transaction is committed (or failed); start a new one"));
        }
        self.mutations.push(mutation);
        Ok(())
    }

    /// Push a mutation that produces an edge id; returns its position.
    fn push_edge(&mut self, mutation: Mutation) -> PyResult<usize> {
        self.push(mutation)?;
        self.edges += 1;
        Ok(self.edges - 1)
    }
}

#[pymethods]
impl PyTransaction {
    #[pyo3(signature = (id, *, labels = Vec::new(), attr = None, meta = None, expected_version = None))]
    fn upsert_node(
        &mut self,
        id: String,
        labels: Vec<String>,
        attr: Option<&Bound<'_, PyAny>>,
        meta: Option<&Bound<'_, PyAny>>,
        expected_version: Option<u64>,
    ) -> PyResult<()> {
        let (attr, meta) = (to_attrs(attr)?, to_attrs(meta)?);
        self.push(Mutation::UpsertNode { id, labels, attr, meta, expected_version })
    }

    #[pyo3(signature = (id, *, expected_version = None))]
    fn delete_node(&mut self, id: String, expected_version: Option<u64>) -> PyResult<()> {
        self.push(Mutation::DeleteNode { id, expected_version })
    }

    /// Add an edge; returns the position of its id in `result["edge_ids"]`.
    #[pyo3(signature = (from_, to, *, r#type = None, attr = None, meta = None))]
    fn add_edge(
        &mut self,
        from_: String,
        to: String,
        r#type: Option<String>,
        attr: Option<&Bound<'_, PyAny>>,
        meta: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<usize> {
        let (attr, meta) = (to_attrs(attr)?, to_attrs(meta)?);
        self.push_edge(Mutation::AddEdge { from: from_, to, ty: r#type, attr, meta })
    }

    /// Update edge `id`, or the one edge `from_ -> to` of `type`; returns
    /// the position of its id in `result["edge_ids"]`.
    #[pyo3(signature = (*, id = None, from_ = None, to = None, r#type = None, attr = None, meta = None, expected_version = None))]
    #[allow(clippy::too_many_arguments)]
    fn upsert_edge(
        &mut self,
        id: Option<u64>,
        from_: Option<String>,
        to: Option<String>,
        r#type: Option<String>,
        attr: Option<&Bound<'_, PyAny>>,
        meta: Option<&Bound<'_, PyAny>>,
        expected_version: Option<u64>,
    ) -> PyResult<usize> {
        let key = match (id, from_, to) {
            (Some(id), None, None) if r#type.is_none() => EdgeKey::Id(EdgeId(id)),
            (None, Some(from), Some(to)) => EdgeKey::Endpoints { from, to, ty: r#type },
            _ => return Err(value_error("upsert_edge takes id=..., or from_=... and to=... (and type=...)")),
        };
        let (attr, meta) = (to_attrs(attr)?, to_attrs(meta)?);
        self.push_edge(Mutation::UpsertEdge { key, attr, meta, expected_version })
    }

    #[pyo3(signature = (id, *, expected_version = None))]
    fn delete_edge(&mut self, id: u64, expected_version: Option<u64>) -> PyResult<()> {
        self.push(Mutation::DeleteEdge { id: EdgeId(id), expected_version })
    }

    #[pyo3(signature = (target, key, value, *, expected_version = None))]
    fn set_attr(
        &mut self,
        target: &Bound<'_, PyAny>,
        key: String,
        value: &Bound<'_, PyAny>,
        expected_version: Option<u64>,
    ) -> PyResult<()> {
        let (target, value) = (self::target(target)?, to_value(value, 1)?);
        self.push(Mutation::SetAttr { target, key, value, expected_version })
    }

    #[pyo3(signature = (target, key, *, expected_version = None))]
    fn remove_attr(&mut self, target: &Bound<'_, PyAny>, key: String, expected_version: Option<u64>) -> PyResult<()> {
        let target = self::target(target)?;
        self.push(Mutation::RemoveAttr { target, key, expected_version })
    }

    #[pyo3(signature = (target, key, value, *, expected_version = None))]
    fn append_attr(
        &mut self,
        target: &Bound<'_, PyAny>,
        key: String,
        value: &Bound<'_, PyAny>,
        expected_version: Option<u64>,
    ) -> PyResult<()> {
        // The appended value sits one level down, in the list
        let (target, value) = (self::target(target)?, to_value(value, 2)?);
        self.push(Mutation::AppendAttr { target, key, value, expected_version })
    }

    #[pyo3(signature = (id, label, *, expected_version = None))]
    fn add_label(&mut self, id: String, label: String, expected_version: Option<u64>) -> PyResult<()> {
        self.push(Mutation::AddLabel { id, label, expected_version })
    }

    #[pyo3(signature = (id, label, *, expected_version = None))]
    fn remove_label(&mut self, id: String, label: String, expected_version: Option<u64>) -> PyResult<()> {
        self.push(Mutation::RemoveLabel { id, label, expected_version })
    }

    #[pyo3(signature = (id, r#type, *, expected_version = None))]
    fn set_edge_type(&mut self, id: u64, r#type: Option<String>, expected_version: Option<u64>) -> PyResult<()> {
        self.push(Mutation::SetEdgeType { id: EdgeId(id), ty: r#type, expected_version })
    }

    /// Commit the mutations as one transaction, once; returns the result.
    /// After a failure the transaction is finished too: start a new one.
    fn commit(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.done {
            return Err(invalid("the transaction is committed (or failed); start a new one"));
        }
        self.done = true;
        let mutations = std::mem::take(&mut self.mutations);
        let store = self.store.get();
        let result = guard(|| store.with(py, move |s| s.commit(&mutations)))?;
        let result = commit_result(py, &result)?;
        self.result = Some(result.clone_ref(py));
        Ok(result)
    }

    /// The commit's result, or `None` before it (and for an empty block).
    #[getter]
    fn result(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.result.as_ref().map(|r| r.clone_ref(py))
    }

    fn __len__(&self) -> usize {
        self.mutations.len()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Commits if the block ended without an exception (and there is
    /// something to commit); on an exception, commits nothing.
    #[pyo3(signature = (exc_type = None, _exc = None, _traceback = None))]
    fn __exit__(
        &mut self,
        py: Python<'_>,
        exc_type: Option<Bound<'_, PyAny>>,
        _exc: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let failed = exc_type.is_some_and(|t| !t.is_none());
        if failed || self.done || self.mutations.is_empty() {
            self.done = true;
            self.mutations.clear();
            return Ok(false);
        }
        self.commit(py)?;
        Ok(false)
    }
}
