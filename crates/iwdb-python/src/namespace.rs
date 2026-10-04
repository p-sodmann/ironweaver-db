//! `iwdb.Namespace`: the store's graph methods for one namespace.

use iwdb::{CatalogChange, IndexDef};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::errors::guard;
use crate::query::ReadArgs;
use crate::reports;
use crate::store::{PyStore, attr_path, constraint};
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
        guard(|| Ok(self.store.get().status_of(py, &self.name)?.seq))
    }

    fn synced_seq(&self, py: Python<'_>) -> PyResult<Option<u64>> {
        guard(|| Ok(self.store.get().status_of(py, &self.name)?.synced_seq))
    }

    fn read_only(&self, py: Python<'_>) -> PyResult<Option<String>> {
        guard(|| Ok(self.store.get().status_of(py, &self.name)?.read_only))
    }

    /// The namespace's state: counts, indexes, memory.
    fn status(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        guard(|| {
            let status = self.store.get().status_of(py, &self.name)?;
            reports::namespace_status(py, &status)
        })
    }

    /// A batch of the change stream (ADR 0031); see `Store.changes`.
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
        self.store.get().changes_in(py, &self.name, from_seq, wait, max_results, history, timeout)
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
        self.store.get().find_in(
            py,
            &self.name,
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
        self.store.get().explain_in(py, &self.name, filter, analyze, min_seq, timeout)
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
        self.store.get().neighbourhood_in(
            py,
            &self.name,
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
        self.store.get().traverse_in(
            py,
            &self.name,
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
        self.store.get().shortest_path_in(
            py,
            &self.name,
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
        self.store.get().random_walks_in(
            py,
            &self.name,
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
        self.store.get().subgraph_in(
            py,
            &self.name,
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
        self.store.get().match_in(
            py,
            &self.name,
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
        self.store.get().analyze_in(
            py,
            &self.name,
            job,
            params,
            direction,
            weight,
            default_weight,
            ReadArgs { max_results, max_visited, max_edges, partial, cursor, min_seq, timeout },
        )
    }

    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        guard(|| self.store.get().with_ns(py, &self.name, |n| n.sync()))
    }

    fn checkpoint(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.store.get().checkpoint_in(py, &self.name)
    }

    /// Merge the graph file `path` (core JSON or binary, or LGF; `format`
    /// as for `Store.import_namespace`) into this namespace through
    /// commits: nodes upserted, edges upserted by their ends and type, in
    /// batches of up to 10 000 mutations. A failure leaves the batches
    /// before it committed; running it again converges.
    #[pyo3(signature = (path, *, format = None))]
    fn import_file(&self, py: Python<'_>, path: std::path::PathBuf, format: Option<&str>) -> PyResult<Py<PyAny>> {
        self.store.get().merge_in(py, &self.name, path, format)
    }

    /// Write the namespace's graph to `path` as a core file (see
    /// `Store.export`).
    #[pyo3(signature = (path, *, format = None))]
    fn export(&self, py: Python<'_>, path: std::path::PathBuf, format: Option<&str>) -> PyResult<Py<PyAny>> {
        self.store.get().export_in(py, &self.name, path, format)
    }
}
