//! Helpers for tests: an order-independent, canonical form of a graph.
//!
//! The core's iteration order is not part of our contract (slots are reused,
//! save/load compacts them, rollback can reorder adjacency lists). Tests that
//! compare two graphs, e.g. before a crash and after recovery, compare their
//! [`canonical`] forms instead.

use std::fmt::Write as _;

use ironweaver_core::{Attrs, Graph, Record, Value};

use crate::DbRecord;

/// A payload that can be rendered in a canonical, order-independent form.
///
/// Two payloads with equal state must render to the same string, whatever
/// the iteration order of their maps.
pub trait Canonical {
    fn canonical(&self) -> String;
}

impl Canonical for Record {
    fn canonical(&self) -> String {
        format!("attr={} meta={}", canonical_attrs(&self.attr), canonical_attrs(&self.meta))
    }
}

impl Canonical for DbRecord {
    fn canonical(&self) -> String {
        format!("attr={} meta={} version={}", canonical_attrs(&self.attr), canonical_attrs(&self.meta), self.version)
    }
}

/// The canonical form of a graph: one line per node and per edge, sorted.
///
/// - `node <id> labels=[..] <payload>`, labels sorted by name;
/// - `edge <edge id> <from> -> <to> type=<type> <payload>`.
///
/// Covers ids, labels, edge types, endpoints, edge ids and payloads. It
/// deliberately leaves out slot order, adjacency order and the next edge id
/// counter: the counter may advance without any edge being added (a failed
/// `apply_all` that added an edge before failing keeps it raised), and the
/// database assigns explicit edge ids, so the counter is not observable
/// state. Complexity: O((n + m) log(n + m)).
pub fn canonical<N: Canonical, E: Canonical>(g: &Graph<N, E>) -> Vec<String> {
    let mut lines = Vec::with_capacity(g.node_count() + g.edge_count());
    for (ix, node) in g.nodes() {
        let mut labels = g.label_names(ix).unwrap_or_default();
        labels.sort_unstable();
        lines.push(format!("node {:?} labels={:?} {}", node.id(), labels, node.data.canonical()));
    }
    for (ix, edge) in g.edges() {
        let name = |n| g.node(n).map_or("<stale>", |n| n.id());
        lines.push(format!(
            "edge {} {:?} -> {:?} type={:?} {}",
            edge.id().0,
            name(edge.source()),
            name(edge.target()),
            g.edge_type_name(ix),
            edge.data.canonical()
        ));
    }
    lines.sort_unstable();
    lines
}

/// An attribute map with its keys sorted, nested dicts included.
pub fn canonical_attrs(attrs: &Attrs) -> String {
    let mut keys: Vec<&String> = attrs.keys().collect();
    keys.sort_unstable();
    let mut out = String::from("{");
    for (i, key) in keys.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{:?}: ", key);
        if let Some(value) = attrs.get(key) {
            out.push_str(&canonical_value(value));
        }
    }
    out.push('}');
    out
}

/// A value with dict keys sorted. Scalars use their `Debug` form, which
/// keeps the variant (`Int(1)` differs from `Float(1.0)`).
pub fn canonical_value(value: &Value) -> String {
    match value {
        Value::List(items) => format!("[{}]", items.iter().map(canonical_value).collect::<Vec<_>>().join(", ")),
        Value::Dict(entries) => canonical_attrs(entries),
        other => format!("{:?}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironweaver_core::{EdgeId, Op};

    fn build(order: &[&str]) -> Graph<Record, Record> {
        let mut g = Graph::new();
        for id in order {
            let mut attr = Attrs::new();
            attr.insert("name".into(), Value::String(id.to_string()));
            attr.insert(
                "nested".into(),
                Value::Dict([("b".into(), Value::Int(2)), ("a".into(), Value::Int(1))].into()),
            );
            g.apply(Op::AddNode {
                id: id.to_string(),
                labels: vec!["Z".into(), "A".into()],
                data: Record { attr, meta: Attrs::new() },
            })
            .unwrap();
        }
        g.apply(Op::AddEdge {
            id: EdgeId(7),
            from: "a".into(),
            to: "b".into(),
            ty: Some("KNOWS".into()),
            data: Record::default(),
        })
        .unwrap();
        g
    }

    #[test]
    fn independent_of_insertion_order() {
        assert_eq!(canonical(&build(&["a", "b", "c"])), canonical(&build(&["c", "b", "a"])));
    }

    #[test]
    fn sees_payload_and_structure_differences() {
        let base = canonical(&build(&["a", "b"]));
        let mut g = build(&["a", "b"]);
        g.apply(Op::SetNodeAttr { id: "a".into(), key: "x".into(), value: Some(Value::Int(1)) }).unwrap();
        assert_ne!(canonical(&g), base);
        let mut g = build(&["a", "b"]);
        g.apply(Op::SetEdgeType { id: EdgeId(7), ty: None }).unwrap();
        assert_ne!(canonical(&g), base);
        let mut g = build(&["a", "b"]);
        g.apply(Op::RemoveLabel { id: "b".into(), label: "Z".into() }).unwrap();
        assert_ne!(canonical(&g), base);
    }

    #[test]
    fn db_records_include_the_version() {
        let a = DbRecord { version: 1, ..DbRecord::with_attr([("x", Value::Int(1))]) };
        let b = DbRecord { version: 2, ..a.clone() };
        assert_ne!(a.canonical(), b.canonical());
        assert_eq!(a.canonical(), a.clone().canonical());
        assert!(a.canonical().ends_with("version=1"));
    }

    #[test]
    fn distinguishes_value_kinds() {
        assert_ne!(canonical_value(&Value::Int(1)), canonical_value(&Value::Float(1.0)));
        assert_ne!(canonical_value(&Value::String("1".into())), canonical_value(&Value::Int(1)));
    }
}
