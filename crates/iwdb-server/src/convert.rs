//! Translation between the protos and the trait's types, both ways: the
//! server reads requests and writes answers, the client (feature `client`)
//! does the opposite with the same functions. Translation only (design rule
//! 8): the checks here are that a message is complete and that its values
//! decode; everything else is the trait's.
//!
//! `Value`, `Expr` and `Pattern` are the core's serde form in postcard
//! (ADR 0023). A decode error is `invalid_argument` with the core's message
//! (upstream #29: postcard drops custom serde messages, so the core
//! remembers them for `format::take_error`).

// The client half is used only with the `client` feature
#![cfg_attr(not(feature = "client"), allow(dead_code))]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ironweaver_core::algo::{Leiden, PageRank};
use ironweaver_core::format::take_error;
use ironweaver_core::pathfinding::{Coords, EdgeCost, Metric};
use ironweaver_core::query::Pattern;
use ironweaver_core::{Attrs, Direction, EdgeId, Expr, Value};
use iwdb_engine::catalog::{
    AttrPath, Constraint, ConstraintKind, IndexChanges, IndexDef, Label, NamespaceCatalog, NamespaceName,
};
use iwdb_engine::{CatalogChange, CommitResult, CommitTime, EdgeKey, IdempotencyKey, Mutation, Target};
use iwdb_query::read::{Explain, Lookup, Plan};
use iwdb_query::{
    AnalyticsRequest, Answer, CommitOptions, Cursor, Edge, Error, ExplainRequest, FindRequest, IndexSize, IndexState,
    IndexStatus, Job, JobResult, Limits, MatchRequest, MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Order,
    Path, PathMethod, PathRequest, ProjectionSpec, QueryOptions, Subgraph, SubgraphRequest, TraverseRequest,
    WalkRequest, Work,
};
use iwdb_storage::format::Damage;
use iwdb_storage::namespaces::{Event, EventKind, NamespaceInfo, NamespaceResult};
use iwdb_storage::{CutTail, HistoryId, RecoveryReport, SkippedCheckpoint};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::proto as pb;

pub(crate) type PbAttrs = BTreeMap<String, pb::Value>;

// ---- numbers ----

/// A count or limit from the wire; above `usize::MAX` (on 32-bit targets)
/// it saturates, and the database lowers it to its cap.
fn size(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn wide(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn missing(what: &str) -> Error {
    Error::invalid(format!("{} is missing", what))
}

// ---- the core's types in postcard (ADR 0023) ----

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>, Error> {
    take_error();
    postcard::to_stdvec(value)
        .map_err(|e| Error::invalid(format!("invalid {}: {}", what, take_error().unwrap_or_else(|| e.to_string()))))
}

fn decode<T: DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T, Error> {
    // A message left by an earlier failure on this thread isn't ours
    take_error();
    match postcard::take_from_bytes::<T>(bytes) {
        Ok((value, [])) => Ok(value),
        Ok((_, rest)) => {
            Err(Error::invalid(format!("invalid {}: {} bytes after its postcard encoding", what, rest.len())))
        }
        Err(e) => Err(Error::invalid(format!("invalid {}: {}", what, take_error().unwrap_or_else(|| e.to_string())))),
    }
}

pub(crate) fn value_to_pb(value: &Value) -> Result<pb::Value, Error> {
    Ok(pb::Value { form: Some(pb::value::Form::Postcard(encode(value, "value")?)) })
}

pub(crate) fn value_from_pb(value: pb::Value, what: &str) -> Result<Value, Error> {
    match value.form {
        Some(pb::value::Form::Postcard(bytes)) => decode(&bytes, what),
        None => Err(Error::invalid(format!("{} has no form", what))),
    }
}

fn value_field(value: Option<pb::Value>, what: &str) -> Result<Value, Error> {
    value_from_pb(value.ok_or_else(|| missing(what))?, what)
}

pub(crate) fn attrs_to_pb(attrs: &Attrs) -> Result<PbAttrs, Error> {
    attrs.iter().map(|(k, v)| Ok((k.clone(), value_to_pb(v)?))).collect()
}

pub(crate) fn attrs_from_pb(attrs: PbAttrs) -> Result<Attrs, Error> {
    attrs
        .into_iter()
        .map(|(k, v)| {
            let value = value_from_pb(v, &format!("value of '{}'", k))?;
            Ok((k, value))
        })
        .collect()
}

pub(crate) fn expr_to_pb(expr: &Expr) -> Result<pb::Expr, Error> {
    Ok(pb::Expr { form: Some(pb::expr::Form::Postcard(encode(expr, "filter")?)) })
}

fn expr_from_pb(expr: pb::Expr, what: &str) -> Result<Expr, Error> {
    match expr.form {
        Some(pb::expr::Form::Postcard(bytes)) => decode(&bytes, what),
        None => Err(Error::invalid(format!("{} has no form", what))),
    }
}

fn opt_expr_to_pb(expr: &Option<Expr>) -> Result<Option<pb::Expr>, Error> {
    expr.as_ref().map(expr_to_pb).transpose()
}

fn opt_expr_from_pb(expr: Option<pb::Expr>, what: &str) -> Result<Option<Expr>, Error> {
    expr.map(|e| expr_from_pb(e, what)).transpose()
}

/// The text if the core can write the pattern as text, postcard otherwise.
pub(crate) fn pattern_to_pb(pattern: &Pattern) -> Result<pb::Pattern, Error> {
    let form = match pattern.to_text() {
        Ok(text) => pb::pattern::Form::Text(text),
        Err(_) => pb::pattern::Form::Postcard(encode(pattern, "pattern")?),
    };
    Ok(pb::Pattern { form: Some(form) })
}

pub(crate) fn pattern_from_pb(pattern: Option<pb::Pattern>) -> Result<Pattern, Error> {
    match pattern.and_then(|p| p.form) {
        Some(pb::pattern::Form::Text(text)) => Ok(Pattern::parse(&text)?),
        Some(pb::pattern::Form::Postcard(bytes)) => decode(&bytes, "pattern"),
        None => Err(missing("the pattern")),
    }
}

// ---- small shared types ----

fn path_to_pb(keys: &[String]) -> pb::AttrPath {
    pb::AttrPath { keys: keys.to_vec() }
}

fn keys_from_pb(path: Option<pb::AttrPath>) -> Vec<String> {
    path.map(|p| p.keys).unwrap_or_default()
}

fn attr_path_from_pb(path: Option<pb::AttrPath>) -> Result<AttrPath, Error> {
    AttrPath::new(keys_from_pb(path)).map_err(|e| Error::invalid(e.to_string()))
}

fn direction_to_pb(direction: Direction) -> i32 {
    match direction {
        Direction::Out => pb::Direction::Out,
        Direction::In => pb::Direction::In,
        Direction::Both => pb::Direction::Both,
    }
    .into()
}

fn direction_from_pb(direction: i32) -> Result<Direction, Error> {
    match pb::Direction::try_from(direction) {
        Ok(pb::Direction::Unspecified | pb::Direction::Out) => Ok(Direction::Out),
        Ok(pb::Direction::In) => Ok(Direction::In),
        Ok(pb::Direction::Both) => Ok(Direction::Both),
        Err(_) => Err(Error::invalid(format!("unknown direction {}", direction))),
    }
}

fn cost_to_pb(cost: &EdgeCost) -> pb::EdgeCost {
    let kind = match cost {
        EdgeCost::Unit => pb::edge_cost::Kind::Unit(pb::UnitCost {}),
        EdgeCost::Weighted { key, default } => {
            pb::edge_cost::Kind::Weighted(pb::WeightedCost { key: key.clone(), default_weight: *default })
        }
    };
    pb::EdgeCost { kind: Some(kind) }
}

fn cost_from_pb(cost: Option<pb::EdgeCost>) -> EdgeCost {
    match cost.and_then(|c| c.kind) {
        None | Some(pb::edge_cost::Kind::Unit(_)) => EdgeCost::Unit,
        Some(pb::edge_cost::Kind::Weighted(w)) => EdgeCost::Weighted { key: w.key, default: w.default_weight },
    }
}

fn time_from_pb(micros: i64) -> CommitTime {
    CommitTime(micros)
}

// ---- options and answers ----

/// Whole milliseconds, rounded up so that a positive timeout stays positive.
fn millis(timeout: Duration) -> u32 {
    let ms = timeout.as_millis() + u128::from(timeout.subsec_nanos() % 1_000_000 != 0);
    u32::try_from(ms).unwrap_or(u32::MAX)
}

/// The trait's options from the request's and the `grpc-timeout` header's
/// (`deadline`): the smaller timeout applies (ADR 0026).
pub(crate) fn options_from_pb(
    options: Option<pb::QueryOptions>,
    deadline: Option<Duration>,
) -> Result<QueryOptions, Error> {
    let o = options.unwrap_or_default();
    let history = match o.history.as_str() {
        "" => None,
        text => Some(text.parse::<HistoryId>().map_err(Error::invalid)?),
    };
    let asked = o.timeout_ms.map(|ms| Duration::from_millis(u64::from(ms)));
    let timeout = match (asked, deadline) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let l = o.limits.unwrap_or_default();
    Ok(QueryOptions {
        min_seq: o.min_seq,
        history,
        timeout,
        limits: Limits {
            max_results: l.max_results.map(size),
            max_visited: l.max_visited.map(size),
            max_edges: l.max_edges.map(size),
        },
        partial: o.partial,
        cursor: (!o.cursor.is_empty()).then(|| Cursor::new(o.cursor)),
    })
}

pub(crate) fn options_to_pb(o: &QueryOptions) -> pb::QueryOptions {
    let l = &o.limits;
    pb::QueryOptions {
        min_seq: o.min_seq,
        history: o.history.map(|h| h.to_string()).unwrap_or_default(),
        timeout_ms: o.timeout.map(millis),
        limits: Some(pb::Limits {
            max_results: l.max_results.map(wide),
            max_visited: l.max_visited.map(wide),
            max_edges: l.max_edges.map(wide),
        }),
        partial: o.partial,
        cursor: o.cursor.as_ref().map(|c| c.as_str().to_owned()).unwrap_or_default(),
    }
}

pub(crate) fn meta_to_pb<T>(answer: &Answer<T>) -> pb::AnswerMeta {
    pb::AnswerMeta {
        seq: answer.seq,
        next: answer.next.as_ref().map(|c| c.as_str().to_owned()).unwrap_or_default(),
        truncated: answer.truncated,
        work: Some(pb::Work { visited: wide(answer.work.visited), edges: wide(answer.work.edges) }),
    }
}

pub(crate) fn answer_from_pb<T>(value: T, meta: pb::AnswerMeta) -> Answer<T> {
    let work = meta.work.unwrap_or_default();
    Answer {
        value,
        seq: meta.seq,
        next: (!meta.next.is_empty()).then(|| Cursor::new(meta.next)),
        truncated: meta.truncated,
        work: Work { visited: size(work.visited), edges: size(work.edges) },
    }
}

// ---- entities ----

pub(crate) fn node_to_pb(node: &Node) -> Result<pb::Node, Error> {
    Ok(pb::Node {
        id: node.id.clone(),
        labels: node.labels.clone(),
        attr: attrs_to_pb(&node.attr)?,
        meta: attrs_to_pb(&node.meta)?,
        version: node.version,
    })
}

pub(crate) fn node_from_pb(node: pb::Node) -> Result<Node, Error> {
    Ok(Node {
        id: node.id,
        labels: node.labels,
        attr: attrs_from_pb(node.attr)?,
        meta: attrs_from_pb(node.meta)?,
        version: node.version,
    })
}

pub(crate) fn edge_to_pb(edge: &Edge) -> Result<pb::Edge, Error> {
    Ok(pb::Edge {
        id: edge.id.0,
        from: edge.from.clone(),
        to: edge.to.clone(),
        r#type: edge.ty.clone(),
        attr: attrs_to_pb(&edge.attr)?,
        meta: attrs_to_pb(&edge.meta)?,
        version: edge.version,
    })
}

pub(crate) fn edge_from_pb(edge: pb::Edge) -> Result<Edge, Error> {
    Ok(Edge {
        id: EdgeId(edge.id),
        from: edge.from,
        to: edge.to,
        ty: edge.r#type,
        attr: attrs_from_pb(edge.attr)?,
        meta: attrs_from_pb(edge.meta)?,
        version: edge.version,
    })
}

pub(crate) fn nodes_to_pb(nodes: &[Node]) -> Result<Vec<pb::Node>, Error> {
    nodes.iter().map(node_to_pb).collect()
}

pub(crate) fn nodes_from_pb(nodes: Vec<pb::Node>) -> Result<Vec<Node>, Error> {
    nodes.into_iter().map(node_from_pb).collect()
}

pub(crate) fn edges_to_pb(edges: &[Edge]) -> Result<Vec<pb::Edge>, Error> {
    edges.iter().map(edge_to_pb).collect()
}

pub(crate) fn edges_from_pb(edges: Vec<pb::Edge>) -> Result<Vec<Edge>, Error> {
    edges.into_iter().map(edge_from_pb).collect()
}

pub(crate) fn maybe_nodes_to_pb(nodes: &[Option<Node>]) -> Result<Vec<pb::MaybeNode>, Error> {
    nodes.iter().map(|n| Ok(pb::MaybeNode { node: n.as_ref().map(node_to_pb).transpose()? })).collect()
}

pub(crate) fn maybe_nodes_from_pb(nodes: Vec<pb::MaybeNode>) -> Result<Vec<Option<Node>>, Error> {
    nodes.into_iter().map(|n| n.node.map(node_from_pb).transpose()).collect()
}

pub(crate) fn maybe_edges_to_pb(edges: &[Option<Edge>]) -> Result<Vec<pb::MaybeEdge>, Error> {
    edges.iter().map(|e| Ok(pb::MaybeEdge { edge: e.as_ref().map(edge_to_pb).transpose()? })).collect()
}

pub(crate) fn maybe_edges_from_pb(edges: Vec<pb::MaybeEdge>) -> Result<Vec<Option<Edge>>, Error> {
    edges.into_iter().map(|e| e.edge.map(edge_from_pb).transpose()).collect()
}

// ---- mutations and commits ----

fn target_to_pb(target: &Target) -> pb::Target {
    let kind = match target {
        Target::Node(id) => pb::target::Kind::Node(id.clone()),
        Target::Edge(id) => pb::target::Kind::Edge(id.0),
    };
    pb::Target { kind: Some(kind) }
}

fn target_from_pb(target: Option<pb::Target>) -> Result<Target, Error> {
    match target.and_then(|t| t.kind) {
        Some(pb::target::Kind::Node(id)) => Ok(Target::Node(id)),
        Some(pb::target::Kind::Edge(id)) => Ok(Target::Edge(EdgeId(id))),
        None => Err(missing("the target")),
    }
}

fn edge_key_to_pb(key: &EdgeKey) -> pb::EdgeKey {
    let kind = match key {
        EdgeKey::Id(id) => pb::edge_key::Kind::Id(id.0),
        EdgeKey::Endpoints { from, to, ty } => {
            pb::edge_key::Kind::Endpoints(pb::EdgeEndpoints { from: from.clone(), to: to.clone(), r#type: ty.clone() })
        }
    };
    pb::EdgeKey { kind: Some(kind) }
}

fn edge_key_from_pb(key: Option<pb::EdgeKey>) -> Result<EdgeKey, Error> {
    match key.and_then(|k| k.kind) {
        Some(pb::edge_key::Kind::Id(id)) => Ok(EdgeKey::Id(EdgeId(id))),
        Some(pb::edge_key::Kind::Endpoints(e)) => Ok(EdgeKey::Endpoints { from: e.from, to: e.to, ty: e.r#type }),
        None => Err(missing("the edge key")),
    }
}

pub(crate) fn mutation_to_pb(mutation: &Mutation) -> Result<pb::Mutation, Error> {
    use pb::mutation::Kind;
    let kind = match mutation {
        Mutation::UpsertNode { id, labels, attr, meta, expected_version } => Kind::UpsertNode(pb::UpsertNode {
            id: id.clone(),
            labels: labels.clone(),
            attr: attrs_to_pb(attr)?,
            meta: attrs_to_pb(meta)?,
            expected_version: *expected_version,
        }),
        Mutation::DeleteNode { id, expected_version } => {
            Kind::DeleteNode(pb::DeleteNode { id: id.clone(), expected_version: *expected_version })
        }
        Mutation::AddEdge { from, to, ty, attr, meta } => Kind::AddEdge(pb::AddEdge {
            from: from.clone(),
            to: to.clone(),
            r#type: ty.clone(),
            attr: attrs_to_pb(attr)?,
            meta: attrs_to_pb(meta)?,
        }),
        Mutation::UpsertEdge { key, attr, meta, expected_version } => Kind::UpsertEdge(pb::UpsertEdge {
            key: Some(edge_key_to_pb(key)),
            attr: attrs_to_pb(attr)?,
            meta: attrs_to_pb(meta)?,
            expected_version: *expected_version,
        }),
        Mutation::DeleteEdge { id, expected_version } => {
            Kind::DeleteEdge(pb::DeleteEdge { id: id.0, expected_version: *expected_version })
        }
        Mutation::SetAttr { target, key, value, expected_version } => Kind::SetAttr(pb::SetAttr {
            target: Some(target_to_pb(target)),
            key: key.clone(),
            value: Some(value_to_pb(value)?),
            expected_version: *expected_version,
        }),
        Mutation::RemoveAttr { target, key, expected_version } => Kind::RemoveAttr(pb::RemoveAttr {
            target: Some(target_to_pb(target)),
            key: key.clone(),
            expected_version: *expected_version,
        }),
        Mutation::AppendAttr { target, key, value, expected_version } => Kind::AppendAttr(pb::AppendAttr {
            target: Some(target_to_pb(target)),
            key: key.clone(),
            value: Some(value_to_pb(value)?),
            expected_version: *expected_version,
        }),
        Mutation::AddLabel { id, label, expected_version } => {
            Kind::AddLabel(pb::AddLabel { id: id.clone(), label: label.clone(), expected_version: *expected_version })
        }
        Mutation::RemoveLabel { id, label, expected_version } => Kind::RemoveLabel(pb::RemoveLabel {
            id: id.clone(),
            label: label.clone(),
            expected_version: *expected_version,
        }),
        Mutation::SetEdgeType { id, ty, expected_version } => {
            Kind::SetEdgeType(pb::SetEdgeType { id: id.0, r#type: ty.clone(), expected_version: *expected_version })
        }
    };
    Ok(pb::Mutation { kind: Some(kind) })
}

pub(crate) fn mutation_from_pb(mutation: pb::Mutation) -> Result<Mutation, Error> {
    use pb::mutation::Kind;
    Ok(match mutation.kind.ok_or_else(|| missing("the kind of a mutation"))? {
        Kind::UpsertNode(n) => Mutation::UpsertNode {
            id: n.id,
            labels: n.labels,
            attr: attrs_from_pb(n.attr)?,
            meta: attrs_from_pb(n.meta)?,
            expected_version: n.expected_version,
        },
        Kind::DeleteNode(n) => Mutation::DeleteNode { id: n.id, expected_version: n.expected_version },
        Kind::AddEdge(e) => Mutation::AddEdge {
            from: e.from,
            to: e.to,
            ty: e.r#type,
            attr: attrs_from_pb(e.attr)?,
            meta: attrs_from_pb(e.meta)?,
        },
        Kind::UpsertEdge(e) => Mutation::UpsertEdge {
            key: edge_key_from_pb(e.key)?,
            attr: attrs_from_pb(e.attr)?,
            meta: attrs_from_pb(e.meta)?,
            expected_version: e.expected_version,
        },
        Kind::DeleteEdge(e) => Mutation::DeleteEdge { id: EdgeId(e.id), expected_version: e.expected_version },
        Kind::SetAttr(s) => Mutation::SetAttr {
            target: target_from_pb(s.target)?,
            value: value_field(s.value, &format!("value of '{}'", s.key))?,
            key: s.key,
            expected_version: s.expected_version,
        },
        Kind::RemoveAttr(r) => {
            Mutation::RemoveAttr { target: target_from_pb(r.target)?, key: r.key, expected_version: r.expected_version }
        }
        Kind::AppendAttr(a) => Mutation::AppendAttr {
            target: target_from_pb(a.target)?,
            value: value_field(a.value, &format!("value of '{}'", a.key))?,
            key: a.key,
            expected_version: a.expected_version,
        },
        Kind::AddLabel(l) => Mutation::AddLabel { id: l.id, label: l.label, expected_version: l.expected_version },
        Kind::RemoveLabel(l) => {
            Mutation::RemoveLabel { id: l.id, label: l.label, expected_version: l.expected_version }
        }
        Kind::SetEdgeType(t) => {
            Mutation::SetEdgeType { id: EdgeId(t.id), ty: t.r#type, expected_version: t.expected_version }
        }
    })
}

pub(crate) fn mutations_to_pb(mutations: &[Mutation]) -> Result<Vec<pb::Mutation>, Error> {
    mutations.iter().map(mutation_to_pb).collect()
}

pub(crate) fn mutations_from_pb(mutations: Vec<pb::Mutation>) -> Result<Vec<Mutation>, Error> {
    mutations.into_iter().map(mutation_from_pb).collect()
}

fn constraint_to_pb(c: &Constraint) -> pb::Constraint {
    let kind = match c.kind {
        ConstraintKind::Unique => pb::ConstraintKind::Unique,
        ConstraintKind::Required => pb::ConstraintKind::Required,
    };
    pb::Constraint { kind: kind.into(), label: c.label.as_str().to_owned(), path: Some(path_to_pb(c.path.keys())) }
}

fn constraint_from_pb(c: Option<pb::Constraint>) -> Result<Constraint, Error> {
    let c = c.ok_or_else(|| missing("the constraint"))?;
    let kind = match pb::ConstraintKind::try_from(c.kind) {
        Ok(pb::ConstraintKind::Unique) => ConstraintKind::Unique,
        Ok(pb::ConstraintKind::Required) => ConstraintKind::Required,
        Ok(pb::ConstraintKind::Unspecified) => return Err(missing("the constraint's kind")),
        Err(_) => return Err(Error::invalid(format!("unknown constraint kind {}", c.kind))),
    };
    Ok(Constraint {
        kind,
        label: Label::new(c.label).map_err(|e| Error::invalid(e.to_string()))?,
        path: attr_path_from_pb(c.path)?,
    })
}

fn index_to_pb(index: &IndexDef) -> pb::IndexDef {
    pb::IndexDef { path: Some(path_to_pb(index.path.keys())) }
}

fn index_from_pb(index: Option<pb::IndexDef>) -> Result<IndexDef, Error> {
    Ok(IndexDef { path: attr_path_from_pb(index.ok_or_else(|| missing("the index"))?.path)? })
}

pub(crate) fn catalog_change_to_pb(change: &CatalogChange) -> pb::CatalogChange {
    use pb::catalog_change::Kind;
    let kind = match change {
        CatalogChange::CreateIndex(i) => Kind::CreateIndex(index_to_pb(i)),
        CatalogChange::DropIndex(i) => Kind::DropIndex(index_to_pb(i)),
        CatalogChange::AddConstraint(c) => Kind::AddConstraint(constraint_to_pb(c)),
        CatalogChange::DropConstraint(c) => Kind::DropConstraint(constraint_to_pb(c)),
    };
    pb::CatalogChange { kind: Some(kind) }
}

pub(crate) fn catalog_change_from_pb(change: Option<pb::CatalogChange>) -> Result<CatalogChange, Error> {
    use pb::catalog_change::Kind;
    Ok(match change.and_then(|c| c.kind).ok_or_else(|| missing("the catalog change"))? {
        Kind::CreateIndex(i) => CatalogChange::CreateIndex(index_from_pb(Some(i))?),
        Kind::DropIndex(i) => CatalogChange::DropIndex(index_from_pb(Some(i))?),
        Kind::AddConstraint(c) => CatalogChange::AddConstraint(constraint_from_pb(Some(c))?),
        Kind::DropConstraint(c) => CatalogChange::DropConstraint(constraint_from_pb(Some(c))?),
    })
}

fn key_from_pb(key: Option<String>) -> Result<Option<IdempotencyKey>, Error> {
    Ok(key.map(IdempotencyKey::new).transpose()?)
}

fn key_to_pb(key: &Option<IdempotencyKey>) -> Option<String> {
    key.as_ref().map(|k| k.as_str().to_owned())
}

pub(crate) fn commit_options_to_pb(options: &CommitOptions) -> pb::CommitOptions {
    pb::CommitOptions { idempotency_key: key_to_pb(&options.idempotency_key) }
}

pub(crate) fn commit_options_from_pb(options: Option<pb::CommitOptions>) -> Result<CommitOptions, Error> {
    Ok(CommitOptions { idempotency_key: key_from_pb(options.and_then(|o| o.idempotency_key))? })
}

pub(crate) fn idempotency_key_to_pb(key: &Option<IdempotencyKey>) -> Option<String> {
    key_to_pb(key)
}

pub(crate) fn idempotency_key_from_pb(key: Option<String>) -> Result<Option<IdempotencyKey>, Error> {
    key_from_pb(key)
}

pub(crate) fn commit_result_to_pb(result: &CommitResult) -> pb::CommitResult {
    pb::CommitResult {
        seq: result.seq,
        edge_ids: result.edge_ids.iter().map(|e| e.0).collect(),
        versions: result
            .versions
            .iter()
            .map(|(target, version)| pb::Version { target: Some(target_to_pb(target)), version: *version })
            .collect(),
        time_micros: result.time.map(|t| t.0),
        deduplicated: result.deduplicated,
    }
}

pub(crate) fn commit_result_from_pb(result: Option<pb::CommitResult>) -> Result<CommitResult, Error> {
    let r = result.ok_or_else(|| missing("the commit result"))?;
    Ok(CommitResult {
        seq: r.seq,
        edge_ids: r.edge_ids.into_iter().map(EdgeId).collect(),
        versions: r
            .versions
            .into_iter()
            .map(|v| Ok((target_from_pb(v.target)?, v.version)))
            .collect::<Result<_, Error>>()?,
        time: r.time_micros.map(time_from_pb),
        deduplicated: r.deduplicated,
    })
}

// ---- read requests ----

pub(crate) fn find_to_pb(request: &FindRequest) -> Result<pb::Expr, Error> {
    expr_to_pb(&request.filter)
}

pub(crate) fn find_from_pb(filter: Option<pb::Expr>) -> Result<FindRequest, Error> {
    Ok(FindRequest { filter: expr_from_pb(filter.ok_or_else(|| missing("the filter"))?, "filter")? })
}

pub(crate) fn explain_from_pb(filter: Option<pb::Expr>, analyze: bool) -> Result<ExplainRequest, Error> {
    Ok(ExplainRequest { filter: find_from_pb(filter)?.filter, analyze })
}

pub(crate) fn neighbourhood_to_pb(
    namespace: &str,
    r: &NeighbourhoodRequest,
    options: &QueryOptions,
) -> Result<pb::NeighbourhoodRequest, Error> {
    Ok(pb::NeighbourhoodRequest {
        namespace: namespace.to_owned(),
        seeds: r.seeds.clone(),
        depth: wide(r.depth),
        direction: direction_to_pb(r.direction),
        edge_types: r.edge_types.clone(),
        edge_filter: opt_expr_to_pb(&r.edge_filter)?,
        node_filter: opt_expr_to_pb(&r.node_filter)?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn neighbourhood_from_pb(r: pb::NeighbourhoodRequest) -> Result<NeighbourhoodRequest, Error> {
    Ok(NeighbourhoodRequest {
        seeds: r.seeds,
        depth: size(r.depth),
        direction: direction_from_pb(r.direction)?,
        edge_types: r.edge_types,
        edge_filter: opt_expr_from_pb(r.edge_filter, "edge filter")?,
        node_filter: opt_expr_from_pb(r.node_filter, "node filter")?,
    })
}

pub(crate) fn traverse_to_pb(
    namespace: &str,
    r: &TraverseRequest,
    options: &QueryOptions,
) -> Result<pb::TraverseRequest, Error> {
    let order = match r.order {
        Order::Bfs => pb::Order::Bfs,
        Order::Dfs => pb::Order::Dfs,
    };
    Ok(pb::TraverseRequest {
        namespace: namespace.to_owned(),
        start: r.start.clone(),
        order: order.into(),
        depth: r.depth.map(wide),
        direction: direction_to_pb(r.direction),
        edge_types: r.edge_types.clone(),
        edge_filter: opt_expr_to_pb(&r.edge_filter)?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn traverse_from_pb(r: pb::TraverseRequest) -> Result<TraverseRequest, Error> {
    let order = match pb::Order::try_from(r.order) {
        Ok(pb::Order::Unspecified | pb::Order::Bfs) => Order::Bfs,
        Ok(pb::Order::Dfs) => Order::Dfs,
        Err(_) => return Err(Error::invalid(format!("unknown order {}", r.order))),
    };
    Ok(TraverseRequest {
        start: r.start,
        order,
        depth: r.depth.map(size),
        direction: direction_from_pb(r.direction)?,
        edge_types: r.edge_types,
        edge_filter: opt_expr_from_pb(r.edge_filter, "edge filter")?,
    })
}

fn coords_to_pb(coords: &Coords) -> pb::Coords {
    let kind = match coords {
        Coords::Sequence(path) => pb::coords::Kind::Sequence(path_to_pb(path)),
        Coords::PerDimension(paths) => pb::coords::Kind::PerDimension(pb::CoordsPerDimension {
            paths: paths.iter().map(|p| path_to_pb(p)).collect(),
        }),
    };
    pb::Coords { kind: Some(kind) }
}

fn coords_from_pb(coords: Option<pb::Coords>) -> Coords {
    match coords.and_then(|c| c.kind) {
        None => Coords::default(),
        Some(pb::coords::Kind::Sequence(path)) => Coords::Sequence(path.keys),
        Some(pb::coords::Kind::PerDimension(d)) => Coords::PerDimension(d.paths.into_iter().map(|p| p.keys).collect()),
    }
}

pub(crate) fn path_request_to_pb(namespace: &str, r: &PathRequest, options: &QueryOptions) -> pb::ShortestPathRequest {
    use pb::path_method::Kind;
    let method = match &r.method {
        PathMethod::Bfs => Kind::Bfs(pb::PathBfs {}),
        PathMethod::Dijkstra => Kind::Dijkstra(pb::PathDijkstra {}),
        PathMethod::AStar { coords, metric } => {
            let metric = match metric {
                Metric::Euclidean => pb::Metric::Euclidean,
                Metric::Manhattan => pb::Metric::Manhattan,
            };
            Kind::AStar(pb::PathAStar { coords: Some(coords_to_pb(coords)), metric: metric.into() })
        }
    };
    pb::ShortestPathRequest {
        namespace: namespace.to_owned(),
        from: r.from.clone(),
        to: r.to.clone(),
        method: Some(pb::PathMethod { kind: Some(method) }),
        cost: Some(cost_to_pb(&r.cost)),
        direction: direction_to_pb(r.direction),
        max_depth: r.max_depth.map(wide),
        max_cost: r.max_cost,
        options: Some(options_to_pb(options)),
    }
}

pub(crate) fn path_request_from_pb(r: pb::ShortestPathRequest) -> Result<PathRequest, Error> {
    use pb::path_method::Kind;
    let method = match r.method.and_then(|m| m.kind) {
        None | Some(Kind::Bfs(_)) => PathMethod::Bfs,
        Some(Kind::Dijkstra(_)) => PathMethod::Dijkstra,
        Some(Kind::AStar(a)) => {
            let metric = match pb::Metric::try_from(a.metric) {
                Ok(pb::Metric::Unspecified | pb::Metric::Euclidean) => Metric::Euclidean,
                Ok(pb::Metric::Manhattan) => Metric::Manhattan,
                Err(_) => return Err(Error::invalid(format!("unknown metric {}", a.metric))),
            };
            PathMethod::AStar { coords: coords_from_pb(a.coords), metric }
        }
    };
    Ok(PathRequest {
        from: r.from,
        to: r.to,
        method,
        cost: cost_from_pb(r.cost),
        direction: direction_from_pb(r.direction)?,
        max_depth: r.max_depth.map(size),
        max_cost: r.max_cost,
    })
}

pub(crate) fn walk_to_pb(namespace: &str, r: &WalkRequest, options: &QueryOptions) -> pb::RandomWalksRequest {
    pb::RandomWalksRequest {
        namespace: namespace.to_owned(),
        start: r.start.clone(),
        max_length: wide(r.max_length),
        walks: wide(r.walks),
        min_length: wide(r.min_length),
        allow_revisit: r.allow_revisit,
        seed: r.seed,
        options: Some(options_to_pb(options)),
    }
}

pub(crate) fn walk_from_pb(r: pb::RandomWalksRequest) -> WalkRequest {
    WalkRequest {
        start: r.start,
        max_length: size(r.max_length),
        walks: size(r.walks),
        min_length: size(r.min_length),
        allow_revisit: r.allow_revisit,
        seed: r.seed,
    }
}

pub(crate) fn subgraph_to_pb(
    namespace: &str,
    r: &SubgraphRequest,
    options: &QueryOptions,
) -> Result<pb::SubgraphRequest, Error> {
    Ok(pb::SubgraphRequest {
        namespace: namespace.to_owned(),
        seeds: r.seeds.clone(),
        depth: wide(r.depth),
        direction: direction_to_pb(r.direction),
        edge_types: r.edge_types.clone(),
        edge_filter: opt_expr_to_pb(&r.edge_filter)?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn subgraph_from_pb(r: pb::SubgraphRequest) -> Result<SubgraphRequest, Error> {
    Ok(SubgraphRequest {
        seeds: r.seeds,
        depth: size(r.depth),
        direction: direction_from_pb(r.direction)?,
        edge_types: r.edge_types,
        edge_filter: opt_expr_from_pb(r.edge_filter, "edge filter")?,
    })
}

pub(crate) fn match_to_pb(
    namespace: &str,
    r: &MatchRequest,
    options: &QueryOptions,
) -> Result<pb::MatchPatternRequest, Error> {
    Ok(pb::MatchPatternRequest {
        namespace: namespace.to_owned(),
        pattern: Some(pattern_to_pb(&r.pattern)?),
        filters: r
            .filters
            .iter()
            .map(|(name, filter)| Ok(pb::VariableFilter { name: name.clone(), filter: Some(expr_to_pb(filter)?) }))
            .collect::<Result<_, Error>>()?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn match_from_pb(r: pb::MatchPatternRequest) -> Result<MatchRequest, Error> {
    let filters = r
        .filters
        .into_iter()
        .map(|f| {
            let filter = f.filter.ok_or_else(|| missing(&format!("the filter of '{}'", f.name)))?;
            Ok((f.name, expr_from_pb(filter, "filter")?))
        })
        .collect::<Result<_, Error>>()?;
    Ok(MatchRequest { pattern: pattern_from_pb(r.pattern)?, filters })
}

fn job_to_pb(job: &Job) -> Result<pb::Job, Error> {
    use pb::job::Kind;
    let kind = match job {
        Job::PageRank(p) => {
            if p.personalization.is_some() {
                return Err(Error::invalid("PageRank's personalization (by dense index) can't be sent to a server"));
            }
            Kind::PageRank(pb::PageRankJob { alpha: Some(p.alpha), max_iter: Some(wide(p.max_iter)), tol: Some(p.tol) })
        }
        Job::Degree { incoming } => Kind::Degree(pb::DegreeJob { incoming: *incoming }),
        Job::WeaklyConnectedComponents => Kind::WeaklyConnectedComponents(pb::WeaklyConnectedComponentsJob {}),
        Job::StronglyConnectedComponents => Kind::StronglyConnectedComponents(pb::StronglyConnectedComponentsJob {}),
        Job::Leiden(l) => Kind::Leiden(pb::LeidenJob {
            resolution: Some(l.resolution),
            randomness: Some(l.randomness),
            max_iter: Some(wide(l.max_iter)),
            seed: Some(l.seed),
        }),
        Job::LabelPropagation { max_iter } => {
            Kind::LabelPropagation(pb::LabelPropagationJob { max_iter: wide(*max_iter) })
        }
        Job::CoreNumber => Kind::CoreNumber(pb::CoreNumberJob {}),
        Job::Triangles => Kind::Triangles(pb::TrianglesJob {}),
    };
    Ok(pb::Job { kind: Some(kind) })
}

fn job_from_pb(job: Option<pb::Job>) -> Result<Job, Error> {
    use pb::job::Kind;
    Ok(match job.and_then(|j| j.kind).ok_or_else(|| missing("the job"))? {
        Kind::PageRank(p) => {
            let d = PageRank::default();
            Job::PageRank(PageRank {
                alpha: p.alpha.unwrap_or(d.alpha),
                personalization: None,
                max_iter: p.max_iter.map_or(d.max_iter, size),
                tol: p.tol.unwrap_or(d.tol),
            })
        }
        Kind::Degree(d) => Job::Degree { incoming: d.incoming },
        Kind::WeaklyConnectedComponents(_) => Job::WeaklyConnectedComponents,
        Kind::StronglyConnectedComponents(_) => Job::StronglyConnectedComponents,
        Kind::Leiden(l) => {
            let d = Leiden::default();
            Job::Leiden(Leiden {
                resolution: l.resolution.unwrap_or(d.resolution),
                randomness: l.randomness.unwrap_or(d.randomness),
                max_iter: l.max_iter.map_or(d.max_iter, size),
                seed: l.seed.unwrap_or(d.seed),
            })
        }
        Kind::LabelPropagation(l) => Job::LabelPropagation { max_iter: size(l.max_iter) },
        Kind::CoreNumber(_) => Job::CoreNumber,
        Kind::Triangles(_) => Job::Triangles,
    })
}

pub(crate) fn analyze_to_pb(
    namespace: &str,
    r: &AnalyticsRequest,
    options: &QueryOptions,
) -> Result<pb::AnalyzeRequest, Error> {
    Ok(pb::AnalyzeRequest {
        namespace: namespace.to_owned(),
        projection: Some(pb::Projection {
            direction: direction_to_pb(r.projection.direction),
            cost: Some(cost_to_pb(&r.projection.cost)),
        }),
        job: Some(job_to_pb(&r.job)?),
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn analyze_from_pb(r: pb::AnalyzeRequest) -> Result<AnalyticsRequest, Error> {
    let p = r.projection.unwrap_or_default();
    Ok(AnalyticsRequest {
        projection: ProjectionSpec { direction: direction_from_pb(p.direction)?, cost: cost_from_pb(p.cost) },
        job: job_from_pb(r.job)?,
    })
}

// ---- read answers ----

fn plan_to_pb(plan: &Plan) -> pb::Plan {
    use pb::plan::Kind;
    let kind = match plan {
        Plan::Empty => Kind::Empty(pb::PlanEmpty {}),
        Plan::Label { label } => Kind::Label(pb::PlanLabel { label: label.clone() }),
        Plan::Index { path, lookup } => {
            let (lookup, in_values) = match lookup {
                Lookup::Point => (pb::Lookup::Point, 0),
                Lookup::In { values } => (pb::Lookup::In, wide(*values)),
                Lookup::Range => (pb::Lookup::Range, 0),
            };
            Kind::Index(pb::PlanIndex { path: Some(path_to_pb(path)), lookup: lookup.into(), in_values })
        }
        Plan::Union(plans) => Kind::Union(pb::PlanUnion { plans: plans.iter().map(plan_to_pb).collect() }),
        Plan::Scan => Kind::Scan(pb::PlanScan {}),
        Plan::Other(text) => Kind::Other(text.clone()),
    };
    pb::Plan { kind: Some(kind) }
}

fn plan_from_pb(plan: Option<pb::Plan>) -> Result<Plan, Error> {
    use pb::plan::Kind;
    Ok(match plan.and_then(|p| p.kind).ok_or_else(|| missing("the plan"))? {
        Kind::Empty(_) => Plan::Empty,
        Kind::Label(l) => Plan::Label { label: l.label },
        Kind::Index(i) => {
            let lookup = match pb::Lookup::try_from(i.lookup) {
                Ok(pb::Lookup::Point) => Lookup::Point,
                Ok(pb::Lookup::In) => Lookup::In { values: size(i.in_values) },
                Ok(pb::Lookup::Range) => Lookup::Range,
                _ => return Err(Error::invalid(format!("unknown lookup {}", i.lookup))),
            };
            Plan::Index { path: keys_from_pb(i.path), lookup }
        }
        Kind::Union(u) => Plan::Union(u.plans.into_iter().map(|p| plan_from_pb(Some(p))).collect::<Result<_, _>>()?),
        Kind::Scan(_) => Plan::Scan,
        Kind::Other(text) => Plan::Other(text),
    })
}

pub(crate) fn explain_to_pb(e: &Explain) -> pb::Explain {
    pb::Explain {
        plan: Some(plan_to_pb(&e.plan)),
        estimated_candidates: wide(e.estimated_candidates),
        candidates: e.candidates.map(wide),
        nodes: wide(e.nodes),
        building: e.building.iter().map(|p| path_to_pb(p)).collect(),
    }
}

pub(crate) fn explain_answer_from_pb(e: Option<pb::Explain>) -> Result<Explain, Error> {
    let e = e.ok_or_else(|| missing("the explanation"))?;
    Ok(Explain {
        plan: plan_from_pb(e.plan)?,
        estimated_candidates: size(e.estimated_candidates),
        candidates: e.candidates.map(size),
        nodes: size(e.nodes),
        building: e.building.into_iter().map(|p| p.keys).collect(),
    })
}

pub(crate) fn path_to_answer_pb(path: &Path) -> pb::Path {
    pb::Path { nodes: path.nodes.clone(), cost: path.cost }
}

pub(crate) fn path_from_answer_pb(path: pb::Path) -> Path {
    Path { nodes: path.nodes, cost: path.cost }
}

pub(crate) fn walks_to_pb(walks: &[Vec<String>]) -> Vec<pb::Walk> {
    walks.iter().map(|w| pb::Walk { nodes: w.clone() }).collect()
}

pub(crate) fn match_row_to_pb(row: &MatchRow) -> pb::MatchRow {
    pb::MatchRow {
        nodes: row.nodes.clone(),
        edges: row.edges.iter().map(|ids| pb::EdgePath { ids: ids.iter().map(|e| e.0).collect() }).collect(),
    }
}

pub(crate) fn match_row_from_pb(row: pb::MatchRow) -> MatchRow {
    MatchRow {
        nodes: row.nodes,
        edges: row.edges.into_iter().map(|p| p.ids.into_iter().map(EdgeId).collect()).collect(),
    }
}

/// A job result's rows, as the three row lists and their kind.
pub(crate) struct JobRows {
    pub kind: pb::JobResultKind,
    pub scores: Vec<pb::Score>,
    pub groups: Vec<pb::Group>,
    pub counts: Vec<pb::Count>,
}

pub(crate) fn job_result_to_pb(result: &JobResult) -> JobRows {
    let mut rows = JobRows { kind: pb::JobResultKind::Unspecified, scores: vec![], groups: vec![], counts: vec![] };
    match result {
        JobResult::Scores(s) => {
            rows.kind = pb::JobResultKind::Scores;
            rows.scores = s.iter().map(|(id, score)| pb::Score { id: id.clone(), score: *score }).collect();
        }
        JobResult::Groups(g) => {
            rows.kind = pb::JobResultKind::Groups;
            rows.groups = g.iter().map(|ids| pb::Group { ids: ids.clone() }).collect();
        }
        JobResult::Counts(c) => {
            rows.kind = pb::JobResultKind::Counts;
            rows.counts = c.iter().map(|(id, count)| pb::Count { id: id.clone(), count: *count }).collect();
        }
    }
    rows
}

pub(crate) fn job_result_from_pb(rows: JobRows) -> Result<JobResult, Error> {
    Ok(match rows.kind {
        pb::JobResultKind::Scores => JobResult::Scores(rows.scores.into_iter().map(|s| (s.id, s.score)).collect()),
        pb::JobResultKind::Groups => JobResult::Groups(rows.groups.into_iter().map(|g| g.ids).collect()),
        pb::JobResultKind::Counts => JobResult::Counts(rows.counts.into_iter().map(|c| (c.id, c.count)).collect()),
        pb::JobResultKind::Unspecified => return Err(missing("the kind of the job result")),
    })
}

pub(crate) fn subgraph_answer_from_pb(nodes: Vec<pb::Node>, edges: Vec<pb::Edge>) -> Result<Subgraph, Error> {
    Ok(Subgraph { nodes: nodes_from_pb(nodes)?, edges: edges_from_pb(edges)? })
}

// ---- catalog, status, namespaces ----

pub(crate) fn catalog_to_pb(catalog: &NamespaceCatalog) -> pb::NamespaceCatalog {
    pb::NamespaceCatalog {
        indexes: catalog.indexes().map(index_to_pb).collect(),
        constraints: catalog.constraints().map(constraint_to_pb).collect(),
    }
}

pub(crate) fn catalog_from_pb(catalog: Option<pb::NamespaceCatalog>) -> Result<NamespaceCatalog, Error> {
    let c = catalog.ok_or_else(|| missing("the catalog"))?;
    let mut out = NamespaceCatalog::new();
    for index in c.indexes {
        out.add_index(index_from_pb(Some(index))?);
    }
    for constraint in c.constraints {
        out.add_constraint(constraint_from_pb(Some(constraint))?);
    }
    Ok(out)
}

fn index_status_to_pb(i: &IndexStatus) -> pb::IndexStatus {
    let state = match i.state {
        IndexState::Ready => pb::index_status::State::Ready(pb::IndexReady {}),
        IndexState::Building { scanned, total } => {
            pb::index_status::State::Building(pb::IndexBuilding { scanned: wide(scanned), total: wide(total) })
        }
    };
    pb::IndexStatus {
        path: Some(path_to_pb(i.path.keys())),
        state: Some(state),
        declared: i.declared,
        unique: i.unique,
        size: i.size.map(|s| pb::IndexSize {
            entries: wide(s.entries),
            distinct_keys: wide(s.distinct_keys),
            memory_bytes: wide(s.memory_bytes),
        }),
    }
}

fn index_status_from_pb(i: pb::IndexStatus) -> Result<IndexStatus, Error> {
    let state = match i.state.ok_or_else(|| missing("the index state"))? {
        pb::index_status::State::Ready(_) => IndexState::Ready,
        pb::index_status::State::Building(b) => IndexState::Building { scanned: size(b.scanned), total: size(b.total) },
    };
    Ok(IndexStatus {
        path: attr_path_from_pb(i.path)?,
        state,
        declared: i.declared,
        unique: i.unique,
        size: i.size.map(|s| IndexSize {
            entries: size(s.entries),
            distinct_keys: size(s.distinct_keys),
            memory_bytes: size(s.memory_bytes),
        }),
    })
}

fn damage_to_pb(damage: Damage) -> pb::Damage {
    match damage {
        Damage::Truncated => pb::Damage::Truncated,
        Damage::BadHeader => pb::Damage::BadHeader,
        Damage::BadLength => pb::Damage::BadLength,
        Damage::Checksum => pb::Damage::Checksum,
    }
}

fn damage_from_pb(damage: i32) -> Result<Damage, Error> {
    Ok(match pb::Damage::try_from(damage) {
        Ok(pb::Damage::Truncated) => Damage::Truncated,
        Ok(pb::Damage::BadHeader) => Damage::BadHeader,
        Ok(pb::Damage::BadLength) => Damage::BadLength,
        Ok(pb::Damage::Checksum) => Damage::Checksum,
        _ => return Err(Error::invalid(format!("unknown damage {}", damage))),
    })
}

fn recovery_to_pb(r: &RecoveryReport) -> pb::RecoveryReport {
    pb::RecoveryReport {
        checkpoint: r.checkpoint,
        skipped_checkpoints: r
            .skipped_checkpoints
            .iter()
            .map(|s| pb::SkippedCheckpoint { seq: s.seq, path: s.path.display().to_string(), reason: s.reason.clone() })
            .collect(),
        created_indexes: r.index_changes.created.iter().map(|p| path_to_pb(p.keys())).collect(),
        dropped_indexes: r.index_changes.dropped.iter().map(|p| path_to_pb(p)).collect(),
        replayed: r.replayed,
        torn_tail: r.torn_tail.as_ref().map(|t| pb::CutTail {
            path: t.path.display().to_string(),
            file_len: t.file_len,
            valid_len: t.valid_len,
            damage: damage_to_pb(t.damage).into(),
            discarded_frames: t.discarded_frames,
            removed: t.removed,
        }),
        seq: r.seq,
    }
}

fn recovery_from_pb(r: Option<pb::RecoveryReport>) -> Result<RecoveryReport, Error> {
    let r = r.unwrap_or_default();
    Ok(RecoveryReport {
        checkpoint: r.checkpoint,
        skipped_checkpoints: r
            .skipped_checkpoints
            .into_iter()
            .map(|s| SkippedCheckpoint { seq: s.seq, path: PathBuf::from(s.path), reason: s.reason })
            .collect(),
        index_changes: IndexChanges {
            created: r.created_indexes.into_iter().map(|p| attr_path_from_pb(Some(p))).collect::<Result<_, _>>()?,
            dropped: r.dropped_indexes.into_iter().map(|p| p.keys).collect(),
        },
        replayed: r.replayed,
        torn_tail: r
            .torn_tail
            .map(|t| {
                Ok::<_, Error>(CutTail {
                    path: PathBuf::from(t.path),
                    file_len: t.file_len,
                    valid_len: t.valid_len,
                    damage: damage_from_pb(t.damage)?,
                    discarded_frames: t.discarded_frames,
                    removed: t.removed,
                })
            })
            .transpose()?,
        seq: r.seq,
    })
}

pub(crate) fn status_to_pb(s: &NamespaceStatus) -> pb::NamespaceStatus {
    pb::NamespaceStatus {
        id: s.id,
        name: s.name.clone(),
        created_micros: s.created.0,
        seq: s.seq,
        synced_seq: s.synced_seq,
        checkpoint: s.checkpoint,
        read_only: s.read_only.clone(),
        checkpoint_failure: s.checkpoint_failure.clone(),
        nodes: wide(s.nodes),
        edges: wide(s.edges),
        memory_bytes: wide(s.memory_bytes),
        indexes: s.indexes.iter().map(index_status_to_pb).collect(),
        constraints: wide(s.constraints),
        recovery: Some(recovery_to_pb(&s.recovery)),
    }
}

pub(crate) fn status_from_pb(s: Option<pb::NamespaceStatus>) -> Result<NamespaceStatus, Error> {
    let s = s.ok_or_else(|| missing("the namespace status"))?;
    Ok(NamespaceStatus {
        id: s.id,
        name: s.name,
        created: time_from_pb(s.created_micros),
        seq: s.seq,
        synced_seq: s.synced_seq,
        checkpoint: s.checkpoint,
        read_only: s.read_only,
        checkpoint_failure: s.checkpoint_failure,
        nodes: size(s.nodes),
        edges: size(s.edges),
        memory_bytes: size(s.memory_bytes),
        indexes: s.indexes.into_iter().map(index_status_from_pb).collect::<Result<_, _>>()?,
        constraints: size(s.constraints),
        recovery: recovery_from_pb(s.recovery)?,
    })
}

fn name_from_pb(name: String) -> Result<NamespaceName, Error> {
    NamespaceName::new(name).map_err(|e| Error::invalid(e.to_string()))
}

pub(crate) fn namespace_info_to_pb(i: &NamespaceInfo) -> pb::NamespaceInfo {
    pb::NamespaceInfo {
        id: i.id,
        name: i.name.as_str().to_owned(),
        created_micros: i.created.0,
        created_seq: i.created_seq,
    }
}

pub(crate) fn namespace_info_from_pb(i: pb::NamespaceInfo) -> Result<NamespaceInfo, Error> {
    Ok(NamespaceInfo {
        id: i.id,
        name: name_from_pb(i.name)?,
        created: time_from_pb(i.created_micros),
        created_seq: i.created_seq,
    })
}

pub(crate) fn event_to_pb(e: &Event) -> pb::NamespaceEvent {
    let kind = match e.kind {
        EventKind::Create => pb::NamespaceEventKind::Create,
        EventKind::Drop => pb::NamespaceEventKind::Drop,
    };
    pb::NamespaceEvent {
        seq: e.seq,
        time_micros: e.time.0,
        kind: kind.into(),
        id: e.id,
        name: e.name.as_str().to_owned(),
        idempotency_key: e.keyed.as_ref().map(|(k, _)| k.as_str().to_owned()),
        fingerprint: e.keyed.as_ref().map_or(0, |(_, f)| *f),
    }
}

pub(crate) fn namespace_result_from_pb(
    event: Option<pb::NamespaceEvent>,
    deduplicated: bool,
) -> Result<NamespaceResult, Error> {
    let e = event.ok_or_else(|| missing("the namespace event"))?;
    let kind = match pb::NamespaceEventKind::try_from(e.kind) {
        Ok(pb::NamespaceEventKind::Create) => EventKind::Create,
        Ok(pb::NamespaceEventKind::Drop) => EventKind::Drop,
        _ => return Err(Error::invalid(format!("unknown namespace event kind {}", e.kind))),
    };
    let keyed = key_from_pb(e.idempotency_key)?.map(|k| (k, e.fingerprint));
    Ok(NamespaceResult {
        event: Event {
            seq: e.seq,
            time: time_from_pb(e.time_micros),
            kind,
            id: e.id,
            name: name_from_pb(e.name)?,
            keyed,
        },
        deduplicated,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn nest(depth: usize, leaf: Value) -> Value {
        (0..depth).fold(leaf, |v, _| Value::List(vec![v]))
    }

    /// A filter of `depth` levels (`Expr::depth`): `Not`s around a `Const`.
    fn nest_expr(depth: usize) -> Expr {
        let e = (1..depth).fold(Expr::Const(true), |e, _| Expr::Not(Box::new(e)));
        assert_eq!(e.depth(), depth);
        e
    }

    #[test]
    fn values_nested_100_levels_pass_and_101_fail_with_the_cores_message() {
        let ok = nest(99, Value::Int(1));
        assert_eq!(value_from_pb(value_to_pb(&ok).unwrap(), "value").unwrap(), ok);
        // Encoding refuses a value the core refuses
        let deep = nest(100, Value::Int(1));
        let e = value_to_pb(&deep).unwrap_err();
        assert!(e.message().contains("nested more than 100 levels"), "{}", e);
        // So does decoding bytes made without the limit: a list of one item
        // is the variant index, a length of 1, and the item
        let list = postcard::to_stdvec(&Value::List(vec![])).unwrap()[0];
        let mut bytes = Vec::new();
        for _ in 0..100 {
            bytes.extend_from_slice(&[list, 1]);
        }
        bytes.extend_from_slice(&postcard::to_stdvec(&Value::Int(1)).unwrap());
        let e = value_from_pb(pb::Value { form: Some(pb::value::Form::Postcard(bytes)) }, "value").unwrap_err();
        assert_eq!(e.code(), iwdb_query::Code::InvalidArgument);
        assert!(e.message().contains("nested more than 100 levels"), "#29: the core's message, got: {}", e);
    }

    #[test]
    fn filters_nested_100_levels_pass_and_101_fail_with_the_cores_message() {
        let ok = nest_expr(100);
        assert_eq!(find_from_pb(Some(expr_to_pb(&ok).unwrap())).unwrap().filter, ok);
        let e = expr_to_pb(&nest_expr(101)).unwrap_err();
        assert!(e.message().contains("expression nested more than 100 levels"), "{}", e);
    }

    #[test]
    fn a_stale_message_of_an_earlier_failure_is_not_reported() {
        assert!(expr_to_pb(&nest_expr(101)).is_err());
        let e = value_from_pb(pb::Value { form: Some(pb::value::Form::Postcard(vec![0xff, 0xff])) }, "value");
        assert!(!e.unwrap_err().message().contains("nested"));
    }

    #[test]
    fn garbage_and_trailing_bytes_are_invalid() {
        let mut bytes = postcard::to_stdvec(&Value::Int(1)).unwrap();
        bytes.push(0);
        let e = value_from_pb(pb::Value { form: Some(pb::value::Form::Postcard(bytes)) }, "value").unwrap_err();
        assert!(e.message().contains("1 bytes after"), "{}", e);
        assert_eq!(
            value_from_pb(pb::Value { form: None }, "value").unwrap_err().code(),
            iwdb_query::Code::InvalidArgument
        );
        assert!(pattern_from_pb(Some(pb::Pattern { form: Some(pb::pattern::Form::Text("(a)-[".into())) })).is_err());
    }

    #[test]
    fn patterns_go_as_text_when_the_text_can_express_them() {
        let text = MatchRequest::parse("(a:Person {age: 30})-[:knows*1..3]->(b)").unwrap();
        let pb = pattern_to_pb(&text.pattern).unwrap();
        assert!(matches!(pb.form, Some(pb::pattern::Form::Text(_))));
        assert_eq!(pattern_from_pb(Some(pb)).unwrap(), text.pattern);
        let mut bound = text.pattern.clone();
        bound.bind_ids("b", vec!["x".into()]).unwrap();
        let pb = pattern_to_pb(&bound).unwrap();
        assert!(matches!(pb.form, Some(pb::pattern::Form::Postcard(_))));
        assert_eq!(pattern_from_pb(Some(pb)).unwrap(), bound);
    }

    #[test]
    fn timeouts_take_the_smaller_and_round_up() {
        let o = |ms| Some(pb::QueryOptions { timeout_ms: ms, ..Default::default() });
        let ms = Duration::from_millis;
        assert_eq!(options_from_pb(o(Some(50)), Some(ms(20))).unwrap().timeout, Some(ms(20)));
        assert_eq!(options_from_pb(o(Some(10)), Some(ms(20))).unwrap().timeout, Some(ms(10)));
        assert_eq!(options_from_pb(o(None), Some(ms(20))).unwrap().timeout, Some(ms(20)));
        assert_eq!(options_from_pb(o(None), None).unwrap().timeout, None);
        assert_eq!(millis(Duration::ZERO), 0);
        assert_eq!(millis(Duration::from_micros(1)), 1);
        assert_eq!(millis(Duration::MAX), u32::MAX);
        let bad = Some(pb::QueryOptions { history: "nope".into(), ..Default::default() });
        assert_eq!(options_from_pb(bad, None).unwrap_err().code(), iwdb_query::Code::InvalidArgument);
    }
}
