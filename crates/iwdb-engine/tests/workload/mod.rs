//! Proptest strategies for random commit workloads: transactions and
//! catalog changes over small id, key and value spaces, so that mutations
//! collide, conflict and violate constraints often.
//!
//! Shared by the step 3 model test (`commit_model.rs`) and the WAL tests in
//! `iwdb-storage`, which include this file with `#[path]`.

// Each test binary uses a different subset.
#![allow(dead_code, clippy::unwrap_used)]

use ironweaver_core::{Attrs, EdgeId, Value};
use iwdb_engine::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label};
use iwdb_engine::{CatalogChange, EdgeKey, Mutation, Target};
use proptest::collection::{hash_map, vec};
use proptest::prelude::*;

pub fn node_id() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "d"]).prop_map(str::to_owned)
}

pub fn edge_id() -> impl Strategy<Value = EdgeId> {
    (0u64..8).prop_map(EdgeId)
}

pub fn label() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["A", "B"]).prop_map(str::to_owned)
}

pub fn edge_type() -> impl Strategy<Value = Option<String>> {
    prop::option::of(prop::sample::select(vec!["T", "U"]).prop_map(str::to_owned))
}

/// Attribute keys, rarely a reserved one.
pub fn key() -> impl Strategy<Value = String> {
    prop_oneof![40 => prop::sample::select(vec!["x", "y"]).prop_map(str::to_owned), 1 => Just("iwdb.k".to_owned())]
}

pub fn value() -> impl Strategy<Value = Value> {
    // Int(1) and Float(1.0) are the same key for unique constraints
    let scalar = prop_oneof![
        (0i64..3).prop_map(Value::Int),
        prop::sample::select(vec![0.0, 1.0, 1.5]).prop_map(Value::Float),
        prop::sample::select(vec!["s", "t"]).prop_map(Value::from),
        Just(Value::None),
    ];
    prop_oneof![4 => scalar.clone(), 1 => vec(scalar, 0..3).prop_map(Value::List)]
}

pub fn attrs() -> impl Strategy<Value = Attrs> {
    hash_map(key(), value(), 0..3)
}

/// User meta, rarely with a reserved key.
pub fn meta() -> impl Strategy<Value = Attrs> {
    prop_oneof![
        30 => Just(Attrs::new()),
        8 => Just([("m".to_owned(), Value::Int(1))].into()),
        1 => Just([("iwdb.m".to_owned(), Value::Int(1))].into()),
    ]
}

pub fn expected() -> impl Strategy<Value = Option<u64>> {
    prop_oneof![12 => Just(None), 1 => (0u64..3).prop_map(Some)]
}

pub fn target() -> impl Strategy<Value = Target> {
    prop_oneof![3 => node_id().prop_map(Target::Node), 1 => edge_id().prop_map(Target::Edge)]
}

pub fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        5 => (node_id(), vec(label(), 0..2), attrs(), meta(), expected()).prop_map(
            |(id, labels, attr, meta, expected_version)| Mutation::UpsertNode { id, labels, attr, meta, expected_version }
        ),
        1 => (node_id(), expected()).prop_map(|(id, expected_version)| Mutation::DeleteNode { id, expected_version }),
        3 => (node_id(), node_id(), edge_type(), attrs(), meta())
            .prop_map(|(from, to, ty, attr, meta)| Mutation::AddEdge { from, to, ty, attr, meta }),
        2 => (
            prop_oneof![
                edge_id().prop_map(EdgeKey::Id),
                (node_id(), node_id(), edge_type()).prop_map(|(from, to, ty)| EdgeKey::Endpoints { from, to, ty })
            ],
            attrs(),
            meta(),
            expected()
        )
            .prop_map(|(key, attr, meta, expected_version)| Mutation::UpsertEdge { key, attr, meta, expected_version }),
        1 => (edge_id(), expected()).prop_map(|(id, expected_version)| Mutation::DeleteEdge { id, expected_version }),
        5 => (target(), key(), value(), expected())
            .prop_map(|(target, key, value, expected_version)| Mutation::SetAttr { target, key, value, expected_version }),
        1 => (target(), key(), expected())
            .prop_map(|(target, key, expected_version)| Mutation::RemoveAttr { target, key, expected_version }),
        1 => (target(), key(), value(), expected()).prop_map(|(target, key, value, expected_version)| {
            Mutation::AppendAttr { target, key, value, expected_version }
        }),
        2 => (node_id(), label(), expected())
            .prop_map(|(id, label, expected_version)| Mutation::AddLabel { id, label, expected_version }),
        1 => (node_id(), label(), expected())
            .prop_map(|(id, label, expected_version)| Mutation::RemoveLabel { id, label, expected_version }),
        1 => (edge_id(), edge_type(), expected())
            .prop_map(|(id, ty, expected_version)| Mutation::SetEdgeType { id, ty, expected_version }),
    ]
}

pub fn catalog_change() -> impl Strategy<Value = CatalogChange> {
    let path = prop::sample::select(vec!["x", "y"]).prop_map(|k| AttrPath::new([k]).unwrap());
    let constraint = (any::<bool>(), label(), path.clone())
        .prop_map(|(unique, label, path)| Constraint {
            kind: if unique { ConstraintKind::Unique } else { ConstraintKind::Required },
            label: Label::new(label).unwrap(),
            path,
        })
        .boxed();
    prop_oneof![
        1 => path.clone().prop_map(|path| CatalogChange::CreateIndex(IndexDef { path })),
        1 => path.prop_map(|path| CatalogChange::DropIndex(IndexDef { path })),
        2 => constraint.clone().prop_map(CatalogChange::AddConstraint),
        1 => constraint.prop_map(CatalogChange::DropConstraint),
    ]
}

#[derive(Clone, Debug)]
pub enum Step {
    Tx(Vec<Mutation>),
    Catalog(CatalogChange),
}

pub fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => vec(mutation(), 1..4).prop_map(Step::Tx),
        1 => Just(Step::Tx(vec![])),
        3 => catalog_change().prop_map(Step::Catalog),
    ]
}

/// A first transaction that creates every node, so that later mutations
/// mostly find what they address.
pub fn seed() -> impl Strategy<Value = Step> {
    vec((vec(label(), 0..3), attrs()), 4).prop_map(|nodes| {
        let ids = ["a", "b", "c", "d"];
        Step::Tx(
            ids.iter()
                .zip(nodes)
                .map(|(id, (labels, attr))| Mutation::UpsertNode {
                    id: id.to_string(),
                    labels,
                    attr: attr.into_iter().filter(|(k, _)| !k.starts_with("iwdb.")).collect(),
                    meta: Attrs::new(),
                    expected_version: None,
                })
                .collect(),
        )
    })
}
