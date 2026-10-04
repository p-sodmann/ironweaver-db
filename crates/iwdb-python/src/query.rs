//! The query methods (`find`, `explain`, `neighbourhood`, `traverse`,
//! `shortest_path`, `random_walks`, `subgraph`, `match`, `analyze`):
//! Python arguments to the `Database` trait's requests, answers to dicts
//! (`documentation/python-api.md`, "Queries"). `Store` and `Namespace` call
//! these with their namespace.
//!
//! Every answer is a dict with the results under an operation's key, and
//! `"seq"` (the state the read saw), `"cursor"` (the next page, or `None`),
//! `"truncated"` (a limit cut a `partial` answer) and `"work"`
//! (`{"visited", "edges"}`).

use ironweaver_core::Direction;
use ironweaver_core::algo::{Leiden, PageRank};
use ironweaver_core::pathfinding::{Coords, EdgeCost, Metric};
use ironweaver_core::query::Pattern;
use iwdb_query::exec::block_on;
use iwdb_query::read::{Explain, Lookup, Plan};
use iwdb_query::{
    AnalyticsRequest, Answer, Cursor, Database, Edge, ExplainRequest, FindRequest, Job, JobResult, Limits,
    MatchRequest, NeighbourhoodRequest, Node, Order, PathMethod, PathRequest, ProjectionSpec, SubgraphRequest,
    TraverseRequest, WalkRequest,
};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};

use crate::convert::from_attrs;
use crate::errors::{guard, invalid, value_error};
use crate::filter::{to_expr, to_expr_opt};
use crate::store::{PyStore, attr_path, read_options};

/// The options every bounded read takes.
pub struct ReadArgs {
    pub max_results: Option<usize>,
    pub max_visited: Option<usize>,
    pub max_edges: Option<usize>,
    pub partial: bool,
    pub cursor: Option<String>,
    pub min_seq: Option<u64>,
    pub timeout: Option<f64>,
}

impl ReadArgs {
    fn options(self) -> PyResult<iwdb_query::QueryOptions> {
        let mut options = read_options(self.min_seq, self.timeout)?;
        options.limits =
            Limits { max_results: self.max_results, max_visited: self.max_visited, max_edges: self.max_edges };
        options.partial = self.partial;
        options.cursor = self.cursor.map(Cursor::new);
        Ok(options)
    }
}

/// A node as a dict: `{"id", "labels", "attr", "meta", "version"}`.
pub fn node(py: Python<'_>, node: &Node) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item("id", &node.id)?;
    dict.set_item("labels", PyList::new(py, &node.labels)?)?;
    dict.set_item("attr", from_attrs(py, &node.attr)?)?;
    dict.set_item("meta", from_attrs(py, &node.meta)?)?;
    dict.set_item("version", node.version)?;
    Ok(dict.into_any().unbind())
}

/// An edge as a dict: `{"id", "from", "to", "type", "attr", "meta",
/// "version"}`.
pub fn edge(py: Python<'_>, edge: &Edge) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item("id", edge.id.0)?;
    dict.set_item("from", &edge.from)?;
    dict.set_item("to", &edge.to)?;
    dict.set_item("type", &edge.ty)?;
    dict.set_item("attr", from_attrs(py, &edge.attr)?)?;
    dict.set_item("meta", from_attrs(py, &edge.meta)?)?;
    dict.set_item("version", edge.version)?;
    Ok(dict.into_any().unbind())
}

fn nodes(py: Python<'_>, list: &[Node]) -> PyResult<Py<PyAny>> {
    let out = PyList::empty(py);
    for n in list {
        out.append(node(py, n)?)?;
    }
    Ok(out.into_any().unbind())
}

/// The answer dict: `fields` plus `"seq"`, `"cursor"`, `"truncated"` and
/// `"work"`.
fn answer_with<T>(py: Python<'_>, a: &Answer<T>, dict: Bound<'_, PyDict>) -> PyResult<Py<PyAny>> {
    let work = PyDict::new(py);
    work.set_item("visited", a.work.visited)?;
    work.set_item("edges", a.work.edges)?;
    dict.set_item("seq", a.seq)?;
    dict.set_item("cursor", a.next.as_ref().map(Cursor::as_str))?;
    dict.set_item("truncated", a.truncated)?;
    dict.set_item("work", work)?;
    Ok(dict.into_any().unbind())
}

/// The answer dict with the results under `key`.
fn answer<T>(py: Python<'_>, a: &Answer<T>, key: &str, value: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item(key, value)?;
    answer_with(py, a, dict)
}

fn direction(name: &str) -> PyResult<Direction> {
    Ok(match name {
        "out" => Direction::Out,
        "in" => Direction::In,
        "both" => Direction::Both,
        other => return Err(value_error(format!("direction must be 'out', 'in' or 'both', not '{}'", other))),
    })
}

/// Node ids: a `str` (one) or a list of `str`.
fn ids(seeds: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if seeds.is_instance_of::<PyString>() {
        Ok(vec![seeds.extract()?])
    } else {
        seeds.extract().map_err(|_| PyTypeError::new_err("node ids are a str or a list of str"))
    }
}

/// The cost of an edge: unweighted, or the edge attribute `weight` (and
/// `default_weight` for edges without it).
fn cost(weight: Option<String>, default_weight: f64) -> EdgeCost {
    match weight {
        None => EdgeCost::Unit,
        Some(key) => EdgeCost::Weighted { key, default: default_weight },
    }
}

/// A* coordinates: a path (`str`) to an attribute holding a sequence of
/// numbers, or a list of paths, one per dimension (`["x", "y"]`, the
/// default).
fn coords(coords: Option<&Bound<'_, PyAny>>) -> PyResult<Coords> {
    let Some(coords) = coords else { return Ok(Coords::default()) };
    if coords.is_instance_of::<PyString>() {
        return Ok(Coords::Sequence(vec![coords.extract()?]));
    }
    let dims = coords.try_iter().map_err(|_| PyTypeError::new_err("coords is a str or a list of paths"))?;
    Ok(Coords::PerDimension(dims.map(|p| Ok(attr_path(&p?)?.keys().to_vec())).collect::<PyResult<_>>()?))
}

fn plan(py: Python<'_>, p: &Plan) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    match p {
        Plan::Empty => dict.set_item("kind", "empty")?,
        Plan::Label { label } => {
            dict.set_item("kind", "label")?;
            dict.set_item("label", label)?;
        }
        Plan::Index { path, lookup } => {
            dict.set_item("kind", "index")?;
            dict.set_item("path", path)?;
            match lookup {
                Lookup::Point => dict.set_item("lookup", "point")?,
                Lookup::In { values } => {
                    dict.set_item("lookup", "in")?;
                    dict.set_item("values", values)?;
                }
                Lookup::Range => dict.set_item("lookup", "range")?,
            }
        }
        Plan::Union(plans) => {
            dict.set_item("kind", "union")?;
            let list = PyList::empty(py);
            for p in plans {
                list.append(plan(py, p)?)?;
            }
            dict.set_item("plans", list)?;
        }
        Plan::Scan => dict.set_item("kind", "scan")?,
        Plan::Other(text) => {
            dict.set_item("kind", "other")?;
            dict.set_item("text", text)?;
        }
    }
    Ok(dict.into_any().unbind())
}

fn explanation<'py>(py: Python<'py>, e: &Explain) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("plan", plan(py, &e.plan)?)?;
    dict.set_item("estimated_candidates", e.estimated_candidates)?;
    dict.set_item("candidates", e.candidates)?;
    dict.set_item("nodes", e.nodes)?;
    dict.set_item("building", &e.building)?;
    Ok(dict)
}

/// Read a job's parameter `name` from `params`, removing it.
fn param<T: for<'a, 'py> FromPyObject<'a, 'py>>(params: &Bound<'_, PyDict>, name: &str) -> PyResult<Option<T>> {
    match params.get_item(name)? {
        Some(v) => {
            params.del_item(name)?;
            Ok(Some(v.extract().map_err(Into::into)?))
        }
        None => Ok(None),
    }
}

/// The analytics job `name` with its `params`.
fn job(py: Python<'_>, name: &str, params: Option<&Bound<'_, PyDict>>) -> PyResult<Job> {
    // A copy, so that each parameter read can be removed: what is left is unknown
    let params = match params {
        Some(p) => p.copy()?,
        None => PyDict::new(py),
    };
    let p = &params;
    let job = match name {
        "pagerank" => {
            let d = PageRank::default();
            Job::PageRank(PageRank {
                alpha: param(p, "alpha")?.unwrap_or(d.alpha),
                personalization: None,
                max_iter: param(p, "max_iter")?.unwrap_or(d.max_iter),
                tol: param(p, "tol")?.unwrap_or(d.tol),
            })
        }
        "degree" => Job::Degree { incoming: param(p, "incoming")?.unwrap_or(false) },
        "weakly_connected_components" => Job::WeaklyConnectedComponents,
        "strongly_connected_components" => Job::StronglyConnectedComponents,
        "leiden" => {
            let d = Leiden::default();
            Job::Leiden(Leiden {
                resolution: param(p, "resolution")?.unwrap_or(d.resolution),
                randomness: param(p, "randomness")?.unwrap_or(d.randomness),
                max_iter: param(p, "max_iter")?.unwrap_or(d.max_iter),
                seed: param(p, "seed")?.unwrap_or(d.seed),
            })
        }
        "label_propagation" => Job::LabelPropagation { max_iter: param(p, "max_iter")?.unwrap_or(100) },
        "core_number" => Job::CoreNumber,
        "triangles" => Job::Triangles,
        other => {
            return Err(value_error(format!(
                "unknown job '{}': use 'pagerank', 'degree', 'weakly_connected_components', \
                 'strongly_connected_components', 'leiden', 'label_propagation', 'core_number' or 'triangles'",
                other
            )));
        }
    };
    if let Some((key, _)) = params.iter().next() {
        return Err(value_error(format!("the job '{}' has no parameter {}", name, key.repr()?)));
    }
    Ok(job)
}

fn job_result(py: Python<'_>, result: &JobResult) -> PyResult<(&'static str, Py<PyAny>)> {
    let list = PyList::empty(py);
    let pair = |id: &String, value: Bound<'_, PyAny>| -> PyResult<()> {
        let pair = PyList::empty(py);
        pair.append(id)?;
        pair.append(value)?;
        list.append(pair)
    };
    let key = match result {
        JobResult::Scores(rows) => {
            for (id, score) in rows {
                pair(id, score.into_pyobject(py)?.into_any())?;
            }
            "scores"
        }
        JobResult::Groups(groups) => {
            for group in groups {
                list.append(PyList::new(py, group)?)?;
            }
            "groups"
        }
        JobResult::Counts(rows) => {
            for (id, count) in rows {
                pair(id, count.into_pyobject(py)?.into_any())?;
            }
            "counts"
        }
    };
    Ok((key, list.into_any().unbind()))
}

impl PyStore {
    pub(crate) fn find_in(
        &self,
        py: Python<'_>,
        ns: &str,
        filter: &Bound<'_, PyAny>,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let (ns, request, options) = (ns.to_owned(), FindRequest { filter: to_expr(filter)? }, read.options()?);
            let a = self.query(py, move |db| block_on(db.find(&ns, request, options)))?;
            answer(py, &a, "nodes", nodes(py, &a.value)?)
        })
    }

    pub(crate) fn explain_in(
        &self,
        py: Python<'_>,
        ns: &str,
        filter: &Bound<'_, PyAny>,
        analyze: bool,
        min_seq: Option<u64>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let request = ExplainRequest { filter: to_expr(filter)?, analyze };
            let options = read_options(min_seq, timeout)?;
            let a = self.query(py, move |db| block_on(db.explain(&ns, request, options)))?;
            answer_with(py, &a, explanation(py, &a.value)?)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn neighbourhood_in(
        &self,
        py: Python<'_>,
        ns: &str,
        seeds: &Bound<'_, PyAny>,
        depth: usize,
        dir: &str,
        edge_types: Option<Vec<String>>,
        edge_filter: Option<&Bound<'_, PyAny>>,
        node_filter: Option<&Bound<'_, PyAny>>,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let request = NeighbourhoodRequest {
                seeds: ids(seeds)?,
                depth,
                direction: direction(dir)?,
                edge_types: edge_types.unwrap_or_default(),
                edge_filter: to_expr_opt(edge_filter)?,
                node_filter: to_expr_opt(node_filter)?,
            };
            let options = read.options()?;
            let a = self.query(py, move |db| block_on(db.neighbourhood(&ns, request, options)))?;
            answer(py, &a, "nodes", nodes(py, &a.value)?)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn traverse_in(
        &self,
        py: Python<'_>,
        ns: &str,
        start: &str,
        order: &str,
        depth: Option<usize>,
        dir: &str,
        edge_types: Option<Vec<String>>,
        edge_filter: Option<&Bound<'_, PyAny>>,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let order = match order {
                "bfs" => Order::Bfs,
                "dfs" => Order::Dfs,
                other => return Err(value_error(format!("order must be 'bfs' or 'dfs', not '{}'", other))),
            };
            let request = TraverseRequest {
                start: start.to_owned(),
                order,
                depth,
                direction: direction(dir)?,
                edge_types: edge_types.unwrap_or_default(),
                edge_filter: to_expr_opt(edge_filter)?,
            };
            let options = read.options()?;
            let a = self.query(py, move |db| block_on(db.traverse(&ns, request, options)))?;
            answer(py, &a, "ids", PyList::new(py, &a.value)?.into_any().unbind())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn shortest_path_in(
        &self,
        py: Python<'_>,
        ns: &str,
        from: &str,
        to: &str,
        method: &str,
        weight: Option<String>,
        default_weight: f64,
        coords_arg: Option<&Bound<'_, PyAny>>,
        metric: &str,
        dir: &str,
        max_depth: Option<usize>,
        max_cost: Option<f64>,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let method = match method {
                "bfs" => PathMethod::Bfs,
                "dijkstra" => PathMethod::Dijkstra,
                "astar" => PathMethod::AStar {
                    coords: coords(coords_arg)?,
                    metric: Metric::parse(Some(metric)).map_err(|e| value_error(e.to_string()))?,
                },
                other => {
                    return Err(value_error(format!("method must be 'bfs', 'dijkstra' or 'astar', not '{}'", other)));
                }
            };
            let request = PathRequest {
                from: from.to_owned(),
                to: to.to_owned(),
                method,
                cost: cost(weight, default_weight),
                direction: direction(dir)?,
                max_depth,
                max_cost,
            };
            let options = read.options()?;
            let a = self.query(py, move |db| block_on(db.shortest_path(&ns, request, options)))?;
            let path = match &a.value {
                Some(p) => {
                    let dict = PyDict::new(py);
                    dict.set_item("nodes", &p.nodes)?;
                    dict.set_item("cost", p.cost)?;
                    dict.into_any().unbind()
                }
                None => py.None(),
            };
            answer(py, &a, "path", path)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn random_walks_in(
        &self,
        py: Python<'_>,
        ns: &str,
        start: &str,
        max_length: usize,
        walks: usize,
        min_length: usize,
        allow_revisit: bool,
        seed: Option<u64>,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let request = WalkRequest { start: start.to_owned(), max_length, walks, min_length, allow_revisit, seed };
            let options = read.options()?;
            let a = self.query(py, move |db| block_on(db.random_walks(&ns, request, options)))?;
            answer(py, &a, "walks", PyList::new(py, &a.value)?.into_any().unbind())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn subgraph_in(
        &self,
        py: Python<'_>,
        ns: &str,
        seeds: &Bound<'_, PyAny>,
        depth: usize,
        dir: &str,
        edge_types: Option<Vec<String>>,
        edge_filter: Option<&Bound<'_, PyAny>>,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let request = SubgraphRequest {
                seeds: ids(seeds)?,
                depth,
                direction: direction(dir)?,
                edge_types: edge_types.unwrap_or_default(),
                edge_filter: to_expr_opt(edge_filter)?,
            };
            let options = read.options()?;
            let a = self.query(py, move |db| block_on(db.subgraph(&ns, request, options)))?;
            let edges = PyList::empty(py);
            for e in &a.value.edges {
                edges.append(edge(py, e)?)?;
            }
            let dict = PyDict::new(py);
            dict.set_item("nodes", nodes(py, &a.value.nodes)?)?;
            dict.set_item("edges", edges)?;
            answer_with(py, &a, dict)
        })
    }

    pub(crate) fn match_in(
        &self,
        py: Python<'_>,
        ns: &str,
        pattern: &str,
        filters: Option<&Bound<'_, PyDict>>,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let pattern = Pattern::parse(pattern).map_err(|e| invalid(e.to_string()))?;
            let mut where_ = Vec::new();
            if let Some(filters) = filters {
                for (name, filter) in filters.iter() {
                    let name: String =
                        name.extract().map_err(|_| PyTypeError::new_err("where's keys are variable names (str)"))?;
                    where_.push((name, to_expr(&filter)?));
                }
            }
            // Sorted by variable: the request (and so a cursor) doesn't
            // depend on the dict's order
            where_.sort_by(|a, b| a.0.cmp(&b.0));
            let request = MatchRequest { pattern, filters: where_ };
            let options = read.options()?;
            let a = self.query(py, move |db| block_on(db.match_pattern(&ns, request, options)))?;
            let rows = PyList::empty(py);
            for row in &a.value {
                let dict = PyDict::new(py);
                dict.set_item("nodes", &row.nodes)?;
                let edges = PyList::empty(py);
                for path in &row.edges {
                    edges.append(PyList::new(py, path.iter().map(|e| e.0))?)?;
                }
                dict.set_item("edges", edges)?;
                rows.append(dict)?;
            }
            answer(py, &a, "rows", rows.into_any().unbind())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn analyze_in(
        &self,
        py: Python<'_>,
        ns: &str,
        name: &str,
        params: Option<&Bound<'_, PyDict>>,
        dir: &str,
        weight: Option<String>,
        default_weight: f64,
        read: ReadArgs,
    ) -> PyResult<Py<PyAny>> {
        guard(|| {
            let ns = ns.to_owned();
            let request = AnalyticsRequest {
                projection: ProjectionSpec { direction: direction(dir)?, cost: cost(weight, default_weight) },
                job: job(py, name, params)?,
            };
            let options = read.options()?;
            let a = self.query(py, move |db| block_on(db.analyze(&ns, request, options)))?;
            let (key, rows) = job_result(py, &a.value)?;
            answer(py, &a, key, rows)
        })
    }
}
