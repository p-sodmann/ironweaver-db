//! Step 2 acceptance: `Graph<DbRecord, DbRecord>` saves and loads through
//! the core's binary and JSON formats with versions and catalog, and the
//! core's filters, indexes and analytics work on it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::ops::Bound::{Excluded, Included};

use ironweaver_core::algo::{pagerank, PageRank};
use ironweaver_core::format::{self, GraphWriter, RecordCodec};
use ironweaver_core::pathfinding::EdgeCost;
use ironweaver_core::{
    Attrs, CmpOp, Direction, EdgeId, Expr, Graph, GraphError, NodeIx, Op, Projection, Record, Value,
};
use iwdb_engine::catalog::{
    AttrPath, CatalogError, Constraint, ConstraintKind, IndexDef, Label, NamespaceCatalog, NamespaceName,
};
use iwdb_engine::codec::{self, GraphMeta, Loaded};
use iwdb_engine::testutil::canonical;
use iwdb_engine::{DbGraph, DbRecord, Entity, Error};
use proptest::prelude::*;

type O = Op<DbRecord, DbRecord>;

fn path(keys: &[&str]) -> AttrPath {
    AttrPath::new(keys.iter().copied()).unwrap()
}

fn keys(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

fn rec(version: u64, attr: impl IntoIterator<Item = (&'static str, Value)>) -> DbRecord {
    DbRecord { version, ..DbRecord::with_attr(attr) }
}

fn ids(g: &DbGraph, ixs: impl IntoIterator<Item = NodeIx>) -> Vec<String> {
    let mut out: Vec<String> = ixs.into_iter().filter_map(|ix| g.node(ix)).map(|n| n.id().to_owned()).collect();
    out.sort();
    out
}

/// A small social graph with versions, user meta and a catalog.
fn social() -> (DbGraph, GraphMeta) {
    let mut meta_alice = rec(3, [("name", Value::from("alice")), ("age", Value::Int(30))]);
    meta_alice.meta.insert("source".into(), Value::from("import"));
    let ops: Vec<O> = vec![
        Op::AddNode { id: "alice".into(), labels: vec!["Person".into()], data: meta_alice },
        Op::AddNode {
            id: "bob".into(),
            labels: vec!["Person".into(), "Admin".into()],
            data: rec(1, [("name", Value::from("bob")), ("age", Value::Int(25)), ("email", Value::from("b@x"))]),
        },
        Op::AddNode {
            id: "carol".into(),
            labels: vec!["Person".into()],
            data: rec(u64::MAX >> 1, [("name", Value::from("carol")), ("age", Value::Float(41.0))]),
        },
        Op::AddNode {
            id: "acme".into(),
            labels: vec!["Company".into()],
            data: rec(1, [("name", Value::from("acme"))]),
        },
        Op::AddEdge {
            id: EdgeId(0),
            from: "alice".into(),
            to: "bob".into(),
            ty: Some("KNOWS".into()),
            data: rec(2, [("weight", Value::Float(0.5))]),
        },
        Op::AddEdge {
            id: EdgeId(5),
            from: "bob".into(),
            to: "carol".into(),
            ty: Some("KNOWS".into()),
            data: rec(1, [("weight", Value::Float(2.0))]),
        },
        Op::AddEdge {
            id: EdgeId(9),
            from: "carol".into(),
            to: "acme".into(),
            ty: Some("WORKS_AT".into()),
            data: rec(1, []),
        },
        Op::AddEdge { id: EdgeId(10), from: "acme".into(), to: "alice".into(), ty: None, data: rec(7, []) },
    ];
    let mut g = DbGraph::new();
    g.apply_all(ops).unwrap();

    let mut catalog = NamespaceCatalog::new();
    catalog.add_index(IndexDef { path: path(&["age"]) });
    catalog.add_constraint(Constraint {
        kind: ConstraintKind::Unique,
        label: Label::new("Person").unwrap(),
        path: path(&["email"]),
    });
    catalog.add_constraint(Constraint {
        kind: ConstraintKind::Required,
        label: Label::new("Person").unwrap(),
        path: path(&["name"]),
    });
    (g, GraphMeta { namespace: NamespaceName::new("social").unwrap(), catalog, seq: 17, keys: Default::default() })
}

/// Every way to load `g` saved with `meta`.
fn all_loads(g: &DbGraph, meta: &GraphMeta) -> Vec<(&'static str, Loaded)> {
    let binary = codec::to_binary(g, meta).unwrap();
    let json = codec::to_json(g, meta, false).unwrap();
    let pretty = codec::to_json(g, meta, true).unwrap();
    vec![
        ("binary", codec::from_binary(&binary).unwrap()),
        ("binary reader", codec::from_binary_reader(&binary[..]).unwrap()),
        ("json", codec::from_json(&json).unwrap()),
        ("pretty json", codec::from_json(&pretty).unwrap()),
    ]
}

fn assert_round_trip(g: &DbGraph, meta: &GraphMeta) {
    let want: Vec<Vec<String>> = meta.catalog.index_paths().into_iter().map(|p| p.keys().to_vec()).collect();
    for (how, loaded) in all_loads(g, meta) {
        assert_eq!(canonical(&loaded.graph), canonical(g), "{}", how);
        assert_eq!(loaded.graph.next_edge_id(), g.next_edge_id(), "{}", how);
        assert_eq!(&loaded.meta, meta, "{}", how);
        let mut have: Vec<Vec<String>> = loaded.graph.index_paths().into_iter().map(<[String]>::to_vec).collect();
        have.sort();
        assert_eq!(have, want, "{}: indexes follow the catalog", how);
        assert!(!loaded.graph.indexes_dirty(), "{}", how);
    }
}

#[test]
fn round_trips_with_versions_and_catalog() {
    let (g, meta) = social();
    assert_round_trip(&g, &meta);
}

proptest! {
    #[test]
    fn random_graphs_round_trip(ops in common::graph_ops(), meta in common::graph_meta()) {
        let g = common::build(ops);
        assert_round_trip(&g, &meta);
    }

    #[test]
    fn saves_are_deterministic(ops in common::graph_ops(), meta in common::graph_meta()) {
        // Two graphs from the same ops have attribute maps with different
        // hash seeds, so their iteration orders differ
        let (a, b) = (common::build(ops.clone()), common::build(ops));
        prop_assert_eq!(codec::to_binary(&a, &meta).unwrap(), codec::to_binary(&b, &meta).unwrap());
        prop_assert_eq!(codec::to_json(&a, &meta, false).unwrap(), codec::to_json(&b, &meta, false).unwrap());
        // A loaded graph saves again to a file with the same state (not
        // necessarily the same bytes: loading compacts slots)
        let bytes = codec::to_binary(&a, &meta).unwrap();
        let again = codec::to_binary(&codec::from_binary(&bytes).unwrap().graph, &meta).unwrap();
        prop_assert_eq!(codec::from_binary(&again).map(|l| canonical(&l.graph)).unwrap(), canonical(&a));
        // ... and from then on saves are stable
        let third = codec::to_binary(&codec::from_binary(&again).unwrap().graph, &meta).unwrap();
        prop_assert_eq!(third, again);
        // `GraphWriter::with_timestamp(None)` directly gives the same bytes
        let mut direct = Vec::new();
        GraphWriter::new(&a, &codec::DbCodec::new(&meta)).with_timestamp(None).write_binary(&mut direct).unwrap();
        prop_assert_eq!(direct, bytes);
    }
}

/// `g` rebuilt with payloads `conv(..)`, in the same slot and adjacency
/// order (for graphs without removed nodes or edges).
fn convert(g: &DbGraph, conv: impl Fn(&DbRecord) -> Record) -> Graph<Record, Record> {
    let mut out = Graph::new();
    let mut ops: Vec<Op<Record, Record>> = Vec::new();
    for (ix, node) in g.nodes() {
        let labels = g.label_names(ix).unwrap().into_iter().map(str::to_owned).collect();
        ops.push(Op::AddNode { id: node.id().to_owned(), labels, data: conv(&node.data) });
    }
    for (ix, edge) in g.edges() {
        ops.push(Op::AddEdge {
            id: edge.id(),
            from: g.node(edge.source()).unwrap().id().to_owned(),
            to: g.node(edge.target()).unwrap().id().to_owned(),
            ty: g.edge_type_name(ix).map(str::to_owned),
            data: conv(&edge.data),
        });
    }
    out.apply_all(ops).unwrap();
    out
}

/// The same graph as `Record`s, with the version as an `iwdb.version`
/// meta entry.
fn as_records(g: &DbGraph) -> Graph<Record, Record> {
    convert(g, |r| {
        let mut meta = r.meta.clone();
        meta.insert("iwdb.version".into(), Value::Int(r.version as i64));
        Record { attr: r.attr.clone(), meta }
    })
}

#[test]
fn files_are_ordinary_ironweaver_files() {
    let (g, meta) = social();
    let records = as_records(&g);
    let graph_meta = meta.to_attrs();
    let bytes = codec::to_binary(&g, &meta).unwrap();
    // Byte for byte what the core's own codec writes for the Record view
    let mut core = Vec::new();
    GraphWriter::new(&records, &RecordCodec { meta: &graph_meta, half: false })
        .with_timestamp(None)
        .write_binary(&mut core)
        .unwrap();
    assert_eq!(bytes, core);
    let json = codec::to_json(&g, &meta, false).unwrap();
    let core_json =
        GraphWriter::new(&records, &RecordCodec { meta: &graph_meta, half: false }).with_timestamp(None).to_json(false);
    assert_eq!(json, core_json.unwrap());

    // And the core reads them as Record graphs
    let (loaded, loaded_meta) = format::from_binary(&bytes).unwrap();
    assert_eq!(canonical(&loaded), canonical(&records));
    assert_eq!(loaded_meta, graph_meta);
}

/// Save a `Record` graph with the core's codec, as a foreign or damaged
/// file would be written.
fn foreign(g: &Graph<Record, Record>, meta: &Attrs) -> (Vec<u8>, Vec<u8>) {
    let codec = RecordCodec { meta, half: false };
    let writer = GraphWriter::new(g, &codec).with_timestamp(None);
    let mut binary = Vec::new();
    writer.write_binary(&mut binary).unwrap();
    (binary, writer.to_json(false).unwrap())
}

/// Load a foreign file in every way; all must fail with the same error.
fn load_error(g: &Graph<Record, Record>, meta: &Attrs) -> Error {
    let (binary, json) = foreign(g, meta);
    let errors = [
        codec::from_binary(&binary).unwrap_err(),
        codec::from_binary_reader(&binary[..]).unwrap_err(),
        codec::from_json(&json).unwrap_err(),
    ];
    assert_eq!(errors[0], errors[1], "slice and streaming loads agree");
    assert_eq!(errors[0], errors[2], "binary and JSON loads agree");
    errors[0].clone()
}

#[test]
fn bad_versions_are_errors_in_every_loader() {
    let (g, meta) = social();
    let graph_meta = meta.to_attrs();
    let tamper = |node: bool, value: Option<Value>| {
        let mut records = as_records(&g);
        let target = if node {
            &mut records.node_mut(records.node_ix("bob").unwrap()).unwrap().data.meta
        } else {
            &mut records.edge_mut(records.edge_ix(EdgeId(5)).unwrap()).unwrap().data.meta
        };
        match value {
            Some(v) => target.insert("iwdb.version".into(), v),
            None => target.remove("iwdb.version"),
        };
        load_error(&records, &graph_meta)
    };
    let bob = || Entity::Node("bob".into());
    assert_eq!(tamper(true, None), Error::MissingVersion { entity: bob() });
    assert_eq!(tamper(false, None), Error::MissingVersion { entity: Entity::Edge("5".into()) });
    for bad in [Value::Int(-1), Value::from("1"), Value::Float(1.0), Value::None, Value::List(vec![Value::Int(1)])] {
        let found = format!("{:?}", bad);
        assert_eq!(tamper(true, Some(bad)), Error::InvalidVersion { entity: bob(), found });
    }
    let err = tamper(true, Some(Value::Int(-5)));
    assert_eq!(err.to_string(), "node 'bob' has an invalid 'iwdb.version' meta entry: Int(-5)");
}

#[test]
fn unknown_reserved_keys_and_foreign_graph_meta_are_errors() {
    let (g, meta) = social();
    let graph_meta = meta.to_attrs();

    let mut records = as_records(&g);
    let alice = records.node_ix("alice").unwrap();
    records.node_mut(alice).unwrap().data.meta.insert("iwdb.future".into(), Value::Int(1));
    assert_eq!(
        load_error(&records, &graph_meta),
        Error::UnknownReservedKey { entity: Entity::Node("alice".into()), key: "iwdb.future".into() }
    );

    let records = as_records(&g);
    assert_eq!(load_error(&records, &Attrs::new()), Error::Catalog(CatalogError::Missing));
    let mut extra = graph_meta.clone();
    extra.insert("owner".into(), Value::from("me"));
    assert_eq!(load_error(&records, &extra), Error::UnexpectedGraphMeta { key: "owner".into() });
    let mut extra = graph_meta.clone();
    extra.insert("iwdb.replica".into(), Value::Bool(true));
    assert_eq!(
        load_error(&records, &extra),
        Error::UnknownReservedKey { entity: Entity::Graph, key: "iwdb.replica".into() }
    );
    let bad_catalog: Attrs = [("iwdb.catalog".to_owned(), Value::from(r#"{"format": 9, "namespace": "a"}"#))].into();
    assert_eq!(load_error(&records, &bad_catalog), Error::Catalog(CatalogError::UnsupportedFormat { found: 9 }));

    // A plain Record graph (no versions) is not a database file
    let plain = convert(&g, |r| Record { attr: r.attr.clone(), meta: Attrs::new() });
    assert!(matches!(load_error(&plain, &graph_meta), Error::MissingVersion { .. }));
}

#[test]
fn the_seq_in_graph_meta_is_required_and_checked() {
    let (g, meta) = social();
    let records = as_records(&g);
    let with_seq = |seq: Option<Value>| {
        let mut attrs = meta.to_attrs();
        attrs.remove("iwdb.seq");
        if let Some(seq) = seq {
            attrs.insert("iwdb.seq".into(), seq);
        }
        attrs
    };
    assert_eq!(load_error(&records, &with_seq(None)), Error::MissingSeq);
    for (value, found) in [(Value::Int(-1), "Int(-1)"), (Value::from("7"), "String(\"7\")"), (Value::None, "None")] {
        assert_eq!(load_error(&records, &with_seq(Some(value))), Error::InvalidSeq { found: found.into() });
    }

    // The largest seq an Int holds round-trips; a larger one isn't saved
    let top = GraphMeta { seq: i64::MAX as u64, ..meta.clone() };
    assert_eq!(codec::from_binary(&codec::to_binary(&g, &top).unwrap()).unwrap().meta.seq, i64::MAX as u64);
    let over = GraphMeta { seq: i64::MAX as u64 + 1, ..meta };
    let err = codec::to_binary(&g, &over).unwrap_err().to_string();
    assert!(err.contains("too large to save"), "{}", err);
}

/// Fixed upstream (#26, #46): JSON keeps `-0.0` (the loader read it as
/// `0.0`), and NaN and the infinities round-trip as `"NaN"`, `"Infinity"`
/// and `"-Infinity"` through `DbCodec` (they were written as `null`), like
/// the binary format.
#[test]
fn json_keeps_negative_zero_nan_and_infinities() {
    let (_, meta) = social();
    let floats = [-0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
    let mut g = DbGraph::new();
    for (i, f) in floats.iter().enumerate() {
        g.add_node(format!("z{}", i), rec(1, [("x", Value::Float(*f))])).unwrap();
    }
    let check = |loaded: Loaded| {
        for (i, f) in floats.iter().enumerate() {
            match loaded.graph.node(loaded.graph.node_ix(&format!("z{}", i)).unwrap()).unwrap().data.attr["x"] {
                Value::Float(x) => assert_eq!(x.to_bits(), f.to_bits(), "{} loaded as {}", f, x),
                ref other => panic!("{:?}", other),
            }
        }
    };
    check(codec::from_binary(&codec::to_binary(&g, &meta).unwrap()).unwrap());
    let json = codec::to_json(&g, &meta, false).unwrap();
    assert!(String::from_utf8_lossy(&json).contains(r#"{"Float":-0.0}"#));
    check(codec::from_json(&json).unwrap());
}

#[test]
fn damaged_files_are_errors() {
    let (g, meta) = social();
    let bytes = codec::to_binary(&g, &meta).unwrap();
    let mut corrupt = bytes.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0x40;
    for result in [codec::from_binary(&corrupt), codec::from_binary_reader(&corrupt[..])] {
        assert!(matches!(result, Err(Error::Graph(GraphError::Format(_)))), "{:?}", result.map(|l| l.meta));
    }
    let short = &bytes[..bytes.len() - 5];
    assert!(matches!(codec::from_binary_reader(short), Err(Error::Graph(GraphError::Format(_)))));
    assert!(matches!(codec::from_json(b"{\"nodes\": 1}"), Err(Error::Graph(GraphError::Format(_)))));
}

#[test]
fn unsaveable_records_are_errors() {
    let (_, meta) = social();
    let mut g = DbGraph::new();
    let mut bad = rec(1, []);
    bad.meta.insert("iwdb.version".into(), Value::Int(2));
    g.add_node("x", bad).unwrap();
    for err in [codec::to_binary(&g, &meta).unwrap_err(), codec::to_json(&g, &meta, false).unwrap_err()] {
        assert!(err.to_string().contains("'iwdb.version' is reserved"), "{}", err);
    }

    let mut g = DbGraph::new();
    g.add_node("x", rec(i64::MAX as u64 + 1, [])).unwrap();
    for err in [codec::to_binary(&g, &meta).unwrap_err(), codec::to_json(&g, &meta, false).unwrap_err()] {
        assert!(err.to_string().contains("version 9223372036854775808 is too large to save"), "{}", err);
    }
}

#[test]
fn the_catalog_decides_which_indexes_exist_after_loading() {
    let (mut g, meta) = social();
    // The graph has an index the catalog doesn't list, and lacks the
    // catalog's unique-constraint index on `email`
    g.create_index::<GraphError>(&keys(&["name"])).unwrap();
    g.create_index::<GraphError>(&keys(&["age"])).unwrap();
    let bytes = codec::to_binary(&g, &meta).unwrap();
    let doc = format::LoadGraph::from_binary_slice(&bytes).unwrap();
    assert_eq!(doc.index_paths().unwrap(), vec![keys(&["name"]), keys(&["age"])]);

    for loaded in [codec::from_binary(&bytes).unwrap(), codec::from_binary_reader(&bytes[..]).unwrap()] {
        assert_eq!(loaded.index_changes.dropped, vec![keys(&["name"])]);
        assert_eq!(loaded.index_changes.created, vec![path(&["email"])]);
        let g = &loaded.graph;
        assert!(!g.has_index(&keys(&["name"])));
        assert!(!g.indexes_dirty());
        let found = g.find_nodes(&keys(&["age"]), &Value::Int(41)).unwrap().expect("indexed");
        assert_eq!(ids(g, found), ["carol"]);
        let found = g.find_nodes(&keys(&["email"]), &Value::from("b@x")).unwrap().expect("indexed");
        assert_eq!(ids(g, found), ["bob"]);
    }

    // Saving what the catalog says gives a file that needs no changes
    let loaded = codec::from_binary(&bytes).unwrap();
    let clean = codec::to_binary(&loaded.graph, &loaded.meta).unwrap();
    assert_eq!(codec::from_binary(&clean).unwrap().index_changes, Default::default());
}

#[test]
fn filters_and_indexes_work_on_db_records() {
    let (mut g, _) = social();
    let age = keys(&["age"]);
    let adults = Expr::And(vec![
        Expr::Label("Person".into()),
        Expr::Compare { path: age.clone(), op: CmpOp::Ge, value: Value::Int(30) },
    ]);
    let scan: Vec<NodeIx> = g.node_indices().filter(|&ix| adults.matches_node(&g, ix).unwrap()).collect();
    assert_eq!(ids(&g, scan), ["alice", "carol"]);
    assert!(g
        .index_candidates(&Expr::Compare { path: age.clone(), op: CmpOp::Ge, value: Value::Int(30) })
        .unwrap()
        .is_none());

    assert!(g.create_index::<GraphError>(&age).unwrap());
    assert!(!g.create_index::<GraphError>(&age).unwrap(), "already indexed");
    // Numbers match across int and float
    assert_eq!(ids(&g, g.find_nodes(&age, &Value::Int(41)).unwrap().unwrap()), ["carol"]);
    let candidates = g.index_candidates(&adults).unwrap().expect("narrowed by the index");
    let matching: Vec<NodeIx> = candidates.into_iter().filter(|&ix| adults.matches_node(&g, ix).unwrap()).collect();
    assert_eq!(ids(&g, matching), ["alice", "carol"]);
    let range =
        g.find_nodes_in_range::<GraphError>(&age, Included(&Value::Int(20)), Excluded(&Value::Int(35))).unwrap();
    assert_eq!(ids(&g, range.unwrap()), ["alice", "bob"]);
    // Not-equal can't use the index
    let ne = Expr::Compare { path: age.clone(), op: CmpOp::Ne, value: Value::Int(1) };
    assert!(g.index_candidates(&ne).unwrap().is_none());

    // Edge filters read the edge's DbRecord
    let heavy = Expr::Compare { path: keys(&["weight"]), op: CmpOp::Gt, value: Value::Float(1.0) };
    let edges: Vec<u64> =
        g.edges().filter(|&(e, _)| heavy.matches_edge(&g, e).unwrap()).map(|(_, edge)| edge.id().0).collect();
    assert_eq!(edges, [5]);

    // Ops keep indexes current, before and after a flush; versions don't
    // affect lookups
    g.apply(Op::SetNodeAttr { id: "bob".into(), key: "age".into(), value: Some(Value::Int(41)) }).unwrap();
    assert_eq!(ids(&g, g.find_nodes(&age, &Value::Int(41)).unwrap().unwrap()), ["bob", "carol"]);
    g.flush_indexes().unwrap();
    assert!(!g.indexes_dirty());
    assert_eq!(ids(&g, g.find_nodes(&age, &Value::Int(41)).unwrap().unwrap()), ["bob", "carol"]);
}

#[test]
fn projections_and_pagerank_work_on_db_records() {
    let (g, _) = social();
    let unit = Projection::build::<_, _, GraphError>(&g, Direction::Out, &EdgeCost::Unit).unwrap();
    assert_eq!(unit.node_count(), 4);
    let ranks = pagerank(&unit, &PageRank::default()).unwrap();
    assert_eq!(ranks.len(), 4);
    assert!((ranks.iter().sum::<f64>() - 1.0).abs() < 1e-6, "{:?}", ranks);
    assert!(ranks.iter().all(|&r| r > 0.0));

    // Weights read through DbRecord's Attributes
    let weighted =
        Projection::build::<_, _, GraphError>(&g, Direction::Out, &EdgeCost::weighted(None, Some(1.0))).unwrap();
    assert_eq!(weighted.node_count(), 4);
    let weighted_ranks = pagerank(&weighted, &PageRank::default()).unwrap();
    assert_eq!(weighted_ranks.len(), 4);
    // An ill-typed weight is an error, not a panic
    let mut bad = g.clone();
    bad.apply(Op::SetEdgeAttr { id: EdgeId(0), key: "weight".into(), value: Some(Value::from("heavy")) }).unwrap();
    assert!(Projection::build::<_, _, GraphError>(&bad, Direction::Out, &EdgeCost::weighted(None, None)).is_err());
}
