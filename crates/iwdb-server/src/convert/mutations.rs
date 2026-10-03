//! Mutations, catalog changes and commits.

use ironweaver_core::EdgeId;
use iwdb_engine::catalog::{Constraint, ConstraintKind, IndexDef, Label};
use iwdb_engine::{CatalogChange, CommitResult, EdgeKey, IdempotencyKey, Mutation, Target};
use iwdb_query::{CommitOptions, Error};

use super::{
    attr_path_from_pb, attrs_from_pb, attrs_to_pb, missing, path_to_pb, time_from_pb, value_field, value_to_pb,
};
use crate::proto as pb;

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

pub(super) fn constraint_to_pb(c: &Constraint) -> pb::Constraint {
    let kind = match c.kind {
        ConstraintKind::Unique => pb::ConstraintKind::Unique,
        ConstraintKind::Required => pb::ConstraintKind::Required,
    };
    pb::Constraint { kind: kind.into(), label: c.label.as_str().to_owned(), path: Some(path_to_pb(c.path.keys())) }
}

pub(super) fn constraint_from_pb(c: Option<pb::Constraint>) -> Result<Constraint, Error> {
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

pub(super) fn index_to_pb(index: &IndexDef) -> pb::IndexDef {
    pb::IndexDef { path: Some(path_to_pb(index.path.keys())) }
}

pub(super) fn index_from_pb(index: Option<pb::IndexDef>) -> Result<IndexDef, Error> {
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

pub(crate) fn idempotency_key_from_pb(key: Option<String>) -> Result<Option<IdempotencyKey>, Error> {
    Ok(key.map(IdempotencyKey::new).transpose()?)
}

pub(crate) fn idempotency_key_to_pb(key: &Option<IdempotencyKey>) -> Option<String> {
    key.as_ref().map(|k| k.as_str().to_owned())
}

pub(crate) fn commit_options_to_pb(options: &CommitOptions) -> pb::CommitOptions {
    pb::CommitOptions { idempotency_key: idempotency_key_to_pb(&options.idempotency_key) }
}

pub(crate) fn commit_options_from_pb(options: Option<pb::CommitOptions>) -> Result<CommitOptions, Error> {
    Ok(CommitOptions { idempotency_key: idempotency_key_from_pb(options.and_then(|o| o.idempotency_key))? })
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
