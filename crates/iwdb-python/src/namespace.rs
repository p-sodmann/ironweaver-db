//! `iwdb.Namespace`: the store's graph methods for one namespace.

use iwdb::{CatalogChange, IndexDef};
use pyo3::prelude::*;

use crate::errors::guard;
use crate::reports;
use crate::store::{attr_path, constraint, PyStore};
use crate::transaction::PyTransaction;

/// A handle on one namespace of a store (`store.namespace(name)`): the
/// methods of the store's graph API, for this namespace. It looks the
/// namespace up by name on every call.
#[pyclass(module = "iwdb", name = "Namespace", frozen)]
pub struct PyNamespace {
    pub(crate) store: Py<PyStore>,
    pub(crate) name: String,
    pub(crate) id: u64,
}

#[pymethods]
impl PyNamespace {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// The namespace's id: never reused, so a namespace created again
    /// under the same name has another.
    #[getter]
    fn id(&self) -> u64 {
        self.id
    }

    fn __repr__(&self) -> String {
        format!("<iwdb.Namespace '{}' (id {})>", self.name, self.id)
    }

    #[pyo3(signature = (*, idempotency_key = None))]
    fn transaction(&self, py: Python<'_>, idempotency_key: Option<String>) -> PyResult<PyTransaction> {
        PyTransaction::new(self.store.clone_ref(py), self.name.clone(), idempotency_key)
    }

    #[pyo3(signature = (id, *, min_seq = None, timeout = None))]
    fn node(
        &self,
        py: Python<'_>,
        id: &str,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Option<Py<PyAny>>> {
        self.store.get().node_in(py, &self.name, id, min_seq, timeout)
    }

    #[pyo3(signature = (id, *, min_seq = None, timeout = None))]
    fn edge(&self, py: Python<'_>, id: u64, min_seq: Option<u64>, timeout: Option<f64>) -> PyResult<Option<Py<PyAny>>> {
        self.store.get().edge_in(py, &self.name, id, min_seq, timeout)
    }

    #[pyo3(signature = (seq, *, timeout = None))]
    fn wait_for_seq(&self, py: Python<'_>, seq: u64, timeout: Option<f64>) -> PyResult<u64> {
        self.store.get().wait_in(py, &self.name, seq, timeout)
    }

    fn seq(&self, py: Python<'_>) -> PyResult<u64> {
        guard(|| self.store.get().with_ns(py, &self.name, |n| Ok(n.seq())))
    }

    fn synced_seq(&self, py: Python<'_>) -> PyResult<Option<u64>> {
        guard(|| self.store.get().with_ns(py, &self.name, |n| Ok(n.status().synced_seq)))
    }

    fn read_only(&self, py: Python<'_>) -> PyResult<Option<String>> {
        guard(|| self.store.get().with_ns(py, &self.name, |n| Ok(n.read_only())))
    }

    /// The namespace's state: counts, indexes, memory.
    fn status(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let status = self.store.get().status_of(py, &self.name)?;
            reports::namespace_status(py, &status)
        })
    }

    #[pyo3(signature = (*, min_seq = None, timeout = None))]
    fn catalog(&self, py: Python<'_>, min_seq: Option<u64>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        self.store.get().catalog_in(py, &self.name, min_seq, timeout)
    }

    fn indexes(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.store.get().indexes_in(py, &self.name)
    }

    #[pyo3(signature = (path, *, idempotency_key = None))]
    fn create_index(
        &self,
        py: Python<'_>,
        path: &Bound<'_, PyAny>,
        idempotency_key: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::CreateIndex(IndexDef { path: attr_path(path)? });
        self.store.get().commit_catalog(py, &self.name, change, idempotency_key)
    }

    #[pyo3(signature = (path, *, idempotency_key = None))]
    fn drop_index(
        &self,
        py: Python<'_>,
        path: &Bound<'_, PyAny>,
        idempotency_key: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let change = CatalogChange::DropIndex(IndexDef { path: attr_path(path)? });
        self.store.get().commit_catalog(py, &self.name, change, idempotency_key)
    }

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
        self.store.get().commit_catalog(py, &self.name, change, idempotency_key)
    }

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
        self.store.get().commit_catalog(py, &self.name, change, idempotency_key)
    }

    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        guard(|| self.store.get().with_ns(py, &self.name, |n| n.sync()))
    }

    fn checkpoint(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.store.get().checkpoint_in(py, &self.name)
    }
}
