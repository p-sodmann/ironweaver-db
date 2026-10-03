//! The resolver: turns a transaction's [`Mutation`]s into core ops.
//!
//! It reads the graph through a transaction-local view: every node and
//! edge the transaction touches is copied into an overlay on first use,
//! and each mutation is checked and applied to the overlay while its ops
//! are emitted. The graph itself is never touched, so a rejected
//! transaction leaves no trace, and the checks see the state after the
//! whole transaction (several mutations of one entity, deletes and
//! re-creates included). Constraints are checked once, at the end.
//!
//! The emitted ops apply without error to the graph the view was taken
//! from: ops are emitted in mutation order and each one is valid for the
//! overlay state before it. New edges get explicit ids counted up from
//! `graph.next_edge_id()`, read afresh for every transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ironweaver_core::{Attributes, Attrs, EdgeId, GraphError, Key, Op, Value};

use crate::catalog::{Constraint, ConstraintKind, NamespaceCatalog};
use crate::mutation::{EdgeKey, MAX_VALUE_DEPTH, Mutation, Target};
use crate::reserved::{self, VERSION_KEY};
use crate::{DbGraph, DbRecord, Error};

type DbOp = Op<DbRecord, DbRecord>;

/// A transaction, resolved: what goes into the commit record and result.
pub(crate) struct Resolved {
    pub ops: Vec<DbOp>,
    pub edge_ids: Vec<EdgeId>,
    pub versions: Vec<(Target, u64)>,
}

/// Resolve `mutations` against `graph` and check the result against the
/// constraints in `catalog`. Doesn't change anything.
pub(crate) fn resolve(graph: &DbGraph, catalog: &NamespaceCatalog, mutations: &[Mutation]) -> Result<Resolved, Error> {
    let mut tx = Tx { view: View::new(graph), ops: Vec::new(), edge_ids: Vec::new() };
    for mutation in mutations {
        tx.apply(mutation)?;
    }
    tx.view.check_constraints(catalog)?;
    Ok(tx.finish())
}

/// Check that the nodes in `graph` satisfy `constraint` (before adding it
/// to the catalog). Reports the violation with the smallest node id.
pub(crate) fn check_existing(graph: &DbGraph, constraint: &Constraint) -> Result<(), Error> {
    let path = constraint.path.keys();
    let mut nodes: Vec<(&str, &DbRecord)> = graph
        .nodes_with_label(constraint.label.as_str())
        .into_iter()
        .filter_map(|ix| graph.node(ix))
        .map(|n| (n.id(), &n.data))
        .collect();
    nodes.sort_unstable_by_key(|(id, _)| *id);
    let violation = |node: &str, other: Option<&str>| violation(constraint, node.to_owned(), other.map(str::to_owned));
    match constraint.kind {
        ConstraintKind::Required => {
            for (id, data) in nodes {
                if value_at(data, path)?.is_none() {
                    return Err(violation(id, None));
                }
            }
        }
        ConstraintKind::Unique => {
            let mut seen: HashMap<Key, &str> = HashMap::new();
            for (id, data) in nodes {
                if let Some(key) = value_at(data, path)?.as_ref().and_then(Key::of) {
                    if let Some(other) = seen.insert(key, id) {
                        return Err(violation(id, Some(other)));
                    }
                }
            }
        }
    }
    Ok(())
}

fn violation(constraint: &Constraint, node: String, other: Option<String>) -> Error {
    Error::ConstraintViolation { constraint: constraint.clone(), node, other }
}

/// The value at `path` (none and missing values are `None`, as for filters).
pub(crate) fn value_at(data: &DbRecord, path: &[String]) -> Result<Option<Value>, GraphError> {
    data.with_value(path, |v| v.cloned())
}

/// Whether `value` (an attribute's own value, at depth 1) is nested deeper
/// than [`MAX_VALUE_DEPTH`]. Recurses at most `MAX_VALUE_DEPTH + 1`
/// levels.
pub(crate) fn too_deep(value: &Value, level: usize) -> bool {
    if level > MAX_VALUE_DEPTH {
        return true;
    }
    match value {
        Value::List(items) => items.iter().any(|v| too_deep(v, level + 1)),
        Value::Dict(map) => map.values().any(|v| too_deep(v, level + 1)),
        _ => false,
    }
}

fn check_value(target: &Target, key: &str, value: &Value) -> Result<(), Error> {
    if too_deep(value, 1) {
        return Err(Error::ValueTooDeep { target: target.clone(), key: key.to_owned() });
    }
    Ok(())
}

/// Check a whole record from user input: reserved keys, then depths (the
/// smallest failing key is reported).
fn check_record(target: &Target, attr: &Attrs, meta: &Attrs) -> Result<(), Error> {
    reserved::check_user_attrs(attr)?;
    reserved::check_user_meta(meta)?;
    for map in [attr, meta] {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_unstable();
        for key in keys {
            if let Some(value) = map.get(key) {
                check_value(target, key, value)?;
            }
        }
    }
    Ok(())
}

fn check_expected(target: &Target, expected: Option<u64>, actual: u64) -> Result<(), Error> {
    match expected {
        Some(expected) if expected != actual => Err(Error::Conflict { target: target.clone(), expected, actual }),
        _ => Ok(()),
    }
}

/// The version an entity written by the transaction gets: its version
/// before the transaction plus one (computed once per transaction).
fn bump(start: u64, new_version: &mut Option<u64>, target: &Target) -> Result<u64, Error> {
    if let Some(v) = *new_version {
        return Ok(v);
    }
    let v = start
        .checked_add(1)
        .filter(|&v| v <= i64::MAX as u64)
        .ok_or_else(|| Error::VersionOverflow { target: target.clone() })?;
    *new_version = Some(v);
    Ok(v)
}

#[derive(Clone, Debug)]
struct NodeState {
    labels: BTreeSet<String>,
    data: DbRecord,
}

#[derive(Clone, Debug)]
struct EdgeState {
    from: String,
    to: String,
    ty: Option<String>,
    data: DbRecord,
}

/// A node or edge as the transaction sees it.
#[derive(Debug)]
struct Entry<S> {
    /// Version before the transaction (0: didn't exist).
    start: u64,
    /// State so far in the transaction (`None`: doesn't exist). Its
    /// `data.version` is what the ops emitted so far leave in the graph.
    state: Option<S>,
    /// The version after the transaction, once the transaction wrote it.
    new_version: Option<u64>,
}

impl<S> Entry<S> {
    fn absent() -> Self {
        Entry { start: 0, state: None, new_version: None }
    }
}

/// The graph as seen by the transaction: the graph plus an overlay.
struct View<'g> {
    graph: &'g DbGraph,
    nodes: BTreeMap<String, Entry<NodeState>>,
    edges: BTreeMap<EdgeId, Entry<EdgeState>>,
    next_edge_id: u64,
}

impl<'g> View<'g> {
    fn new(graph: &'g DbGraph) -> Self {
        View { graph, nodes: BTreeMap::new(), edges: BTreeMap::new(), next_edge_id: graph.next_edge_id().0 }
    }

    fn node(&mut self, id: &str) -> &mut Entry<NodeState> {
        let graph = self.graph;
        self.nodes.entry(id.to_owned()).or_insert_with(|| {
            let found = graph.node_ix(id).and_then(|ix| {
                let node = graph.node(ix)?;
                let labels = graph.label_names(ix)?.into_iter().map(str::to_owned).collect();
                Some(NodeState { labels, data: node.data.clone() })
            });
            match found {
                Some(state) => Entry { start: state.data.version, state: Some(state), new_version: None },
                None => Entry::absent(),
            }
        })
    }

    fn edge(&mut self, id: EdgeId) -> &mut Entry<EdgeState> {
        let graph = self.graph;
        self.edges.entry(id).or_insert_with(|| {
            let found = graph.edge_ix(id).and_then(|ix| {
                let edge = graph.edge(ix)?;
                Some(EdgeState {
                    from: graph.node(edge.source())?.id().to_owned(),
                    to: graph.node(edge.target())?.id().to_owned(),
                    ty: graph.edge_type_name(ix).map(str::to_owned),
                    data: edge.data.clone(),
                })
            });
            match found {
                Some(state) => Entry { start: state.data.version, state: Some(state), new_version: None },
                None => Entry::absent(),
            }
        })
    }

    fn exists(&mut self, id: &str) -> bool {
        self.node(id).state.is_some()
    }

    /// The live node `id`, written by the transaction: checks `expected`,
    /// then that the node exists. Returns it with its new version.
    fn write_node(&mut self, id: &str, expected: Option<u64>) -> Result<(&mut NodeState, u64), Error> {
        let target = Target::Node(id.to_owned());
        let Entry { start, state, new_version } = self.node(id);
        check_expected(&target, expected, *start)?;
        let Some(state) = state.as_mut() else { return Err(Error::NotFound { target }) };
        let version = bump(*start, new_version, &target)?;
        Ok((state, version))
    }

    /// Like [`write_node`](Self::write_node), for an edge.
    fn write_edge(&mut self, id: EdgeId, expected: Option<u64>) -> Result<(&mut EdgeState, u64), Error> {
        let target = Target::Edge(id);
        let Entry { start, state, new_version } = self.edge(id);
        check_expected(&target, expected, *start)?;
        let Some(state) = state.as_mut() else { return Err(Error::NotFound { target }) };
        let version = bump(*start, new_version, &target)?;
        Ok((state, version))
    }

    /// The payload of a live node or edge, written by the transaction.
    fn write_record(&mut self, target: &Target, expected: Option<u64>) -> Result<&mut DbRecord, Error> {
        Ok(match target {
            Target::Node(id) => &mut self.write_node(id, expected)?.0.data,
            Target::Edge(id) => &mut self.write_edge(*id, expected)?.0.data,
        })
    }

    /// Mark the edges of node `id` deleted (its deletion removes them).
    /// Edges only in the graph are marked without copying their payload.
    fn drop_edges_of(&mut self, id: &str) {
        let incident = |s: &EdgeState| s.from == id || s.to == id;
        for entry in self.edges.values_mut() {
            if entry.state.as_ref().is_some_and(incident) {
                entry.state = None;
            }
        }
        let graph = self.graph;
        let Some(node) = graph.node_ix(id).and_then(|ix| graph.node(ix)) else { return };
        for &e in node.out_edges().iter().chain(node.in_edges()) {
            if let Some(edge) = graph.edge(e) {
                // Edges in the overlay were handled above; if this node was
                // deleted and re-created, its old edges are in the overlay
                self.edges.entry(edge.id()).or_insert(Entry {
                    start: edge.data.version,
                    state: None,
                    new_version: None,
                });
            }
        }
    }

    /// The live edges from `from` to `to` with type `ty`, sorted by id.
    fn edges_between(&self, from: &str, to: &str, ty: Option<&str>) -> Vec<EdgeId> {
        let mut found = BTreeSet::new();
        for (&id, entry) in &self.edges {
            if entry.state.as_ref().is_some_and(|s| s.from == from && s.to == to && s.ty.as_deref() == ty) {
                found.insert(id);
            }
        }
        let graph = self.graph;
        if let (Some(a), Some(b)) = (graph.node_ix(from), graph.node_ix(to)) {
            for e in graph.edges_between(a, b, None) {
                if let Some(edge) = graph.edge(e) {
                    if !self.edges.contains_key(&edge.id()) && graph.edge_type_name(e) == ty {
                        found.insert(edge.id());
                    }
                }
            }
        }
        found.into_iter().collect()
    }

    fn new_edge_id(&mut self) -> Result<EdgeId, Error> {
        let id = self.next_edge_id;
        self.next_edge_id = id.checked_add(1).ok_or(Error::EdgeIdsExhausted)?;
        Ok(EdgeId(id))
    }

    /// Check every constraint for the nodes the transaction wrote (the
    /// others were valid before and haven't changed). Constraints in
    /// catalog order, nodes by id; the first violation is reported.
    fn check_constraints(&self, catalog: &NamespaceCatalog) -> Result<(), Error> {
        for constraint in catalog.constraints() {
            let label = constraint.label.as_str();
            let path = constraint.path.keys();
            let written = self.nodes.iter().filter(|(_, e)| e.new_version.is_some());
            match constraint.kind {
                ConstraintKind::Required => {
                    for (id, entry) in written {
                        if let Some(state) = labeled(entry, label) {
                            if value_at(&state.data, path)?.is_none() {
                                return Err(violation(constraint, id.clone(), None));
                            }
                        }
                    }
                }
                ConstraintKind::Unique => {
                    // Every overlay node with the label, by key
                    let mut overlay: HashMap<Key, BTreeSet<&str>> = HashMap::new();
                    for (id, entry) in &self.nodes {
                        if let Some(state) = labeled(entry, label) {
                            if let Some(key) = value_at(&state.data, path)?.as_ref().and_then(Key::of) {
                                overlay.entry(key).or_default().insert(id);
                            }
                        }
                    }
                    for (id, entry) in written {
                        let Some(state) = labeled(entry, label) else { continue };
                        let Some(value) = value_at(&state.data, path)? else { continue };
                        let Some(key) = Key::of(&value) else { continue };
                        let twin = overlay.get(&key).and_then(|ids| ids.iter().find(|o| **o != id.as_str()));
                        let other = match twin {
                            Some(other) => Some((*other).to_owned()),
                            None => self.graph_twin(label, path, &value, &key)?,
                        };
                        if let Some(other) = other {
                            return Err(violation(constraint, id.clone(), Some(other)));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The smallest id of a graph node outside the overlay that has
    /// `label` and the key `key` at `path`. Uses the property index on
    /// `path` (which the catalog guarantees for unique paths), or scans
    /// the label's nodes if there is none.
    fn graph_twin(&self, label: &str, path: &[String], value: &Value, key: &Key) -> Result<Option<String>, Error> {
        let graph = self.graph;
        let Some(sym) = graph.symbol(label) else { return Ok(None) };
        let candidates = match graph.find_nodes(path, value)? {
            Some(found) => found,
            None => graph.nodes_with_label(label),
        };
        let mut best: Option<&str> = None;
        for ix in candidates {
            let Some(node) = graph.node(ix) else { continue };
            if !node.has_label(sym) || self.nodes.contains_key(node.id()) {
                continue;
            }
            if value_at(&node.data, path)?.as_ref().and_then(Key::of).as_ref() == Some(key)
                && best.is_none_or(|b| node.id() < b)
            {
                best = Some(node.id());
            }
        }
        Ok(best.map(str::to_owned))
    }
}

/// The entry's node, if it exists and has `label`.
fn labeled<'a>(entry: &'a Entry<NodeState>, label: &str) -> Option<&'a NodeState> {
    entry.state.as_ref().filter(|s| s.labels.contains(label))
}

struct Tx<'g> {
    view: View<'g>,
    ops: Vec<DbOp>,
    edge_ids: Vec<EdgeId>,
}

impl Tx<'_> {
    fn apply(&mut self, mutation: &Mutation) -> Result<(), Error> {
        match mutation {
            Mutation::UpsertNode { id, labels, attr, meta, expected_version } => {
                let target = Target::Node(id.clone());
                check_record(&target, attr, meta)?;
                let Entry { start, state, new_version } = self.view.node(id);
                check_expected(&target, *expected_version, *start)?;
                let version = bump(*start, new_version, &target)?;
                let data = DbRecord { attr: attr.clone(), meta: meta.clone(), version };
                let labels: BTreeSet<String> = labels.iter().cloned().collect();
                match state {
                    None => {
                        self.ops.push(Op::AddNode {
                            id: id.clone(),
                            labels: labels.iter().cloned().collect(),
                            data: data.clone(),
                        });
                        *state = Some(NodeState { labels, data });
                    }
                    Some(node) => {
                        self.ops.push(Op::SetNode { id: id.clone(), data: data.clone() });
                        node.data = data;
                        for label in labels {
                            if node.labels.insert(label.clone()) {
                                self.ops.push(Op::AddLabel { id: id.clone(), label });
                            }
                        }
                    }
                }
            }
            Mutation::DeleteNode { id, expected_version } => {
                let target = Target::Node(id.clone());
                let entry = self.view.node(id);
                check_expected(&target, *expected_version, entry.start)?;
                if entry.state.take().is_none() {
                    return Err(Error::NotFound { target });
                }
                self.view.drop_edges_of(id);
                self.ops.push(Op::RemoveNode { id: id.clone() });
            }
            Mutation::AddEdge { from, to, ty, attr, meta } => {
                self.add_edge(from, to, ty, attr, meta)?;
            }
            Mutation::UpsertEdge { key, attr, meta, expected_version } => {
                let id = match key {
                    EdgeKey::Id(id) => *id,
                    EdgeKey::Endpoints { from, to, ty } => match self.view.edges_between(from, to, ty.as_deref())[..] {
                        [] => {
                            if let Some(expected) = expected_version.filter(|&v| v != 0) {
                                return Err(Error::NoMatchingEdge {
                                    from: from.clone(),
                                    to: to.clone(),
                                    ty: ty.clone(),
                                    expected,
                                });
                            }
                            return self.add_edge(from, to, ty, attr, meta);
                        }
                        [id] => id,
                        ref many => {
                            return Err(Error::AmbiguousEdge {
                                from: from.clone(),
                                to: to.clone(),
                                ty: ty.clone(),
                                count: many.len(),
                            });
                        }
                    },
                };
                check_record(&Target::Edge(id), attr, meta)?;
                let (edge, version) = self.view.write_edge(id, *expected_version)?;
                edge.data = DbRecord { attr: attr.clone(), meta: meta.clone(), version };
                self.ops.push(Op::SetEdge { id, data: edge.data.clone() });
                self.edge_ids.push(id);
            }
            Mutation::DeleteEdge { id, expected_version } => {
                let target = Target::Edge(*id);
                let entry = self.view.edge(*id);
                check_expected(&target, *expected_version, entry.start)?;
                if entry.state.take().is_none() {
                    return Err(Error::NotFound { target });
                }
                self.ops.push(Op::RemoveEdge { id: *id });
            }
            Mutation::SetAttr { target, key, value, expected_version } => {
                reserved::check_user_key(key)?;
                check_value(target, key, value)?;
                let record = self.view.write_record(target, *expected_version)?;
                record.attr.insert(key.clone(), value.clone());
                self.ops.push(attr_op(target, key, Some(value.clone())));
            }
            Mutation::RemoveAttr { target, key, expected_version } => {
                reserved::check_user_key(key)?;
                let record = self.view.write_record(target, *expected_version)?;
                if record.attr.remove(key).is_some() {
                    self.ops.push(attr_op(target, key, None));
                }
            }
            Mutation::AppendAttr { target, key, value, expected_version } => {
                reserved::check_user_key(key)?;
                let record = self.view.write_record(target, *expected_version)?;
                let mut items = match record.attr.get(key) {
                    None | Some(Value::None) => Vec::with_capacity(1),
                    Some(Value::List(items)) => items.clone(),
                    Some(_) => return Err(Error::NotAList { target: target.clone(), key: key.clone() }),
                };
                items.push(value.clone());
                let list = Value::List(items);
                check_value(target, key, &list)?;
                record.attr.insert(key.clone(), list.clone());
                self.ops.push(attr_op(target, key, Some(list)));
            }
            Mutation::AddLabel { id, label, expected_version } => {
                let (node, _) = self.view.write_node(id, *expected_version)?;
                if node.labels.insert(label.clone()) {
                    self.ops.push(Op::AddLabel { id: id.clone(), label: label.clone() });
                }
            }
            Mutation::RemoveLabel { id, label, expected_version } => {
                let (node, _) = self.view.write_node(id, *expected_version)?;
                if node.labels.remove(label) {
                    self.ops.push(Op::RemoveLabel { id: id.clone(), label: label.clone() });
                }
            }
            Mutation::SetEdgeType { id, ty, expected_version } => {
                let (edge, _) = self.view.write_edge(*id, *expected_version)?;
                if edge.ty != *ty {
                    edge.ty.clone_from(ty);
                    self.ops.push(Op::SetEdgeType { id: *id, ty: ty.clone() });
                }
            }
        }
        Ok(())
    }

    /// Add an edge with a new id (version 1).
    fn add_edge(&mut self, from: &str, to: &str, ty: &Option<String>, attr: &Attrs, meta: &Attrs) -> Result<(), Error> {
        let id = EdgeId(self.view.next_edge_id);
        check_record(&Target::Edge(id), attr, meta)?;
        for end in [from, to] {
            if !self.view.exists(end) {
                return Err(Error::NotFound { target: Target::Node(end.to_owned()) });
            }
        }
        let id = self.view.new_edge_id()?;
        let data = DbRecord { attr: attr.clone(), meta: meta.clone(), version: 1 };
        let state = EdgeState { from: from.to_owned(), to: to.to_owned(), ty: ty.clone(), data: data.clone() };
        self.view.edges.insert(id, Entry { start: 0, state: Some(state), new_version: Some(1) });
        self.ops.push(Op::AddEdge { id, from: from.to_owned(), to: to.to_owned(), ty: ty.clone(), data });
        self.edge_ids.push(id);
        Ok(())
    }

    /// Emit the version ops (for written entities whose version the ops
    /// don't set yet) and collect the new versions.
    fn finish(mut self) -> Resolved {
        let mut versions = Vec::new();
        for (id, entry) in &self.view.nodes {
            if let (Some(state), Some(version)) = (&entry.state, entry.new_version) {
                if state.data.version != version {
                    self.ops.push(Op::SetNodeAttr {
                        id: id.clone(),
                        key: VERSION_KEY.to_owned(),
                        value: version_value(version),
                    });
                }
                versions.push((Target::Node(id.clone()), version));
            }
        }
        for (&id, entry) in &self.view.edges {
            if let (Some(state), Some(version)) = (&entry.state, entry.new_version) {
                if state.data.version != version {
                    self.ops.push(Op::SetEdgeAttr { id, key: VERSION_KEY.to_owned(), value: version_value(version) });
                }
                versions.push((Target::Edge(id), version));
            }
        }
        Resolved { ops: self.ops, edge_ids: self.edge_ids, versions }
    }
}

/// The value of a version op (see `DbRecord`'s `AttrPatch`). Versions from
/// [`bump`] are at most `i64::MAX`, so the cast is exact.
fn version_value(version: u64) -> Option<Value> {
    Some(Value::Int(version as i64))
}

fn attr_op(target: &Target, key: &str, value: Option<Value>) -> DbOp {
    match target {
        Target::Node(id) => Op::SetNodeAttr { id: id.clone(), key: key.to_owned(), value },
        Target::Edge(id) => Op::SetEdgeAttr { id: *id, key: key.to_owned(), value },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(levels: usize, inner: Value) -> Value {
        (0..levels).fold(inner, |v, _| Value::List(vec![v]))
    }

    #[test]
    fn depth_limit_is_the_core_s() {
        // A scalar inside 99 lists is at depth 100: fine
        assert!(!too_deep(&nest(99, Value::Int(1)), 1));
        assert!(too_deep(&nest(100, Value::Int(1)), 1));
        // An empty container is a value like a scalar: at depth 100 fine,
        // at 101 too deep
        assert!(!too_deep(&nest(99, Value::List(vec![])), 1));
        assert!(!too_deep(&nest(99, Value::Dict(Attrs::new())), 1));
        assert!(too_deep(&nest(100, Value::List(vec![])), 1));
        assert!(too_deep(&nest(100, Value::Dict(Attrs::new())), 1));
        let dict = |v: Value| Value::Dict([("k".to_owned(), v)].into());
        assert!(!too_deep(&(0..99).fold(Value::Int(1), |v, _| dict(v)), 1));
        assert!(too_deep(&(0..100).fold(Value::Int(1), |v, _| dict(v)), 1));
    }
}
