//! Random histories of transactions and catalog changes
//! (many of them failing) against a simple reference model, and the replay
//! property the WAL relies on: the records of the successful commits,
//! replayed onto an empty namespace, give the same state.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use ironweaver_core::{Attributes, Attrs, EdgeId, HeapSize, Key, Op, Value};
use iwdb_engine::catalog::{Constraint, ConstraintKind, NamespaceCatalog, NamespaceName};
use iwdb_engine::testutil::{canonical, state};
use iwdb_engine::{
    CatalogChange, Change, CommitRecord, CommitResult, DbGraph, DbRecord, EdgeKey, Error, Mutation, Namespace,
    Prepared, Target,
};
use proptest::collection::vec;
use proptest::prelude::*;

use iwdb_engine::testutil::workload::{Step, seed, step};

// The reference model: plain maps, whole-state copies, checks written
// independently of the engine's overlay.

#[derive(Clone, Debug)]
struct MNode {
    labels: BTreeSet<String>,
    attr: Attrs,
    meta: Attrs,
    version: u64,
}

#[derive(Clone, Debug)]
struct MEdge {
    from: String,
    to: String,
    ty: Option<String>,
    attr: Attrs,
    meta: Attrs,
    version: u64,
}

#[derive(Clone, Debug, Default)]
struct Model {
    nodes: BTreeMap<String, MNode>,
    edges: BTreeMap<EdgeId, MEdge>,
    next_edge: u64,
    catalog: NamespaceCatalog,
    seq: u64,
}

/// The engine's errors, by kind (which of several violations is reported
/// is the engine's choice; the model only predicts the kind).
fn kind(e: &Error) -> &'static str {
    match e {
        Error::ReservedName { .. } => "ReservedName",
        Error::Conflict { .. } => "Conflict",
        Error::NotFound { .. } => "NotFound",
        Error::AmbiguousEdge { .. } => "AmbiguousEdge",
        Error::NoMatchingEdge { .. } => "NoMatchingEdge",
        Error::NotAList { .. } => "NotAList",
        Error::ConstraintViolation { .. } => "ConstraintViolation",
        Error::EmptyTransaction => "EmptyTransaction",
        Error::IndexExists { .. } => "IndexExists",
        Error::NoSuchIndex { .. } => "NoSuchIndex",
        Error::ConstraintExists { .. } => "ConstraintExists",
        Error::NoSuchConstraint { .. } => "NoSuchConstraint",
        other => panic!("unexpected error {:?}", other),
    }
}

type Outcome = Result<CommitResult, &'static str>;

fn reserved(map: &Attrs) -> bool {
    map.keys().any(|k| k.starts_with("iwdb."))
}

fn has_value(attr: &Attrs, key: &str) -> Option<Key> {
    attr.get(key).and_then(Key::of)
}

impl Model {
    fn version_of(&self, target: &Target) -> u64 {
        match target {
            Target::Node(id) => self.nodes.get(id).map_or(0, |n| n.version),
            Target::Edge(id) => self.edges.get(id).map_or(0, |e| e.version),
        }
    }

    fn exists(&self, target: &Target) -> bool {
        match target {
            Target::Node(id) => self.nodes.contains_key(id),
            Target::Edge(id) => self.edges.contains_key(id),
        }
    }

    fn attr_mut(&mut self, target: &Target) -> &mut Attrs {
        match target {
            Target::Node(id) => &mut self.nodes.get_mut(id).unwrap().attr,
            Target::Edge(id) => &mut self.edges.get_mut(id).unwrap().attr,
        }
    }

    fn commit(&mut self, mutations: &[Mutation]) -> Outcome {
        if mutations.is_empty() {
            return Err("EmptyTransaction");
        }
        let before = self.clone();
        let mut m = self.clone();
        let mut written = BTreeSet::new();
        let mut edge_ids = Vec::new();
        for mutation in mutations {
            m.apply(&before, mutation, &mut written, &mut edge_ids)?;
        }
        let mut versions = Vec::new();
        for target in written {
            if m.exists(&target) {
                let v = before.version_of(&target) + 1;
                match &target {
                    Target::Node(id) => m.nodes.get_mut(id).unwrap().version = v,
                    Target::Edge(id) => m.edges.get_mut(id).unwrap().version = v,
                }
                versions.push((target, v));
            }
        }
        for c in m.catalog.constraints() {
            m.check(c)?;
        }
        m.seq += 1;
        *self = m;
        Ok(CommitResult { seq: self.seq, edge_ids, versions, ..CommitResult::default() })
    }

    /// Whether the whole model satisfies `c`.
    fn check(&self, c: &Constraint) -> Result<(), &'static str> {
        let key = &c.path.keys()[0];
        let nodes = self.nodes.values().filter(|n| n.labels.contains(c.label.as_str()));
        match c.kind {
            ConstraintKind::Required => {
                if nodes.clone().any(|n| n.attr.get(key).is_none_or(|v| *v == Value::None)) {
                    return Err("ConstraintViolation");
                }
            }
            ConstraintKind::Unique => {
                let keys: Vec<Key> = nodes.filter_map(|n| has_value(&n.attr, key)).collect();
                let distinct: BTreeSet<&Key> = keys.iter().collect();
                if distinct.len() != keys.len() {
                    return Err("ConstraintViolation");
                }
            }
        }
        Ok(())
    }

    /// Check `expected` against the state before the transaction, then
    /// that the target exists now; mark it written.
    fn write(
        &self,
        before: &Model,
        target: Target,
        expected: Option<u64>,
        written: &mut BTreeSet<Target>,
    ) -> Result<(), &'static str> {
        if expected.is_some_and(|e| e != before.version_of(&target)) {
            return Err("Conflict");
        }
        if !self.exists(&target) {
            return Err("NotFound");
        }
        written.insert(target);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_edge(
        &mut self,
        from: &str,
        to: &str,
        ty: &Option<String>,
        attr: &Attrs,
        meta: &Attrs,
        written: &mut BTreeSet<Target>,
        edge_ids: &mut Vec<EdgeId>,
    ) -> Result<(), &'static str> {
        if reserved(attr) || reserved(meta) {
            return Err("ReservedName");
        }
        if !self.nodes.contains_key(from) || !self.nodes.contains_key(to) {
            return Err("NotFound");
        }
        let id = EdgeId(self.next_edge);
        self.next_edge += 1;
        let edge = MEdge {
            from: from.into(),
            to: to.into(),
            ty: ty.clone(),
            attr: attr.clone(),
            meta: meta.clone(),
            version: 0,
        };
        self.edges.insert(id, edge);
        written.insert(Target::Edge(id));
        edge_ids.push(id);
        Ok(())
    }

    fn apply(
        &mut self,
        before: &Model,
        mutation: &Mutation,
        written: &mut BTreeSet<Target>,
        edge_ids: &mut Vec<EdgeId>,
    ) -> Result<(), &'static str> {
        match mutation {
            Mutation::UpsertNode { id, labels, attr, meta, expected_version } => {
                if reserved(attr) || reserved(meta) {
                    return Err("ReservedName");
                }
                let target = Target::Node(id.clone());
                if expected_version.is_some_and(|e| e != before.version_of(&target)) {
                    return Err("Conflict");
                }
                let node = self.nodes.entry(id.clone()).or_insert(MNode {
                    labels: BTreeSet::new(),
                    attr: Attrs::new(),
                    meta: Attrs::new(),
                    version: 0,
                });
                node.labels.extend(labels.iter().cloned());
                node.attr = attr.clone();
                node.meta = meta.clone();
                written.insert(target);
            }
            Mutation::DeleteNode { id, expected_version } => {
                let target = Target::Node(id.clone());
                if expected_version.is_some_and(|e| e != before.version_of(&target)) {
                    return Err("Conflict");
                }
                if self.nodes.remove(id).is_none() {
                    return Err("NotFound");
                }
                self.edges.retain(|_, e| e.from != *id && e.to != *id);
            }
            Mutation::AddEdge { from, to, ty, attr, meta } => {
                self.add_edge(from, to, ty, attr, meta, written, edge_ids)?;
            }
            Mutation::UpsertEdge { key, attr, meta, expected_version } => {
                let id = match key {
                    EdgeKey::Id(id) => *id,
                    EdgeKey::Endpoints { from, to, ty } => {
                        let found: Vec<EdgeId> = self
                            .edges
                            .iter()
                            .filter(|(_, e)| e.from == *from && e.to == *to && e.ty == *ty)
                            .map(|(id, _)| *id)
                            .collect();
                        match found[..] {
                            [] if expected_version.is_some_and(|e| e != 0) => return Err("NoMatchingEdge"),
                            [] => return self.add_edge(from, to, ty, attr, meta, written, edge_ids),
                            [id] => id,
                            _ => return Err("AmbiguousEdge"),
                        }
                    }
                };
                if reserved(attr) || reserved(meta) {
                    return Err("ReservedName");
                }
                self.write(before, Target::Edge(id), *expected_version, written)?;
                let edge = self.edges.get_mut(&id).unwrap();
                edge.attr = attr.clone();
                edge.meta = meta.clone();
                edge_ids.push(id);
            }
            Mutation::DeleteEdge { id, expected_version } => {
                if expected_version.is_some_and(|e| e != before.version_of(&Target::Edge(*id))) {
                    return Err("Conflict");
                }
                if self.edges.remove(id).is_none() {
                    return Err("NotFound");
                }
            }
            Mutation::SetAttr { target, key, value, expected_version } => {
                if key.starts_with("iwdb.") {
                    return Err("ReservedName");
                }
                self.write(before, target.clone(), *expected_version, written)?;
                self.attr_mut(target).insert(key.clone(), value.clone());
            }
            Mutation::RemoveAttr { target, key, expected_version } => {
                if key.starts_with("iwdb.") {
                    return Err("ReservedName");
                }
                self.write(before, target.clone(), *expected_version, written)?;
                self.attr_mut(target).remove(key);
            }
            Mutation::AppendAttr { target, key, value, expected_version } => {
                if key.starts_with("iwdb.") {
                    return Err("ReservedName");
                }
                self.write(before, target.clone(), *expected_version, written)?;
                let attr = self.attr_mut(target);
                let mut items = match attr.get(key) {
                    None | Some(Value::None) => vec![],
                    Some(Value::List(items)) => items.clone(),
                    Some(_) => return Err("NotAList"),
                };
                items.push(value.clone());
                attr.insert(key.clone(), Value::List(items));
            }
            Mutation::AddLabel { id, label, expected_version } => {
                self.write(before, Target::Node(id.clone()), *expected_version, written)?;
                self.nodes.get_mut(id).unwrap().labels.insert(label.clone());
            }
            Mutation::RemoveLabel { id, label, expected_version } => {
                self.write(before, Target::Node(id.clone()), *expected_version, written)?;
                self.nodes.get_mut(id).unwrap().labels.remove(label);
            }
            Mutation::SetEdgeType { id, ty, expected_version } => {
                self.write(before, Target::Edge(*id), *expected_version, written)?;
                self.edges.get_mut(id).unwrap().ty.clone_from(ty);
            }
        }
        Ok(())
    }

    fn change_catalog(&mut self, change: &CatalogChange) -> Outcome {
        let mut catalog = self.catalog.clone();
        match change {
            CatalogChange::CreateIndex(index) => {
                if !catalog.add_index(index.clone()) {
                    return Err("IndexExists");
                }
            }
            CatalogChange::DropIndex(index) => {
                if !catalog.remove_index(index) {
                    return Err("NoSuchIndex");
                }
            }
            CatalogChange::AddConstraint(c) => {
                if !catalog.add_constraint(c.clone()) {
                    return Err("ConstraintExists");
                }
                self.check(c)?;
            }
            CatalogChange::DropConstraint(c) => {
                if !catalog.remove_constraint(c) {
                    return Err("NoSuchConstraint");
                }
            }
        }
        self.catalog = catalog;
        self.seq += 1;
        Ok(CommitResult { seq: self.seq, ..CommitResult::default() })
    }

    /// The model as a graph, for canonical comparison.
    fn graph(&self) -> DbGraph {
        let mut ops: Vec<Op<DbRecord, DbRecord>> = Vec::new();
        for (id, n) in &self.nodes {
            let data = DbRecord { attr: n.attr.clone(), meta: n.meta.clone(), version: n.version };
            ops.push(Op::AddNode { id: id.clone(), labels: n.labels.iter().cloned().collect(), data });
        }
        for (id, e) in &self.edges {
            let data = DbRecord { attr: e.attr.clone(), meta: e.meta.clone(), version: e.version };
            ops.push(Op::AddEdge { id: *id, from: e.from.clone(), to: e.to.clone(), ty: e.ty.clone(), data });
        }
        let mut g = DbGraph::new();
        g.apply_all(ops).unwrap();
        g
    }
}

fn index_paths(g: &DbGraph) -> BTreeSet<Vec<String>> {
    g.index_paths().into_iter().map(<[String]>::to_vec).collect()
}

fn catalog_paths(c: &NamespaceCatalog) -> BTreeSet<Vec<String>> {
    c.index_paths().into_iter().map(|p| p.keys().to_vec()).collect()
}

/// The payloads from scratch: every node's and edge's `HeapSize`.
fn payload_recounted(g: &DbGraph) -> usize {
    g.nodes().map(|(_, n)| n.data.heap_bytes()).sum::<usize>()
        + g.edges().map(|(_, e)| e.data.heap_bytes()).sum::<usize>()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn commits_match_the_model_and_replay_exactly(steps in (seed(), vec(step(), 1..40)).prop_map(|(seed, mut steps)| {
        steps.insert(0, seed);
        steps
    })) {
        let mut ns = Namespace::new(NamespaceName::new("model").unwrap());
        let mut model = Model::default();
        let mut log: Vec<CommitRecord> = Vec::new();
        let mut committed = 0;

        for step in &steps {
            let before = state(&ns);
            let (expected, prepared): (Outcome, Result<Prepared, Error>) = match step {
                Step::Tx(mutations) => (model.commit(mutations), ns.prepare(mutations)),
                Step::Catalog(change) => (model.change_catalog(change), ns.prepare_catalog(change.clone())),
            };
            let actual = prepared.and_then(|p| {
                log.push(p.record().clone());
                ns.apply(p, None)
            });
            match (&actual, &expected) {
                (Ok(a), Ok(e)) => {
                    prop_assert_eq!(a, e);
                    committed += 1;
                }
                (Err(a), Err(e)) => {
                    prop_assert_eq!(kind(a), *e, "{:?}", a);
                    prop_assert_eq!(state(&ns), before.clone(), "a failed commit changed the namespace");
                }
                _ => prop_assert!(false, "engine {:?}, model {:?}, step {:?}", actual, expected, step),
            }
            prop_assert_eq!(canonical(ns.graph()), canonical(&model.graph()));
            prop_assert_eq!(ns.catalog(), &model.catalog);
            prop_assert_eq!(ns.seq(), model.seq);
            prop_assert_eq!(index_paths(ns.graph()), catalog_paths(ns.catalog()));
            prop_assert!(!ns.graph().indexes_dirty());
            prop_assert!(!ns.is_poisoned());
            // The core keeps the payloads' count (upstream #61); its own
            // tests check it exactly
            prop_assert!(ns.memory_bytes() >= payload_recounted(ns.graph()), "the payloads aren't counted");
        }
        prop_assert_eq!(log.len(), committed);

        // Replay: the records, through the log's encoding, onto an empty namespace
        let mut replica = Namespace::new(NamespaceName::new("model").unwrap());
        for record in &log {
            let bytes = postcard::to_allocvec(record).unwrap();
            let decoded: CommitRecord = postcard::from_bytes(&bytes).unwrap();
            prop_assert_eq!(&decoded, record);
            replica.replay(decoded, None).unwrap();
        }
        prop_assert_eq!(state(&replica), state(&ns));
        prop_assert_eq!(index_paths(replica.graph()), index_paths(ns.graph()));
        prop_assert!(replica.graph().counts_payloads() && replica.memory_bytes() >= payload_recounted(replica.graph()));

        // The data ops alone, applied with the core's apply_all, give the same graph
        let mut raw = DbGraph::new();
        for record in log {
            if let Change::Data(ops) = record.change {
                raw.apply_all(ops).unwrap();
            }
        }
        prop_assert_eq!(canonical(&raw), canonical(ns.graph()));

        // Unique constraints hold with the index lookups the engine uses
        for c in ns.catalog().constraints().filter(|c| c.kind == ConstraintKind::Unique) {
            let mut seen = BTreeSet::new();
            for ix in ns.graph().nodes_with_label(c.label.as_str()) {
                let data = &ns.graph().node(ix).unwrap().data;
                let Some(v) = data.with_value(c.path.keys(), |v| v.cloned()).unwrap() else { continue };
                // Only scalars are indexed (and constrained)
                if let Some(k) = Key::of(&v) {
                    let found = ns.graph().find_nodes(c.path.keys(), &v).unwrap().unwrap();
                    prop_assert!(found.contains(&ix));
                    prop_assert!(seen.insert(k));
                }
            }
        }
    }
}
