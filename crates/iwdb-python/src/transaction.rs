//! `iwdb.Transaction`: mutations collected in Python, committed as one.

use iwdb::{CommitOptions, EdgeId, EdgeKey, Mutation, Target};
use iwdb_query::exec::block_on;
use iwdb_query::Database;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyString;

use crate::convert::{to_attrs, to_value};
use crate::errors::{guard, invalid, value_error};
use crate::store::{commit_options, commit_result, PyStore};

/// A transaction: mutations collected in Python, committed as one
/// `Store::commit`.
#[pyclass(module = "iwdb", name = "Transaction")]
pub struct PyTransaction {
    store: Py<PyStore>,
    /// The namespace it commits to.
    namespace: String,
    mutations: Vec<Mutation>,
    /// Mutations that produce an edge id so far.
    edges: usize,
    result: Option<Py<PyAny>>,
    done: bool,
    /// The idempotency key, if any.
    options: CommitOptions,
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
    pub(crate) fn new(store: Py<PyStore>, namespace: String, idempotency_key: Option<String>) -> PyResult<Self> {
        let options = commit_options(idempotency_key)?;
        Ok(PyTransaction { store, namespace, mutations: Vec::new(), edges: 0, result: None, done: false, options })
    }

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
        let options = self.options.clone();
        let namespace = self.namespace.clone();
        let result = guard(|| store.query(py, move |db| block_on(db.commit(&namespace, mutations, options))))?;
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
