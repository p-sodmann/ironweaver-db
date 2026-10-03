//! The change stream (`changes.proto`, ADR 0031): ops, events and batches.
//! The version op of ADR 0004 (`SetNodeAttr` / `SetEdgeAttr` on
//! `iwdb.version`) is `SetNodeVersion` / `SetEdgeVersion` on the wire.

use ironweaver_core::{EdgeId, Op, Value};
use iwdb_engine::reserved::VERSION_KEY;
use iwdb_engine::{Change, CommitTime, DbRecord};
use iwdb_query::{Answer, ChangeEvent, Changes, Error};

use super::{
    answer_from_pb, attrs_from_pb, attrs_to_pb, catalog_change_from_pb, catalog_change_to_pb, idempotency_key_from_pb,
    idempotency_key_to_pb, meta_to_pb, missing, value_from_pb, value_to_pb,
};
use crate::proto as pb;
use crate::proto::change_op::Kind;

type DbOp = Op<DbRecord, DbRecord>;

fn record_to_pb(r: &DbRecord) -> Result<pb::RecordData, Error> {
    Ok(pb::RecordData { attr: attrs_to_pb(&r.attr)?, meta: attrs_to_pb(&r.meta)?, version: r.version })
}

fn record_from_pb(r: Option<pb::RecordData>, what: &str) -> Result<DbRecord, Error> {
    let r = r.ok_or_else(|| missing(what))?;
    Ok(DbRecord { attr: attrs_from_pb(r.attr)?, meta: attrs_from_pb(r.meta)?, version: r.version })
}

fn opt_value_to_pb(value: &Option<Value>) -> Result<Option<pb::Value>, Error> {
    value.as_ref().map(value_to_pb).transpose()
}

fn opt_value_from_pb(value: Option<pb::Value>) -> Result<Option<Value>, Error> {
    value.map(|v| value_from_pb(v, "attribute value")).transpose()
}

/// A version op's version: the bit cast of ADR 0004.
fn version_op(key: &str, value: &Option<Value>) -> Option<u64> {
    match (key, value) {
        (VERSION_KEY, Some(Value::Int(v))) => Some(*v as u64),
        _ => None,
    }
}

fn version_value(version: u64) -> Option<Value> {
    Some(Value::Int(version as i64))
}

pub(crate) fn op_to_pb(op: &DbOp) -> Result<pb::ChangeOp, Error> {
    let kind = match op {
        Op::AddNode { id, labels, data } => {
            Kind::AddNode(pb::AddNodeOp { id: id.clone(), labels: labels.clone(), data: Some(record_to_pb(data)?) })
        }
        Op::RemoveNode { id } => Kind::RemoveNode(pb::RemoveNodeOp { id: id.clone() }),
        Op::RenameNode { id, new_id } => Kind::RenameNode(pb::RenameNodeOp { id: id.clone(), new_id: new_id.clone() }),
        Op::AddLabel { id, label } => Kind::AddLabel(pb::NodeLabelOp { id: id.clone(), label: label.clone() }),
        Op::RemoveLabel { id, label } => Kind::RemoveLabel(pb::NodeLabelOp { id: id.clone(), label: label.clone() }),
        Op::SetNode { id, data } => Kind::SetNode(pb::SetNodeOp { id: id.clone(), data: Some(record_to_pb(data)?) }),
        Op::SetNodeAttr { id, key, value } => match version_op(key, value) {
            Some(version) => Kind::SetNodeVersion(pb::SetNodeVersionOp { id: id.clone(), version }),
            None => Kind::SetNodeAttr(pb::SetNodeAttrOp {
                id: id.clone(),
                key: key.clone(),
                value: opt_value_to_pb(value)?,
            }),
        },
        Op::AddEdge { id, from, to, ty, data } => Kind::AddEdge(pb::AddEdgeOp {
            id: id.0,
            from: from.clone(),
            to: to.clone(),
            r#type: ty.clone(),
            data: Some(record_to_pb(data)?),
        }),
        Op::RemoveEdge { id } => Kind::RemoveEdge(pb::RemoveEdgeOp { id: id.0 }),
        Op::SetEdgeType { id, ty } => Kind::SetEdgeType(pb::SetEdgeTypeOp { id: id.0, r#type: ty.clone() }),
        Op::SetEdge { id, data } => Kind::SetEdge(pb::SetEdgeOp { id: id.0, data: Some(record_to_pb(data)?) }),
        Op::SetEdgeAttr { id, key, value } => match version_op(key, value) {
            Some(version) => Kind::SetEdgeVersion(pb::SetEdgeVersionOp { id: id.0, version }),
            None => Kind::SetEdgeAttr(pb::SetEdgeAttrOp { id: id.0, key: key.clone(), value: opt_value_to_pb(value)? }),
        },
    };
    Ok(pb::ChangeOp { kind: Some(kind) })
}

pub(crate) fn op_from_pb(op: pb::ChangeOp) -> Result<DbOp, Error> {
    Ok(match op.kind.ok_or_else(|| missing("an op's kind"))? {
        Kind::AddNode(o) => Op::AddNode { id: o.id, labels: o.labels, data: record_from_pb(o.data, "a node's data")? },
        Kind::RemoveNode(o) => Op::RemoveNode { id: o.id },
        Kind::RenameNode(o) => Op::RenameNode { id: o.id, new_id: o.new_id },
        Kind::AddLabel(o) => Op::AddLabel { id: o.id, label: o.label },
        Kind::RemoveLabel(o) => Op::RemoveLabel { id: o.id, label: o.label },
        Kind::SetNode(o) => Op::SetNode { id: o.id, data: record_from_pb(o.data, "a node's data")? },
        Kind::SetNodeAttr(o) => Op::SetNodeAttr { id: o.id, key: o.key, value: opt_value_from_pb(o.value)? },
        Kind::SetNodeVersion(o) => {
            Op::SetNodeAttr { id: o.id, key: VERSION_KEY.to_owned(), value: version_value(o.version) }
        }
        Kind::AddEdge(o) => Op::AddEdge {
            id: EdgeId(o.id),
            from: o.from,
            to: o.to,
            ty: o.r#type,
            data: record_from_pb(o.data, "an edge's data")?,
        },
        Kind::RemoveEdge(o) => Op::RemoveEdge { id: EdgeId(o.id) },
        Kind::SetEdgeType(o) => Op::SetEdgeType { id: EdgeId(o.id), ty: o.r#type },
        Kind::SetEdge(o) => Op::SetEdge { id: EdgeId(o.id), data: record_from_pb(o.data, "an edge's data")? },
        Kind::SetEdgeAttr(o) => Op::SetEdgeAttr { id: EdgeId(o.id), key: o.key, value: opt_value_from_pb(o.value)? },
        Kind::SetEdgeVersion(o) => {
            Op::SetEdgeAttr { id: EdgeId(o.id), key: VERSION_KEY.to_owned(), value: version_value(o.version) }
        }
    })
}

pub(crate) fn change_event_to_pb(e: &ChangeEvent) -> Result<pb::ChangeEvent, Error> {
    let change = match &e.change {
        Change::Data(ops) => {
            pb::change_event::Change::Data(pb::DataChange { ops: ops.iter().map(op_to_pb).collect::<Result<_, _>>()? })
        }
        Change::Catalog(change) => pb::change_event::Change::Catalog(catalog_change_to_pb(change)),
    };
    Ok(pb::ChangeEvent {
        seq: e.seq,
        time_micros: e.time.map(CommitTime::micros),
        idempotency_key: idempotency_key_to_pb(&e.key),
        change: Some(change),
    })
}

pub(crate) fn change_event_from_pb(e: pb::ChangeEvent) -> Result<ChangeEvent, Error> {
    let change = match e.change.ok_or_else(|| missing("a change event's change"))? {
        pb::change_event::Change::Data(data) => {
            Change::Data(data.ops.into_iter().map(op_from_pb).collect::<Result<_, _>>()?)
        }
        pb::change_event::Change::Catalog(change) => Change::Catalog(catalog_change_from_pb(Some(change))?),
    };
    Ok(ChangeEvent {
        seq: e.seq,
        time: e.time_micros.map(CommitTime),
        key: idempotency_key_from_pb(e.idempotency_key)?,
        change,
    })
}

pub(crate) fn changes_to_pb(a: &Answer<Changes>) -> Result<pb::GetChangesResponse, Error> {
    Ok(pb::GetChangesResponse {
        events: a.value.events.iter().map(change_event_to_pb).collect::<Result<_, _>>()?,
        next_seq: a.value.next_seq,
        first_seq: a.value.first_seq,
        meta: Some(meta_to_pb(a)),
    })
}

pub(crate) fn changes_from_pb(r: pb::GetChangesResponse) -> Result<Answer<Changes>, Error> {
    let events = r.events.into_iter().map(change_event_from_pb).collect::<Result<_, _>>()?;
    let changes = Changes { events, next_seq: r.next_seq, first_seq: r.first_seq };
    Ok(answer_from_pb(changes, r.meta.ok_or_else(|| missing("meta"))?))
}

/// A batch of `Watch` is a batch of `GetChanges`.
pub(crate) fn watch_response(r: pb::GetChangesResponse) -> pb::WatchResponse {
    pb::WatchResponse { events: r.events, next_seq: r.next_seq, first_seq: r.first_seq, meta: r.meta }
}
