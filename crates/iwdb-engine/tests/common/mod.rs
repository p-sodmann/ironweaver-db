//! Shared helpers for the integration tests: proptest strategies for
//! values, records, catalogs and whole `DbGraph`s.

// Each test binary uses a different subset.
#![allow(dead_code)]

use ironweaver_core::{Attrs, Date, DateTime, EdgeId, Op, Value};
use iwdb_engine::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label, NamespaceCatalog, NamespaceName};
use iwdb_engine::codec::GraphMeta;
use iwdb_engine::{DbGraph, DbRecord};
use proptest::collection::{btree_set, hash_map, vec};
use proptest::prelude::*;

/// Attribute keys: a small alphabet, so that random paths often hit.
pub fn key() -> impl Strategy<Value = String> {
    "[a-c]{1,2}"
}

pub fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        "[a-z ]{0,6}".prop_map(Value::String),
        any::<i64>().prop_map(Value::Int),
        // Finite floats: NaN is not equal to itself. No -0.0: the core's JSON
        // loader reads it back as 0.0 (pinned in db_graph.rs)
        any::<f64>()
            .prop_filter("finite, not -0.0", |f| f.is_finite() && !(*f == 0.0 && f.is_sign_negative()))
            .prop_map(Value::Float),
        (-3i64..3).prop_map(|i| Value::Float(i as f64 * 0.5)),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::None),
        vec(any::<u8>(), 0..6).prop_map(Value::Bytes),
        (1i32..9999, 1u32..13, 1u32..29)
            .prop_map(|(y, m, d)| Value::Date(Date::from_ymd(y, m, d).expect("valid date"))),
        (-100_000_000_000_000i64..100_000_000_000_000, prop::option::of(-840i32..840))
            .prop_map(|(micros, minutes)| Value::DateTime(DateTime { micros, offset: minutes.map(|m| m * 60) })),
    ]
}

/// Any value, lists and dicts nested a few levels deep.
pub fn value() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(3, 24, 4, |inner| {
        prop_oneof![vec(inner.clone(), 0..4).prop_map(Value::List), hash_map(key(), inner, 0..4).prop_map(Value::Dict),]
    })
}

pub fn attrs() -> impl Strategy<Value = Attrs> {
    hash_map(key(), value(), 0..5)
}

/// User meta: never reserved keys.
pub fn user_meta() -> impl Strategy<Value = Attrs> {
    hash_map("m[a-c]", scalar(), 0..3)
}

/// Versions that can be saved.
pub fn version() -> impl Strategy<Value = u64> {
    prop_oneof![0u64..5, Just(i64::MAX as u64), 0..=i64::MAX as u64]
}

pub fn record() -> impl Strategy<Value = DbRecord> {
    (attrs(), user_meta(), version()).prop_map(|(attr, meta, version)| DbRecord { attr, meta, version })
}

pub fn path() -> impl Strategy<Value = AttrPath> {
    vec(key(), 1..3).prop_map(|keys| AttrPath::new(keys).expect("valid path"))
}

pub fn namespace_catalog() -> impl Strategy<Value = NamespaceCatalog> {
    let constraint = (any::<bool>(), "[A-C]", path()).prop_map(|(unique, label, path)| Constraint {
        kind: if unique { ConstraintKind::Unique } else { ConstraintKind::Required },
        label: Label::new(label).expect("label"),
        path,
    });
    (btree_set(path(), 0..3), btree_set(constraint, 0..3)).prop_map(|(indexes, constraints)| {
        let mut c = NamespaceCatalog::new();
        for path in indexes {
            c.add_index(IndexDef { path });
        }
        for constraint in constraints {
            c.add_constraint(constraint);
        }
        c
    })
}

pub fn graph_meta() -> impl Strategy<Value = GraphMeta> {
    ("[a-z][a-z0-9_-]{0,8}", namespace_catalog(), prop_oneof![Just(0u64), any::<u64>().prop_map(|s| s >> 1)]).prop_map(
        |(name, catalog, seq)| GraphMeta {
            namespace: NamespaceName::new(name).expect("valid name"),
            catalog,
            seq,
            keys: Default::default(),
        },
    )
}

/// The ops that build a random graph: nodes with labels, edges (parallel
/// edges and self-loops included) with explicit ids that have gaps, and
/// some removed edges and nodes, so that slots are reused.
pub fn graph_ops() -> impl Strategy<Value = Vec<Op<DbRecord, DbRecord>>> {
    let node = (btree_set("[A-C]", 0..3), record());
    let edge =
        (any::<prop::sample::Index>(), any::<prop::sample::Index>(), 1u64..4, prop::option::of("[X-Z]"), record());
    (vec(node, 1..8), vec(edge, 0..12), vec(any::<prop::sample::Index>(), 0..3)).prop_map(|(nodes, edges, removed)| {
        let ids: Vec<String> = (0..nodes.len()).map(|i| format!("n{}", i)).collect();
        let mut ops = Vec::new();
        for (id, (labels, data)) in ids.iter().zip(nodes) {
            ops.push(Op::AddNode { id: id.clone(), labels: labels.into_iter().collect(), data });
        }
        let mut next = 0;
        let mut edge_ids = Vec::new();
        for (from, to, gap, ty, data) in edges {
            next += gap;
            edge_ids.push(next);
            ops.push(Op::AddEdge {
                id: EdgeId(next),
                from: from.get(&ids).clone(),
                to: to.get(&ids).clone(),
                ty,
                data,
            });
        }
        if let Some(first) = removed.first() {
            if !edge_ids.is_empty() {
                ops.push(Op::RemoveEdge { id: EdgeId(*first.get(&edge_ids)) });
            }
        }
        for ix in removed.iter().skip(1) {
            let id = ix.get(&ids).clone();
            if !ops.iter().any(|op| matches!(op, Op::RemoveNode { id: gone } if *gone == id)) {
                ops.push(Op::RemoveNode { id });
            }
        }
        ops
    })
}

/// A graph built from `ops`.
pub fn build(ops: Vec<Op<DbRecord, DbRecord>>) -> DbGraph {
    let mut g = DbGraph::new();
    g.apply_all(ops).map_err(|(at, e)| format!("op {}: {}", at, e)).expect("ops apply");
    g
}
