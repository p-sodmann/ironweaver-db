//! What a namespace holds: its labels, edge types and attribute keys.

use std::collections::BTreeMap;

use ironweaver_core::Value;
use iwdb_engine::Namespace;

use super::ReadContext;
use crate::{Answer, Error, Work};

/// Distinct attribute keys reported per label; more set
/// [`LabelInfo::more_keys`].
pub const MAX_KEYS_PER_LABEL: usize = 256;

/// Labels, and edge types, reported at most; more set the answer's
/// `truncated` (a bound on the read, design rule 5).
pub const MAX_NAMES: usize = 10_000;

/// A namespace's labels and edge types ([`Database::schema`](crate::Database::schema)).
///
/// Labels and edge types, with their counts, come from the core (exact,
/// complete; upstream #60, fixed in `c69ef51`). Attribute keys are read
/// from a sample: the first `max_visited` nodes in the core's slot order;
/// the keys are complete when `sampled_nodes == nodes`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    /// Every label a node carries, and the constraints' labels, by name
    /// (at most [`MAX_NAMES`]).
    pub labels: Vec<LabelInfo>,
    /// Every edge type, by name, untyped first (at most [`MAX_NAMES`]).
    pub types: Vec<TypeInfo>,
    /// Nodes in the namespace.
    pub nodes: usize,
    /// Edges in the namespace.
    pub edges: usize,
    /// Nodes the sample read (for the keys).
    pub sampled_nodes: usize,
    /// Edges the type counts cover: every edge since the core counts
    /// types (upstream #60), so equal to `edges`. Kept so that clients
    /// that compare it with `edges` see complete lists.
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
    /// Edges of the type: exact (the core's count).
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

/// The namespace's schema: labels and types with their counts from the
/// core, O(labels + types); keys from a sample of at most `max_visited`
/// nodes, O(sample). Never fails on a limit (the sample is the point):
/// compare `sampled_nodes` with `nodes`.
pub fn schema(ns: &Namespace, cx: &ReadContext) -> Result<Answer<Schema>, Error> {
    let g = ns.graph();
    let stop = ironweaver_core::cancel::stop();
    // Every label, and a constraint's label even without a node
    let mut labels: BTreeMap<String, Keys> = g.labels().map(|(name, _)| (name.to_owned(), Keys::default())).collect();
    for constraint in ns.catalog().constraints() {
        labels.entry(constraint.label.as_str().to_owned()).or_default();
    }
    let mut sampled_nodes = 0;
    for ix in g.node_indices().take(cx.bounds.max_visited) {
        if stop.poll() {
            break;
        }
        let (Some(node), Some(names)) = (g.node(ix), g.label_names(ix)) else { continue };
        sampled_nodes += 1;
        for name in names {
            let Some(keys) = labels.get_mut(name) else { continue };
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
    let types: BTreeMap<Option<String>, usize> = g.edge_types().map(|(name, n)| (name.map(str::to_owned), n)).collect();
    let truncated = labels.len() > MAX_NAMES || types.len() > MAX_NAMES;
    let value = Schema {
        labels: labels
            .into_iter()
            .take(MAX_NAMES)
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
        types: types.into_iter().take(MAX_NAMES).map(|(name, count)| TypeInfo { name, count }).collect(),
        nodes: g.node_count(),
        edges: g.edge_count(),
        sampled_nodes,
        sampled_edges: g.edge_count(),
    };
    let work = Work { visited: sampled_nodes, edges: 0 };
    Ok(Answer { work, truncated, ..Answer::at(ns.seq(), value) })
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
        // A sample of one node: every label is still listed, with its exact
        // count; only the sampled one has keys
        let one = read(&ns, 1);
        assert_eq!(one.sampled_nodes, 1);
        let listed: Vec<_> = one.labels.iter().map(|l| (l.name.as_str(), l.count, l.sampled)).collect();
        assert_eq!(listed, [("Thing", 2, 0), ("Wide", 1, 1)]);
    }
}
