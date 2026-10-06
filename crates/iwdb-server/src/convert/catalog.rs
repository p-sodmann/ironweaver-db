//! Catalog, status and namespaces.

use std::path::PathBuf;

use iwdb_engine::catalog::{IndexChanges, NamespaceCatalog, NamespaceName};
use iwdb_query::{
    Error, IndexSize, IndexState, IndexStatus, KeyInfo, LabelInfo, MarkStatus, NamespaceStatus, Schema, TypeInfo,
};
use iwdb_storage::format::Damage;
use iwdb_storage::namespaces::{Event, EventKind, NamespaceInfo, NamespaceResult};
use iwdb_storage::{CutTail, RecoveryReport, SkippedCheckpoint};

use super::mutations::{constraint_from_pb, constraint_to_pb, index_from_pb, index_to_pb};
use super::{attr_path_from_pb, idempotency_key_from_pb, missing, path_to_pb, size, time_from_pb, wide};
use crate::proto as pb;

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

pub(crate) fn schema_to_pb(s: &Schema) -> pb::Schema {
    pb::Schema {
        labels: s
            .labels
            .iter()
            .map(|l| pb::LabelInfo {
                name: l.name.clone(),
                count: wide(l.count),
                sampled: wide(l.sampled),
                keys: l
                    .keys
                    .iter()
                    .map(|k| pb::KeyInfo {
                        name: k.name.clone(),
                        kinds: k
                            .kinds
                            .iter()
                            .map(|(kind, n)| pb::KindCount { kind: kind.clone(), count: wide(*n) })
                            .collect(),
                    })
                    .collect(),
                more_keys: l.more_keys,
            })
            .collect(),
        types: s.types.iter().map(|t| pb::TypeInfo { name: t.name.clone(), count: wide(t.count) }).collect(),
        nodes: wide(s.nodes),
        edges: wide(s.edges),
        sampled_nodes: wide(s.sampled_nodes),
        sampled_edges: wide(s.sampled_edges),
    }
}

pub(crate) fn schema_from_pb(s: Option<pb::Schema>) -> Result<Schema, Error> {
    let s = s.ok_or_else(|| missing("the schema"))?;
    Ok(Schema {
        labels: s
            .labels
            .into_iter()
            .map(|l| LabelInfo {
                name: l.name,
                count: size(l.count),
                sampled: size(l.sampled),
                keys: l
                    .keys
                    .into_iter()
                    .map(|k| KeyInfo {
                        name: k.name,
                        kinds: k.kinds.into_iter().map(|c| (c.kind, size(c.count))).collect(),
                    })
                    .collect(),
                more_keys: l.more_keys,
            })
            .collect(),
        types: s.types.into_iter().map(|t| TypeInfo { name: t.name, count: size(t.count) }).collect(),
        nodes: size(s.nodes),
        edges: size(s.edges),
        sampled_nodes: size(s.sampled_nodes),
        sampled_edges: size(s.sampled_edges),
    })
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
        finished_import: r.finished_import,
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
        finished_import: r.finished_import,
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
        marks: s
            .marks
            .iter()
            .map(|m| pb::MarkStatus { name: m.name.clone(), position: m.position, seq: m.seq })
            .collect(),
        unsynced: s.unsynced,
        since_checkpoint: s.since_checkpoint,
        last_checkpoint_micros: s.last_checkpoint.map(|t| t.0),
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
        unsynced: s.unsynced,
        since_checkpoint: s.since_checkpoint,
        last_checkpoint: s.last_checkpoint_micros.map(time_from_pb),
        read_only: s.read_only,
        checkpoint_failure: s.checkpoint_failure,
        nodes: size(s.nodes),
        edges: size(s.edges),
        memory_bytes: size(s.memory_bytes),
        indexes: s.indexes.into_iter().map(index_status_from_pb).collect::<Result<_, _>>()?,
        constraints: size(s.constraints),
        recovery: recovery_from_pb(s.recovery)?,
        marks: s.marks.into_iter().map(|m| MarkStatus { name: m.name, position: m.position, seq: m.seq }).collect(),
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
    let keyed = idempotency_key_from_pb(e.idempotency_key)?.map(|k| (k, e.fingerprint));
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
