//! What a namespace holds: its labels, edge types and attribute keys.

use std::collections::BTreeMap;

use ironweaver_core::Value;
use iwdb_engine::Namespace;

use super::ReadContext;
use crate::{Answer, Error, Work};

/// Distinct attribute keys reported per label; more set
/// [`LabelInfo::more_keys`].
pub const MAX_KEYS_PER_LABEL: usize = 256;

/// A namespace's labels and edge types ([`Database::schema`](crate::Database::schema)).
///
/// The core can count a label's nodes in O(1) but can't list the labels a
/// graph has, and has no index of edge types (upstream #60), so this
/// is read from a sample: the first `max_visited` nodes and the first
/// `max_edges` edges in the core's slot order. When the sample covers the
/// namespace (`sampled_nodes == nodes`, `sampled_edges == edges`) the lists
/// are complete and every count exact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    /// The labels of the sampled nodes and of the constraints, by name.
    pub labels: Vec<LabelInfo>,
    /// The types of the sampled edges, by name (untyped first).
    pub types: Vec<TypeInfo>,
    /// Nodes in the namespace.
    pub nodes: usize,
    /// Edges in the namespace.
    pub edges: usize,
    /// Nodes the sample read.
    pub sampled_nodes: usize,
    /// Edges the sample read.
    pub sampled_edges: usize,
}

/// A label ([`Schema::labels`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelInfo {
    pub name: String,
    /// Nodes with the label: exact (the core's label index).
    pub count: usize,
    /// Sampled nodes with the label: what [`keys`](Self::keys) counts.
    pub sampled: usize,
    /// The attribute keys of the sampled nodes with the label, by name.
    pub keys: Vec<KeyInfo>,
    /// The sampled nodes had more than [`MAX_KEYS_PER_LABEL`] keys.
    pub more_keys: bool,
}

/// An attribute key of a label's sampled nodes ([`LabelInfo::keys`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyInfo {
    pub name: String,
    /// How many of them had a value of each kind (`Int`, `String`, ... as
    /// in the value JSON), by kind.
    pub kinds: Vec<(String, usize)>,
}

/// An edge type ([`Schema::types`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeInfo {
    /// `None` for untyped edges.
    pub name: Option<String>,
    /// Sampled edges of the type (exact when the sample covers every edge).
    pub count: usize,
}

/// The name of a value's kind, as the value JSON tags it.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "String",
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Half(_) => "Half",
        Value::Bool(_) => "Bool",
        Value::None => "None",
        Value::List(_) => "List",
        Value::Dict(_) => "Dict",
        Value::Bytes(_) => "Bytes",
        Value::Date(_) => "Date",
        Value::DateTime(_) => "DateTime",
    }
}

#[derive(Default)]
struct Keys {
    nodes: usize,
    kinds: BTreeMap<String, BTreeMap<&'static str, usize>>,
    more: bool,
}

/// The namespace's schema, from a sample of at most `max_visited` nodes and
/// `max_edges` edges: O(sample + labels). Never fails on a limit (the
/// sample is the point): compare the sampled counts with the totals.
pub fn schema(ns: &Namespace, cx: &ReadContext) -> Result<Answer<Schema>, Error> {
    let g = ns.graph();
    let stop = ironweaver_core::cancel::stop();
    let mut labels: BTreeMap<String, Keys> = BTreeMap::new();
    let mut sampled_nodes = 0;
    for ix in g.node_indices().take(cx.bounds.max_visited) {
        if stop.poll() {
            break;
        }
        let (Some(node), Some(names)) = (g.node(ix), g.label_names(ix)) else { continue };
        sampled_nodes += 1;
        for name in names {
            let keys = labels.entry(name.to_owned()).or_default();
            keys.nodes += 1;
            for (key, value) in &node.data.attr {
                if !keys.kinds.contains_key(key.as_str()) && keys.kinds.len() >= MAX_KEYS_PER_LABEL {
                    keys.more = true;
                    continue;
                }
                *keys.kinds.entry(key.clone()).or_default().entry(kind(value)).or_default() += 1;
            }
        }
    }
    // A constraint's label is part of the schema even without a sampled node
    for constraint in ns.catalog().constraints() {
        labels.entry(constraint.label.as_str().to_owned()).or_default();
    }
    let mut types: BTreeMap<Option<String>, usize> = BTreeMap::new();
    let mut sampled_edges = 0;
    for (ix, _) in g.edges().take(cx.bounds.max_edges) {
        if stop.poll() {
            break;
        }
        sampled_edges += 1;
        *types.entry(g.edge_type_name(ix).map(str::to_owned)).or_default() += 1;
    }
    let value = Schema {
        labels: labels
            .into_iter()
            .map(|(name, keys)| LabelInfo {
                count: g.label_count(&name),
                sampled: keys.nodes,
                keys: keys
                    .kinds
                    .into_iter()
                    .map(|(name, kinds)| KeyInfo {
                        name,
                        kinds: kinds.into_iter().map(|(k, n)| (k.to_owned(), n)).collect(),
                    })
                    .collect(),
                more_keys: keys.more,
                name,
            })
            .collect(),
        types: types.into_iter().map(|(name, count)| TypeInfo { name, count }).collect(),
        nodes: g.node_count(),
        edges: g.edge_count(),
        sampled_nodes,
        sampled_edges,
    };
    let work = Work { visited: sampled_nodes, edges: sampled_edges };
    Ok(Answer { work, ..Answer::at(ns.seq(), value) })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use iwdb_engine::Mutation;
    use iwdb_engine::catalog::NamespaceName;
    use iwdb_storage::HistoryId;

    use super::*;
    use crate::{Bounds, QueryOptions};

    fn node(id: &str, labels: &[&str], attr: Vec<(String, Value)>) -> Mutation {
        Mutation::UpsertNode {
            id: id.to_owned(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            attr: attr.into_iter().collect(),
            meta: Default::default(),
            expected_version: None,
        }
    }

    fn read(ns: &Namespace, max_visited: usize) -> Schema {
        let bounds = Bounds { max_results: 10, max_visited, max_edges: 10 };
        let cx = ReadContext::new(bounds, &QueryOptions::default(), 1, HistoryId::default());
        schema(ns, &cx).unwrap().value
    }

    #[test]
    fn keys_are_counted_by_kind_and_capped_per_label() {
        let mut ns = Namespace::new(NamespaceName::new("t").unwrap());
        let wide: Vec<(String, Value)> =
            (0..MAX_KEYS_PER_LABEL + 5).map(|i| (format!("k{:03}", i), Value::Int(1))).collect();
        ns.commit(&[
            node("a", &["Wide"], wide),
            node("b", &["Thing"], vec![("k000".into(), Value::String("x".into()))]),
            node("c", &["Thing"], vec![("k000".into(), Value::Int(2))]),
            node("d", &[], vec![]),
        ])
        .unwrap();
        let s = read(&ns, 100);
        assert_eq!((s.nodes, s.sampled_nodes), (4, 4));
        let wide = s.labels.iter().find(|l| l.name == "Wide").unwrap();
        assert_eq!((wide.count, wide.keys.len(), wide.more_keys), (1, MAX_KEYS_PER_LABEL, true));
        let thing = s.labels.iter().find(|l| l.name == "Thing").unwrap();
        assert_eq!((thing.count, thing.sampled), (2, 2));
        // Both kinds of `k000`, by kind name
        let k000 = thing.keys.iter().find(|k| k.name == "k000").unwrap();
        assert_eq!(k000.kinds, [("Int".to_owned(), 1), ("String".to_owned(), 1)]);
        // A sample of one node: its labels' counts stay exact
        let one = read(&ns, 1);
        assert_eq!(one.sampled_nodes, 1);
        assert!(one.labels.len() == 1 && one.labels.iter().all(|l| l.count == if l.name == "Thing" { 2 } else { 1 }));
    }
}
