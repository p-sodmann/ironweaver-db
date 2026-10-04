//! `iwdb.Store`: translations to the `Database` trait, served by
//! `iwdb::Embedded` or a server ([`Backend`], ADR 0035; design rule 8), and
//! to `iwdb::Store` for what the trait doesn't cover (backups, checkpoints,
//! syncs, the store's status), which only an embedded store has. See
//! `documentation/python-api.md`.

use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use iwdb::import::{ExportFormat, ImportFormat};
use iwdb::{
    AttrPath, CatalogChange, CheckpointOptions, CommitOptions, CommitResult, Constraint, ConstraintKind, EdgeId,
    Embedded, Error, FsyncPolicy, HistoryId, IdempotencyKey, IndexDef, Label, NAMESPACE, NamespaceResult, QueryConfig,
    Store, StoreOptions, Target, WalOptions, WalRetention,
};
use iwdb_query::exec::block_on;
use iwdb_query::{ChangesRequest, Database, LimitConfig, QueryOptions, Secret};
use iwdb_server::client::{ClientTls, Remote};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};

use crate::backend::{Backend, RemoteDb};
use crate::errors::{closed, guard, invalid, query_to_py, to_py, value_error};
use crate::namespace::PyNamespace;
use crate::query::ReadArgs;
use crate::reports;
use crate::transaction::PyTransaction;

/// A store, shared between Python threads, served as a `Database` by
/// `iwdb::Embedded` (`Store.open`) or a server (`iwdb.connect`). Calls
/// hold the read side of the lock (with the GIL released) while they run;
/// `close` takes the write side, so it waits for calls in progress, and
/// leaves `None`.
#[pyclass(module = "iwdb", name = "Store", frozen)]
pub struct PyStore {
    inner: RwLock<Option<Backend>>,
    /// The directory, or the server's endpoint.
    location: String,
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
    /// `Database` trait doesn't cover). A remote store raises
    /// `InvalidError`: these calls need the store's directory.
    pub fn with<R: Send>(&self, py: Python<'_>, f: impl FnOnce(&Store) -> Result<R, Error> + Send) -> PyResult<R> {
        let result = py.detach(|| {
            let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            inner.as_ref().map(|backend| match backend {
                Backend::Embedded(db) => Some(f(db.store())),
                Backend::Remote(_) => None,
            })
        });
        match result {
            Some(Some(result)) => result.map_err(to_py),
            Some(None) => Err(invalid(
                "this call needs an embedded store (iwdb.Store.open); a store from iwdb.connect doesn't have it",
            )),
            None => Err(closed()),
        }
    }

    /// Run `f` on the `Database` with the GIL released.
    pub fn query<R: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&Backend) -> Result<R, iwdb_query::Error> + Send,
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
        checkpoint_on_close = true, checkpoint_keep = 2, checkpoint_background = true, archive = None,
        retain_records = 0, retain_age = None
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
        retain_records: u64,
        retain_age: Option<f64>,
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
                retention: WalRetention {
                    records: retain_records,
                    age: retain_age.map(|s| seconds(s, "retain_age")).transpose()?,
                },
            };
            let store = py.detach(|| Store::open(&path, options)).map_err(to_py)?;
            let db = Embedded::new(store, query_config()).map_err(query_to_py)?;
            Ok(PyStore { inner: RwLock::new(Some(Backend::Embedded(db))), location: path.display().to_string() })
        })
    }

    /// A client of the server at `endpoint` (`https://host:port`, or
    /// `http://host:port` without TLS): a store with the same API, minus
    /// the calls that need the store's directory (ADR 0035). It connects on
    /// the first call, and again after a lost connection.
    ///
    /// TLS (step 15b): `ca` (a PEM file) is what the server's certificate
    /// is verified against (default: the system's trust store); `cert` and
    /// `key` (PEM files) are a client certificate, which authenticates as
    /// the user it names when the server verifies client certificates.
    ///
    /// Credentials (step 15a): a `token` (a session's or an API token), or a
    /// `user` and `password`, which log in now and keep the session's token.
    /// Errors: `UnauthenticatedError` for a wrong user or password;
    /// `InvalidError` for both kinds of credentials, a user without a
    /// password, TLS files that can't be read or used, or TLS files with an
    /// `http://` endpoint.
    #[staticmethod]
    #[pyo3(signature = (endpoint, token = None, user = None, password = None, ca = None, cert = None, key = None))]
    #[allow(clippy::too_many_arguments)]
    fn connect(
        py: Python<'_>,
        endpoint: &str,
        token: Option<String>,
        user: Option<String>,
        password: Option<String>,
        ca: Option<PathBuf>,
        cert: Option<PathBuf>,
        key: Option<PathBuf>,
    ) -> PyResult<Self> {
        guard(|| {
            let tls = ClientTls { ca, cert, key };
            let remote = py.detach(|| Remote::connect_tls(endpoint, &tls)).map_err(query_to_py)?;
            match (token, user, password) {
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(invalid("give a token, or a user and a password, not both"));
                }
                (Some(token), None, None) => remote.set_token(Some(Secret::new(token))),
                (None, Some(user), Some(password)) => {
                    py.detach(|| block_on(remote.login(&user, Secret::new(password)))).map_err(query_to_py)?;
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(invalid("a login needs both a user and a password"));
                }
                (None, None, None) => {}
            }
            let backend = Backend::Remote(Box::new(RemoteDb::new(remote)));
            Ok(PyStore { inner: RwLock::new(Some(backend)), location: endpoint.to_owned() })
        })
    }

    /// Stop the background threads, fsync the WALs, checkpoint (if
    /// `checkpoint_on_close`) and release the lock. Waits for calls in
    /// progress on other threads. Closing a closed store does nothing.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        guard(|| {
            let result = py.detach(|| {
                let backend = self.inner.write().unwrap_or_else(PoisonError::into_inner).take();
                match backend {
                    Some(Backend::Embedded(db)) => db.close(),
                    Some(Backend::Remote(db)) => {
                        // Drops the connection and the client's runtime
                        drop(db);
                        Ok(())
                    }
                    None => Ok(()),
                }
            });
            result.map_err(to_py)
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
        format!("<iwdb.Store '{}' ({})>", self.location, state)
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
            let id = slf.get().status_of(py, name)?.id;
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
        guard(|| Ok(self.status_of(py, NAMESPACE)?.seq))
    }

    /// The highest seq known to be durable; `None` under `fsync="off"`
    /// until an explicit `sync()`.
    fn synced_seq(&self, py: Python<'_>) -> PyResult<Option<u64>> {
        guard(|| Ok(self.status_of(py, NAMESPACE)?.synced_seq))
    }

    /// Why `"default"` is read-only, or `None`.
    fn read_only(&self, py: Python<'_>) -> PyResult<Option<String>> {
        guard(|| Ok(self.status_of(py, NAMESPACE)?.read_only))
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

    /// A batch of the change stream of `"default"` (ADR 0031): the commits
    /// from `from_seq` on, as logged, only durable ones. With `wait`, waits
    /// for one for about `timeout` if there is none yet. Resume with the
    /// batch's `next_seq`; `history` is the store's history id of that seq
    /// (a restored store refuses it).
    #[pyo3(signature = (from_seq = 0, *, wait = false, max_results = None, history = None, timeout = None))]
    fn changes(
        &self,
        py: Python<'_>,
        from_seq: u64,
        wait: bool,
        max_results: Option<usize>,
        history: Option<&str>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.changes_in(py, NAMESPACE, from_seq, wait, max_results, history, timeout)
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

    /// Create the namespace `name` from the graph file `path`: a core JSON
    /// or binary file, or LGF (`format` "json", "binary" or "lgf"; by
    /// default detected from the file's first bytes). The namespace is
    /// made from one checkpoint at seq 1, all or nothing, and the checkpoint
    /// is archived if the store has a WAL archive (ADR 0033).
    #[pyo3(signature = (name, path, *, format = None))]
    fn import_namespace(&self, py: Python<'_>, name: &str, path: PathBuf, format: Option<&str>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let format = format.map(str::parse::<ImportFormat>).transpose().map_err(invalid)?;
            let name = name.to_owned();
            let report = self.with(py, move |s| s.import_file(&name, &path, format, None))?;
            reports::import(py, &report)
        })
    }

    /// Merge the graph file `path` into `"default"` through commits: its
    /// nodes are upserted, its edges upserted by their ends and type, in
    /// batches (see `Namespace.import_file`).
    #[pyo3(signature = (path, *, format = None))]
    fn import_file(&self, py: Python<'_>, path: PathBuf, format: Option<&str>) -> PyResult<Py<PyAny>> {
        self.merge_in(py, NAMESPACE, path, format)
    }

    /// Write `"default"`'s graph to `path` as a core file (`format` "json"
    /// or "binary"; by default JSON for a `.json` path, binary otherwise),
    /// atomically. Commits wait while it writes.
    #[pyo3(signature = (path, *, format = None))]
    fn export(&self, py: Python<'_>, path: PathBuf, format: Option<&str>) -> PyResult<Py<PyAny>> {
        self.export_in(py, NAMESPACE, path, format)
    }

    // ---- queries (`crate::query`) ----

    /// Nodes matching `filter` (an `iwdb` filter), by index or scan, sorted by id; paginated (`cursor`).
    #[pyo3(signature = (filter, *, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn find(
        &self,
        py: Python<'_>,
        filter: &Bound<'_, PyAny>,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.find_in(
            py,
            NAMESPACE,
            filter,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    /// How `find` would read `filter`: the index plan and the estimated candidates (with `analyze`, the exact number).
    #[pyo3(signature = (filter, *, analyze = false, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn explain(
        &self,
        py: Python<'_>,
        filter: &Bound<'_, PyAny>,
        analyze: bool,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.explain_in(py, NAMESPACE, filter, analyze, min_seq, timeout)
    }

    /// The nodes within `depth` edges of `seeds` (an id or a list of ids), sorted by id; paginated (`cursor`).
    #[pyo3(signature = (seeds, *, depth = 1, direction = "out", edge_types = None, edge_filter = None, node_filter = None, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn neighbourhood(
        &self,
        py: Python<'_>,
        seeds: &Bound<'_, PyAny>,
        depth: usize,
        direction: &str,
        edge_types: Option<Vec<String>>,
        edge_filter: Option<&Bound<'_, PyAny>>,
        node_filter: Option<&Bound<'_, PyAny>>,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.neighbourhood_in(
            py,
            NAMESPACE,
            seeds,
            depth,
            direction,
            edge_types,
            edge_filter,
            node_filter,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    /// A breadth-first (`order="bfs"`) or depth-first (`"dfs"`) traversal from `start`: node ids in traversal order.
    #[pyo3(signature = (start, *, order = "bfs", depth = None, direction = "out", edge_types = None, edge_filter = None, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn traverse(
        &self,
        py: Python<'_>,
        start: &str,
        order: &str,
        depth: Option<usize>,
        direction: &str,
        edge_types: Option<Vec<String>>,
        edge_filter: Option<&Bound<'_, PyAny>>,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.traverse_in(
            py,
            NAMESPACE,
            start,
            order,
            depth,
            direction,
            edge_types,
            edge_filter,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    /// A shortest path from `from_` to `to`: `"bfs"` (fewest edges), `"dijkstra"` or `"astar"` (cheapest by the edge attribute `weight`).
    #[pyo3(signature = (from_, to, *, method = "bfs", weight = None, default_weight = 1.0, coords = None, metric = "euclidean", direction = "out", max_depth = None, max_cost = None, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn shortest_path(
        &self,
        py: Python<'_>,
        from_: &str,
        to: &str,
        method: &str,
        weight: Option<String>,
        default_weight: f64,
        coords: Option<&Bound<'_, PyAny>>,
        metric: &str,
        direction: &str,
        max_depth: Option<usize>,
        max_cost: Option<f64>,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.shortest_path_in(
            py,
            NAMESPACE,
            from_,
            to,
            method,
            weight,
            default_weight,
            coords,
            metric,
            direction,
            max_depth,
            max_cost,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    /// Up to `walks` random walks of at most `max_length` nodes from `start` (duplicates removed).
    #[pyo3(signature = (start, *, max_length, walks = 1, min_length = 1, allow_revisit = false, seed = None, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn random_walks(
        &self,
        py: Python<'_>,
        start: &str,
        max_length: usize,
        walks: usize,
        min_length: usize,
        allow_revisit: bool,
        seed: Option<u64>,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.random_walks_in(
            py,
            NAMESPACE,
            start,
            max_length,
            walks,
            min_length,
            allow_revisit,
            seed,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    /// The nodes within `depth` edges of `seeds` and the edges between them, both sorted by id.
    #[pyo3(signature = (seeds, *, depth = 1, direction = "out", edge_types = None, edge_filter = None, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn subgraph(
        &self,
        py: Python<'_>,
        seeds: &Bound<'_, PyAny>,
        depth: usize,
        direction: &str,
        edge_types: Option<Vec<String>>,
        edge_filter: Option<&Bound<'_, PyAny>>,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.subgraph_in(
            py,
            NAMESPACE,
            seeds,
            depth,
            direction,
            edge_types,
            edge_filter,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    /// Every match of `pattern` (the core's pattern text), with `where` filters per node variable; sorted rows, paginated (`cursor`).
    #[pyo3(signature = (pattern, *, r#where = None, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn r#match(
        &self,
        py: Python<'_>,
        pattern: &str,
        r#where: Option<&Bound<'_, PyDict>>,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.match_in(
            py,
            NAMESPACE,
            pattern,
            r#where,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    /// An analytics job (`"pagerank"`, `"degree"`, `"leiden"`, ...) with its `params`, on a projection of the namespace.
    #[pyo3(signature = (job, *, params = None, direction = "out", weight = None, default_weight = 1.0, max_results = None, max_visited = None, max_edges = None, partial = false, cursor = None, min_seq = None, timeout = None))]
    #[allow(clippy::too_many_arguments)]
    fn analyze(
        &self,
        py: Python<'_>,
        job: &str,
        params: Option<&Bound<'_, PyDict>>,
        direction: &str,
        weight: Option<String>,
        default_weight: f64,
        max_results: Option<usize>,
        max_visited: Option<usize>,
        max_edges: Option<usize>,
        partial: bool,
        cursor: Option<String>,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        self.analyze_in(
            py,
            NAMESPACE,
            job,
            params,
            direction,
            weight,
            default_weight,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
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
            Ok(Some(crate::query::node(py, &node)?))
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
            Ok(Some(crate::query::edge(py, &edge)?))
        })
    }

    pub(crate) fn wait_in(&self, py: Python<'_>, ns: &str, seq: u64, timeout: Option<f64>) -> PyResult<u64> {
        guard(|| {
            let ns = ns.to_owned();
            let options = read_options(None, timeout)?;
            self.query(py, move |db| block_on(db.wait_for_seq(&ns, seq, options)))
        })
    }

    /// A batch of the change stream (ADR 0031); see `crate::changes`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn changes_in(
        &self,
        py: Python<'_>,
        ns: &str,
        from_seq: u64,
        wait: bool,
        max_results: Option<usize>,
        history: Option<&str>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let mut options = read_options(None, timeout)?;
            options.limits.max_results = max_results;
            options.history = history.map(|h| h.parse::<HistoryId>().map_err(invalid)).transpose()?;
            let request = ChangesRequest { from_seq, wait };
            let answer = self.query(py, move |db| block_on(db.changes(&ns, request, options)))?;
            crate::changes::batch(py, &answer)
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

    pub(crate) fn merge_in(
        &self,
        py: Python<'_>,
        ns: &str,
        path: PathBuf,
        format: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let format = format.map(str::parse::<ImportFormat>).transpose().map_err(invalid)?;
            let report = self.with_ns(py, ns, move |n| n.import_file(&path, format, None))?;
            reports::merge(py, &report)
        })
    }

    pub(crate) fn export_in(
        &self,
        py: Python<'_>,
        ns: &str,
        path: PathBuf,
        format: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let format = format.map(str::parse::<ExportFormat>).transpose().map_err(invalid)?;
            let report = self.with_ns(py, ns, move |n| n.export_file(&path, format, None))?;
            reports::export(py, &report)
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
