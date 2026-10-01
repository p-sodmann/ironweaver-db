//! The invariants every namespace keeps, checked from scratch: what
//! `verify` (step 7, ADR 0011) checks on every checkpoint and on the state
//! the WAL replays to. The commit pipeline keeps them on every commit; a
//! violation means a bug or a file changed behind the database's back.

use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;

use ironweaver_core::{Date, DateTime, GraphError, Key, NodeIx, Value};

use crate::reserved::{check_user_attrs, check_user_meta};
use crate::resolve::{check_existing, too_deep, value_at};
use crate::{DbRecord, Namespace};

/// The most violations [`check`] reports (the rest are counted, not
/// listed), so that a badly damaged graph doesn't produce millions.
pub const MAX_REPORTED: usize = 100;

/// One value of each kind of index key, to split an index into the
/// ranges below and above it: together they hold every entry of the kind.
fn one_of_each_kind() -> Vec<Value> {
    vec![
        Value::Bool(false),
        Value::Int(0),
        Value::String(String::new()),
        Value::Bytes(Vec::new()),
        Value::Date(Date(0)),
        Value::DateTime(DateTime { micros: 0, offset: None }),
        Value::DateTime(DateTime { micros: 0, offset: Some(0) }),
    ]
}

/// Check the namespace's invariants. Returns the violations found, each a
/// sentence naming the entity (at most [`MAX_REPORTED`], then a count of
/// the rest); empty if there are none.
///
/// - every edge's endpoints are live nodes;
/// - every node and edge has a version from 1 to `i64::MAX`;
/// - no top-level attribute key and no meta key is reserved (`iwdb.*`),
///   and no value is nested deeper than
///   [`MAX_VALUE_DEPTH`](crate::mutation::MAX_VALUE_DEPTH);
/// - the graph's property indexes are exactly the catalog's
///   ([`index_paths`](crate::catalog::NamespaceCatalog::index_paths)) and
///   flushed, and each one's contents equal a scan: every node with an
///   indexable value at the path is found under that value and nowhere
///   else, and the index holds no other entry;
/// - every constraint of the catalog holds;
/// - the idempotency key table (step 8) has at most
///   [`KEY_TABLE_CAPACITY`](crate::idempotency::KEY_TABLE_CAPACITY)
///   entries, all at seqs up to the namespace's.
///
/// O((n + m) · number of indexes) time, plus a lookup per indexed node.
pub fn check(ns: &Namespace) -> Vec<String> {
    let mut found = Violations::default();
    let g = ns.graph();

    let keys = ns.keys();
    if keys.len() > crate::idempotency::KEY_TABLE_CAPACITY {
        found.push(|| format!("the idempotency key table has {} entries, more than it may hold", keys.len()));
    }
    for entry in keys.entries().filter(|e| e.result.seq > ns.seq()) {
        found.push(|| format!("idempotency key {} is at seq {}, after the state's", entry.key, entry.result.seq));
    }

    for (_, edge) in g.edges() {
        let id = edge.id().0;
        if g.node(edge.source()).is_none() || g.node(edge.target()).is_none() {
            found.push(|| format!("edge {} has an endpoint that is not a live node", id));
        }
        check_record(&mut found, &format!("edge {}", id), &edge.data);
    }
    for (_, node) in g.nodes() {
        check_record(&mut found, &format!("node '{}'", node.id()), &node.data);
    }

    let wanted: BTreeSet<&[String]> = ns.catalog().index_paths().into_iter().map(|p| p.keys()).collect();
    let have: BTreeSet<&[String]> = g.index_paths().into_iter().collect();
    for path in wanted.difference(&have) {
        found.push(|| format!("the catalog's index on {:?} is missing from the graph", path));
    }
    for path in have.difference(&wanted) {
        found.push(|| format!("the graph has an index on {:?} that the catalog doesn't declare", path));
    }
    if g.indexes_dirty() {
        found.push(|| "the graph's indexes are not flushed".to_owned());
    } else {
        for path in wanted.intersection(&have) {
            if let Err(e) = check_index(&mut found, ns, path) {
                found.push(|| format!("the index on {:?} can't be read: {}", path, e));
            }
        }
    }

    for constraint in ns.catalog().constraints() {
        if let Err(e) = check_existing(g, constraint) {
            found.push(|| e.to_string());
        }
    }
    found.finish()
}

fn check_record(found: &mut Violations, entity: &str, data: &DbRecord) {
    if data.version == 0 || i64::try_from(data.version).is_err() {
        found.push(|| format!("{} has version {}, outside 1 ..= {}", entity, data.version, i64::MAX));
    }
    if let Err(e) = check_user_attrs(&data.attr).and_then(|()| check_user_meta(&data.meta)) {
        found.push(|| format!("{}: {}", entity, e));
    }
    for (key, value) in data.attr.iter().chain(&data.meta) {
        if too_deep(value, 1) {
            found.push(|| format!("{}: the value of '{}' is nested too deeply", entity, key));
        }
    }
}

/// Compare the index on `path` with a scan of the nodes.
fn check_index(found: &mut Violations, ns: &Namespace, path: &[String]) -> Result<(), GraphError> {
    let g = ns.graph();
    // What the index must hold: each node with an indexable value, by key
    let mut keys: HashMap<NodeIx, Key> = HashMap::new();
    let mut by_key: HashMap<Key, (Value, Vec<NodeIx>)> = HashMap::new();
    for (ix, node) in g.nodes() {
        let Some(value) = value_at(&node.data, path)? else { continue };
        let Some(key) = Key::of(&value) else { continue };
        keys.insert(ix, key.clone());
        by_key.entry(key).or_insert_with(|| (value, Vec::new())).1.push(ix);
    }
    let name = |ix: NodeIx| g.node(ix).map_or_else(|| format!("{:?}", ix), |n| format!("'{}'", n.id()));
    // Under each key: exactly the nodes with it
    for (value, nodes) in by_key.values() {
        let mut expected = nodes.clone();
        expected.sort_unstable_by_key(|ix| ix.slot());
        let listed = g.find_nodes(path, value)?.unwrap_or_default();
        if listed != expected {
            let (missing, extra) = differences(&expected, &listed);
            found.push(|| {
                format!(
                    "the index on {:?} under {:?} lacks {:?} and wrongly holds {:?}",
                    path,
                    value,
                    missing.into_iter().map(name).collect::<Vec<_>>(),
                    extra.into_iter().map(name).collect::<Vec<_>>()
                )
            });
        }
    }
    // Nothing else: every entry, of every kind, belongs to a node with that key
    let mut entries = 0;
    for value in one_of_each_kind() {
        for (lo, hi) in [(Bound::Unbounded, Bound::Excluded(&value)), (Bound::Included(&value), Bound::Unbounded)] {
            for ix in g.find_nodes_in_range::<GraphError>(path, lo, hi)?.unwrap_or_default() {
                entries += 1;
                if !keys.contains_key(&ix) {
                    found
                        .push(|| format!("the index on {:?} holds node {} without an indexable value", path, name(ix)));
                }
            }
        }
    }
    if entries != keys.len() {
        found.push(|| format!("the index on {:?} has {} entries for {} indexed nodes", path, entries, keys.len()));
    }
    Ok(())
}

/// Whether two namespaces hold the same state: seq, catalog, idempotency
/// key table, and the same nodes (ids, labels, payloads with versions) and
/// edges (ids, endpoints, types, payloads). Payloads compare in their canonical form, so `NaN`
/// equals `NaN` and `-0.0` differs from `0.0`. Iteration order and the
/// edge id counter don't count ([`canonical`](crate::testutil::canonical)
/// leaves them out too). Returns the first difference found.
///
/// O(n + m) lookups and no copy of either graph.
pub fn compare(a: &Namespace, b: &Namespace) -> Result<(), String> {
    use crate::testutil::Canonical;
    if a.seq() != b.seq() {
        return Err(format!("seq {} differs from {}", a.seq(), b.seq()));
    }
    if a.catalog() != b.catalog() {
        return Err(format!("the catalogs differ: {:?} and {:?}", a.catalog(), b.catalog()));
    }
    if let Some(difference) = a.keys().difference(b.keys()) {
        return Err(format!("the idempotency key tables differ: {}", difference));
    }
    let (ga, gb) = (a.graph(), b.graph());
    if (ga.node_count(), ga.edge_count()) != (gb.node_count(), gb.edge_count()) {
        return Err(format!(
            "{} nodes and {} edges differ from {} nodes and {} edges",
            ga.node_count(),
            ga.edge_count(),
            gb.node_count(),
            gb.edge_count()
        ));
    }
    let labels = |g: &crate::DbGraph, ix| {
        let mut labels = g.label_names(ix).unwrap_or_default();
        labels.sort_unstable();
        labels.into_iter().map(str::to_owned).collect::<Vec<String>>()
    };
    for (ix, node) in ga.nodes() {
        let id = node.id();
        let other = gb.node_ix(id).and_then(|o| gb.node(o).map(|n| (o, n)));
        let Some((ox, other)) = other else { return Err(format!("node '{}' is missing", id)) };
        if labels(ga, ix) != labels(gb, ox) || node.data.canonical() != other.data.canonical() {
            return Err(format!("node '{}' differs", id));
        }
    }
    for (ix, edge) in ga.edges() {
        let id = edge.id();
        let Some(ox) = gb.edge_ix(id) else { return Err(format!("edge {} is missing", id.0)) };
        let Some(other) = gb.edge(ox) else { return Err(format!("edge {} is missing", id.0)) };
        let ends = |g: &crate::DbGraph, e: &ironweaver_core::Edge<DbRecord>| {
            (g.node(e.source()).map(|n| n.id().to_owned()), g.node(e.target()).map(|n| n.id().to_owned()))
        };
        if ends(ga, edge) != ends(gb, other)
            || ga.edge_type_name(ix) != gb.edge_type_name(ox)
            || edge.data.canonical() != other.data.canonical()
        {
            return Err(format!("edge {} differs", id.0));
        }
    }
    Ok(())
}

/// What `expected` has that `actual` lacks, and the other way round.
fn differences(expected: &[NodeIx], actual: &[NodeIx]) -> (Vec<NodeIx>, Vec<NodeIx>) {
    let missing = expected.iter().filter(|ix| !actual.contains(ix)).copied().collect();
    let extra = actual.iter().filter(|ix| !expected.contains(ix)).copied().collect();
    (missing, extra)
}

/// Violations, at most [`MAX_REPORTED`] of them listed.
#[derive(Default)]
struct Violations {
    listed: Vec<String>,
    more: usize,
}

impl Violations {
    fn push(&mut self, describe: impl FnOnce() -> String) {
        if self.listed.len() < MAX_REPORTED {
            self.listed.push(describe());
        } else {
            self.more += 1;
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.more > 0 {
            self.listed.push(format!("... and {} more violations", self.more));
        }
        self.listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label, NamespaceName};
    use crate::codec::{self, GraphMeta};
    use crate::{CatalogChange, DbGraph, Mutation};
    use ironweaver_core::{Attrs, EdgeId, Op};

    fn upsert(id: &str, labels: &[&str], attr: Attrs) -> Mutation {
        Mutation::UpsertNode {
            id: id.into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            attr,
            meta: Attrs::new(),
            expected_version: None,
        }
    }

    fn attr(key: &str, value: Value) -> Attrs {
        [(key.to_owned(), value)].into()
    }

    /// A namespace with data of every index key kind, an index and a
    /// unique constraint.
    fn sample() -> Namespace {
        let mut ns = Namespace::new(NamespaceName::new("n").expect("name"));
        let path = AttrPath::new(["k"]).expect("path");
        ns.commit_catalog(CatalogChange::CreateIndex(IndexDef { path: path.clone() })).expect("index");
        let unique = Constraint { kind: ConstraintKind::Unique, label: Label::new("U").expect("label"), path };
        ns.commit_catalog(CatalogChange::AddConstraint(unique)).expect("constraint");
        let values = [
            Value::Bool(true),
            Value::Int(-3),
            Value::Float(2.5),
            Value::String("s".into()),
            Value::Bytes(vec![1]),
            Value::Date(Date(5)),
            Value::DateTime(DateTime { micros: -7, offset: None }),
            Value::DateTime(DateTime { micros: 9, offset: Some(3600) }),
            Value::Int(-3),
            Value::List(vec![]),
            Value::None,
        ];
        let mut mutations: Vec<Mutation> =
            values.into_iter().enumerate().map(|(i, v)| upsert(&format!("n{}", i), &[], attr("k", v))).collect();
        mutations.push(upsert("u1", &["U"], attr("k", Value::from("one"))));
        mutations.push(Mutation::AddEdge {
            from: "n0".into(),
            to: "n1".into(),
            ty: None,
            attr: Attrs::new(),
            meta: Attrs::new(),
        });
        ns.commit(&mutations).expect("commit");
        ns
    }

    /// Save and load `graph` with the namespace's meta (a checkpoint
    /// written behind the database's back).
    fn reload(graph: &DbGraph, meta: &GraphMeta) -> Namespace {
        Namespace::from_loaded(codec::from_binary(&codec::to_binary(graph, meta).expect("save")).expect("load"))
    }

    /// `graph` as the namespace's, without saving it.
    fn in_memory(graph: DbGraph, ns: &Namespace) -> Namespace {
        Namespace::from_loaded(codec::Loaded { graph, meta: ns.graph_meta(), index_changes: Default::default() })
    }

    #[test]
    fn a_namespace_the_pipeline_built_has_no_violations() {
        let ns = sample();
        assert_eq!(check(&ns), Vec::<String>::new());
        assert_eq!(check(&reload(ns.graph(), &ns.graph_meta())), Vec::<String>::new());
    }

    #[test]
    fn versions_out_of_range_are_found() {
        let ns = sample();
        let mut graph = ns.graph().clone();
        let n0 = graph.node_ix("n0").expect("n0");
        graph.node_mut(n0).expect("node").data.version = 0;
        let e = graph.edge_ix(EdgeId(0)).expect("edge");
        // Above i64::MAX: a file can't even hold it, so only in memory
        graph.edge_mut(e).expect("edge").data.version = u64::MAX;
        graph.flush_indexes().expect("flush");
        let found = check(&in_memory(graph, &ns));
        assert_eq!(found.len(), 2, "{:?}", found);
        assert!(found.iter().any(|v| v.contains("node 'n0' has version 0")), "{:?}", found);
        assert!(found.iter().any(|v| v.contains("edge 0 has version")), "{:?}", found);
    }

    #[test]
    fn a_violated_constraint_and_a_missing_index_are_found() {
        let ns = sample();
        let mut graph = ns.graph().clone();
        let mut data = DbRecord::with_attr([("k", Value::from("one"))]);
        data.version = 1;
        graph.apply(Op::AddNode { id: "u2".into(), labels: vec!["U".into()], data }).expect("add");
        graph.drop_index(&["k".to_owned()]);
        let found = check(&reload(&graph, &ns.graph_meta()));
        // The loader rebuilds the index from the catalog, so only the
        // constraint is violated
        assert_eq!(found.len(), 1, "{:?}", found);
        assert!(found[0].contains("unique constraint"), "{:?}", found);

        // In memory, a missing index is found too
        let mut ns = reload(&graph, &ns.graph_meta());
        let catalog = ns.catalog().clone();
        let path = AttrPath::new(["other"]).expect("path");
        let mut changed = catalog.clone();
        changed.add_index(IndexDef { path });
        ns = Namespace::from_loaded(codec::Loaded {
            graph: ns.graph().clone(),
            meta: GraphMeta { catalog: changed, ..ns.graph_meta() },
            index_changes: Default::default(),
        });
        let found = check(&ns);
        assert!(found.iter().any(|v| v.contains("missing from the graph")), "{:?}", found);
    }

    #[test]
    fn index_contents_that_differ_from_a_scan_are_found() {
        let ns = sample();
        let mut graph = ns.graph().clone();
        let path = ["k".to_owned()];
        // A stale entry: the index says n3 holds 7 (and not "s")
        let n3 = graph.node_ix("n3").expect("n3");
        graph.set_index_keys(n3, vec![Key::of(&Value::Int(7))]).expect("keys");
        let ns = in_memory(graph, &ns);
        assert!(!ns.graph().indexes_dirty());
        assert!(ns.graph().find_nodes(&path, &Value::from("s")).expect("find").expect("indexed").is_empty());
        let found = check(&ns);
        assert!(found.iter().any(|v| v.contains("lacks [\"'n3'\"]")), "{:?}", found);
    }

    #[test]
    fn compare_finds_any_difference() {
        let a = sample();
        let b = reload(a.graph(), &a.graph_meta());
        assert_eq!(compare(&a, &b), Ok(()));
        let mut changed = a.graph().clone();
        let n2 = changed.node_ix("n2").expect("n2");
        changed.node_mut(n2).expect("node").data.attr.insert("k".into(), Value::Float(-0.0));
        changed.flush_indexes().expect("flush");
        assert_eq!(compare(&a, &in_memory(changed, &a)), Err("node 'n2' differs".into()));
        let mut c = sample();
        c.commit(&[upsert("x", &[], Attrs::new())]).expect("commit");
        assert!(compare(&a, &c).is_err());
    }

    #[test]
    fn many_violations_are_capped() {
        let mut ns = Namespace::new(NamespaceName::new("n").expect("name"));
        let mutations: Vec<Mutation> = (0..150).map(|i| upsert(&format!("n{}", i), &[], Attrs::new())).collect();
        ns.commit(&mutations).expect("commit");
        let mut graph = ns.graph().clone();
        let ixs: Vec<NodeIx> = graph.nodes().map(|(ix, _)| ix).collect();
        for ix in ixs {
            graph.node_mut(ix).expect("node").data.version = 0;
        }
        let found = check(&reload(&graph, &ns.graph_meta()));
        assert_eq!(found.len(), MAX_REPORTED + 1);
        assert_eq!(found.last().map(String::as_str), Some("... and 50 more violations"));
    }
}
