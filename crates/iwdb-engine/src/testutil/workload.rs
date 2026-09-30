//! Random commit workloads (feature `testutil`): proptest strategies for
//! transactions and catalog changes over small id, key and value spaces, so
//! that mutations collide, conflict and violate constraints often; and
//! deterministic workloads generated from a seed ([`seeded`], [`Stream`]),
//! which a crash harness can regenerate in another process.
//!
//! Used by the step 3 model test, the WAL and store tests, and the crash
//! harness (`tests/crash`). Not part of the database's API.

// The strategies build attribute paths and labels from constants, which
// are valid.
#![allow(clippy::unwrap_used)]

use crate::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label};
use crate::{CatalogChange, EdgeKey, Mutation, Target};
use ironweaver_core::{Attrs, EdgeId, Value};
use proptest::collection::{hash_map, vec};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

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

#[derive(Clone, Debug, PartialEq)]
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

/// A test runner whose random choices depend only on `seed`, the same in
/// every process and on every platform (ChaCha).
pub fn runner(seed: u64) -> TestRunner {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    TestRunner::new_with_rng(Config::default(), TestRng::from_seed(RngAlgorithm::ChaCha, &bytes))
}

/// One value of `strategy`, drawn with `runner`.
fn draw<S: Strategy>(runner: &mut TestRunner, strategy: &S) -> S::Value {
    // Only fails when a strategy rejects every value, which none here does
    strategy.new_tree(runner).unwrap().current()
}

/// A fixed workload: the [`seed`] step and `n` random steps from `seed`,
/// each followed by a [`pad`] commit, so that 1 KiB WAL segments rotate
/// every few commits.
pub fn seeded(n: usize, seed: u64) -> Vec<Step> {
    let mut runner = runner(seed);
    let strategy = (self::seed(), vec(step(), n)).prop_map(|(seed, mut steps)| {
        steps.insert(0, seed);
        steps
    });
    let steps = draw(&mut runner, &strategy);
    let mut padded = Vec::with_capacity(2 * steps.len());
    for (i, step) in steps.into_iter().enumerate() {
        padded.push(step);
        padded.push(pad(i));
    }
    padded
}

/// Upsert padding node `p<i % 3>` with a 200-byte string.
pub fn pad(i: usize) -> Step {
    Step::Tx(vec![Mutation::UpsertNode {
        id: format!("p{}", i % 3),
        labels: vec![],
        attr: [("pad".to_owned(), Value::from(format!("{:0>200}", i)))].into(),
        meta: Attrs::new(),
        expected_version: None,
    }])
}

/// An endless workload from a seed: the [`seed`] step, then random
/// [`step`]s, each followed by a [`pad`] commit. The same seed gives the
/// same steps in every process, which is how a crash harness rebuilds what
/// a killed child committed.
pub struct Stream {
    runner: TestRunner,
    step: BoxedStrategy<Step>,
    next: usize,
}

impl Stream {
    pub fn new(seed: u64) -> Self {
        Stream { runner: runner(seed), step: step().boxed(), next: 0 }
    }
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").field("next", &self.next).finish_non_exhaustive()
    }
}

impl Iterator for Stream {
    type Item = Step;

    fn next(&mut self) -> Option<Step> {
        let i = self.next;
        self.next += 1;
        Some(match i {
            0 => draw(&mut self.runner, &seed()),
            i if i % 2 == 0 => pad(i / 2),
            _ => draw(&mut self.runner, &self.step),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(seed: u64) -> Vec<Step> {
        Stream::new(seed).take(50).collect()
    }

    #[test]
    fn workloads_depend_only_on_the_seed() {
        assert_eq!(seeded(20, 7), seeded(20, 7));
        assert_ne!(seeded(20, 7), seeded(20, 8));
        assert_eq!(stream(3), stream(3));
        assert_ne!(stream(3), stream(4));
    }
}
