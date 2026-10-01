//! Smoke tests for the `ironweaver-core` guarantees the database builds on.
//!
//! Each test checks one assumption from
//! `documentation/ironweaver-core-review.md` against the pinned revision.
//! If one fails after a core bump, the review (and the code relying on it)
//! must be revisited before the bump lands.

// Test helpers outside `#[test]` functions may panic too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ironweaver_core::algo::{pagerank, PageRank};
use ironweaver_core::cancel::{self, Token};
use ironweaver_core::format::{self, GraphWriter, LoadGraph, RecordCodec};
use ironweaver_core::pathfinding::EdgeCost;
use ironweaver_core::query::Pattern;
use ironweaver_core::traversal::bfs_limited;
use ironweaver_core::{
    Attrs, Budget, CmpOp, Date, Direction, EdgeId, Expr, Graph, GraphError, Op, Projection, Record, Value,
};
use iwdb_engine::testutil::canonical;

type G = Graph<Record, Record>;
type O = Op<Record, Record>;

// `Graph<Record, Record>` and `Projection` can be shared between threads
// (the database keeps graphs behind an `RwLock` and runs analytics on
// projections outside the lock).
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<G>();
    send_sync::<Projection>();
    send_sync::<Token>();
};

fn rec(attr: impl IntoIterator<Item = (&'static str, Value)>) -> Record {
    Record::with_attr(attr)
}

fn path(name: &str) -> Vec<String> {
    vec![name.to_owned()]
}

fn ids(g: &G, ixs: Vec<ironweaver_core::NodeIx>) -> Vec<String> {
    let mut out: Vec<String> = ixs.into_iter().filter_map(|ix| g.node(ix)).map(|n| n.id().to_owned()).collect();
    out.sort();
    out
}

/// A batch touching every op kind, with explicit edge ids taken from the
/// graph's counter, as the database's writer will do.
fn sample_batch(g: &G) -> Vec<O> {
    let first = g.next_edge_id().0;
    let nested: Attrs =
        [("city".to_owned(), Value::String("Berlin".into())), ("zip".to_owned(), Value::Int(10115))].into();
    vec![
        Op::AddNode {
            id: "alice".into(),
            labels: vec!["Person".into()],
            data: rec([
                ("age", Value::Int(30)),
                ("score", Value::Float(1.5)),
                ("born", Value::Date(Date::from_ymd(1996, 2, 29).expect("valid date"))),
                ("address", Value::Dict(nested)),
                ("tags", Value::List(vec![Value::String("a".into()), Value::Bool(true), Value::None])),
                ("blob", Value::Bytes(vec![0, 1, 2, 255])),
            ]),
        },
        Op::AddNode { id: "bob".into(), labels: vec!["Person".into(), "Admin".into()], data: rec([]) },
        Op::AddNode { id: "carol".into(), labels: vec![], data: rec([("age", Value::Int(41))]) },
        Op::AddEdge {
            id: EdgeId(first),
            from: "alice".into(),
            to: "bob".into(),
            ty: Some("KNOWS".into()),
            data: rec([("since", Value::Int(2020))]),
        },
        Op::AddEdge { id: EdgeId(first + 1), from: "bob".into(), to: "carol".into(), ty: None, data: rec([]) },
        Op::AddEdge { id: EdgeId(first + 2), from: "carol".into(), to: "carol".into(), ty: None, data: rec([]) },
        Op::SetNodeAttr { id: "bob".into(), key: "age".into(), value: Some(Value::Int(25)) },
        Op::SetNode { id: "carol".into(), data: rec([("age", Value::Int(42))]) },
        Op::AddLabel { id: "carol".into(), label: "Person".into() },
        Op::RemoveLabel { id: "bob".into(), label: "Admin".into() },
        Op::SetEdgeType { id: EdgeId(first + 1), ty: Some("MANAGES".into()) },
        Op::SetEdgeAttr { id: EdgeId(first), key: "weight".into(), value: Some(Value::Float(0.25)) },
        Op::SetEdge { id: EdgeId(first + 1), data: rec([("weight", Value::Float(2.0))]) },
        Op::RemoveEdge { id: EdgeId(first + 2) },
        Op::AddNode { id: "tmp".into(), labels: vec![], data: rec([]) },
        Op::RenameNode { id: "tmp".into(), new_id: "dave".into() },
        Op::AddEdge { id: EdgeId(first + 3), from: "dave".into(), to: "alice".into(), ty: None, data: rec([]) },
        Op::RemoveNode { id: "dave".into() },
    ]
}

#[test]
fn builds_without_python() {
    // Design rule 1: only the bindings crate (step 7) depends on pyo3; no
    // other package in the lockfile does (pyo3's own crates aside).
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock = std::fs::read_to_string(lock).expect("workspace Cargo.lock");
    assert!(lock.contains("name = \"ironweaver-core\""));
    for package in lock.split("[[package]]") {
        let Some(name) = package.lines().find_map(|l| l.strip_prefix("name = \"")) else { continue };
        let name = name.trim_end_matches('"');
        let uses_pyo3 = package.lines().any(|l| l.trim() == "\"pyo3\"," || l.trim().starts_with("\"pyo3 "));
        if uses_pyo3 && !name.starts_with("pyo3") {
            assert_eq!(name, "iwdb-python", "pyo3 must not be a dependency of {}", name);
        }
    }
}

#[test]
fn ops_round_trip_through_serde_and_replay_with_identical_edge_ids() {
    let mut g = G::new();
    let batch = sample_batch(&g);
    g.apply_all(batch.clone()).expect("batch applies");

    let bytes = postcard::to_stdvec(&batch).expect("postcard encode");
    let from_postcard: Vec<O> = postcard::from_bytes(&bytes).expect("postcard decode");
    assert_eq!(from_postcard, batch);

    let json = serde_json::to_string(&batch).expect("json encode");
    let from_json: Vec<O> = serde_json::from_str(&json).expect("json decode");
    assert_eq!(from_json, batch);

    for replayed in [from_postcard, from_json] {
        let mut g2 = G::new();
        g2.apply_all(replayed).expect("replay applies");
        assert_eq!(canonical(&g2), canonical(&g));
        let mut edge_ids: Vec<u64> = g2.edges().map(|(_, e)| e.id().0).collect();
        edge_ids.sort();
        assert_eq!(edge_ids, vec![0, 1]);
        assert_eq!(g2.next_edge_id(), g.next_edge_id());
    }
}

#[test]
fn failing_apply_all_leaves_the_graph_unchanged() {
    let mut g = G::new();
    g.apply_all(sample_batch(&g)).expect("setup");
    let before = canonical(&g);
    let counter_before = g.next_edge_id();

    let next = g.next_edge_id().0;
    let batch = vec![
        Op::SetNodeAttr { id: "alice".into(), key: "age".into(), value: Some(Value::Int(99)) },
        Op::AddEdge { id: EdgeId(next + 10), from: "carol".into(), to: "alice".into(), ty: None, data: rec([]) },
        Op::RemoveNode { id: "bob".into() },
        Op::AddLabel { id: "alice".into(), label: "Changed".into() },
        Op::RenameNode { id: "carol".into(), new_id: "carla".into() },
        // Fails: the id is taken
        Op::AddNode { id: "alice".into(), labels: vec![], data: rec([]) },
    ];
    let (at, err) = g.apply_all(batch).expect_err("batch fails");
    assert_eq!(at, 5);
    assert_eq!(err, GraphError::DuplicateNode("alice".into()));
    assert_eq!(canonical(&g), before);

    // Known deviation (documented in the review): rollback does not lower the
    // edge id counter raised by the undone AddEdge. Edge ids are still never
    // reused, and the WAL carries explicit ids, so replay stays exact.
    assert_eq!(g.next_edge_id(), EdgeId(next + 11));
    assert!(g.next_edge_id() > counter_before);
}

/// A directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("iwdb-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn graph_meta_survives_write_atomic_and_binary_format() {
    let mut g = G::new();
    g.apply_all(sample_batch(&g)).expect("setup");
    // Graph-level meta is not stored in `Graph`; it travels next to it
    // through the codec and comes back from the loader.
    let mut meta = Attrs::new();
    meta.insert("iwdb.seq".into(), Value::Int(42));

    let dir = TempDir::new("smoke-format");
    let file = dir.0.join("checkpoint.iwg");
    for round in 0..2 {
        format::write_atomic(&file, |out| {
            GraphWriter::new(&g, &RecordCodec { meta: &meta, half: false })
                .write_binary(&mut *out)
                .map_err(io::Error::other)?;
            out.flush()
        })
        .expect("atomic save");
        // Only the target file is left, also when overwriting
        let entries: Vec<_> = std::fs::read_dir(&dir.0).expect("read dir").collect();
        assert_eq!(entries.len(), 1, "round {}: temp files left behind", round);
    }

    let bytes = std::fs::read(&file).expect("read checkpoint");
    assert!(bytes.starts_with(b"IRONWEAV"));
    // The convenience encoder writes the same format
    assert_eq!(format::to_binary(&g, &meta, false).expect("encode").len(), bytes.len());

    let (loaded, loaded_meta) = format::from_binary(&bytes).expect("load");
    assert_eq!(loaded_meta.get("iwdb.seq"), Some(&Value::Int(42)));
    assert_eq!(canonical(&loaded), canonical(&g));
    assert_eq!(loaded.next_edge_id(), g.next_edge_id());

    // Corruption is an error, not a panic
    let mut corrupt = bytes.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xff;
    assert!(matches!(format::from_binary(&corrupt), Err(GraphError::Format(_))));
    assert!(matches!(format::from_binary(&bytes[..bytes.len() - 3]), Err(GraphError::Format(_))));
}

/// Known core behaviour (upstream #32): `write_atomic` syncs the directory
/// after the rename on a best-effort basis and ignores failures, so `Ok`
/// doesn't mean the rename is durable. Pinned with a directory that can be
/// written to but not opened for reading (mode 0o300): creating the
/// temporary file and renaming it work, opening the directory to fsync it
/// fails, and the save still returns `Ok`. When this fails, upstream
/// reports the error: drop the extra directory sync after checkpoint writes
/// in `iwdb_storage::checkpoint` (see `documentation/steps/upstream-check.md`).
#[cfg(unix)]
#[test]
fn write_atomic_ignores_a_failed_directory_sync() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("smoke-dirsync");
    let sub = dir.0.join("sub");
    std::fs::create_dir(&sub).expect("mkdir");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o300)).expect("chmod");
    if std::fs::File::open(&sub).is_ok() {
        // Running as root: permissions don't apply, nothing to show
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        return;
    }
    let result = format::write_atomic(sub.join("file"), |out| out.write_all(b"data"));
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert!(result.is_ok(), "{:?}", result);
    assert_eq!(std::fs::read(sub.join("file")).expect("read"), b"data");
}

/// Known core behaviour (upstream #33): the binary header's `flags` (u16,
/// bytes 10..12) and `reserved` (u32, bytes 12..16) are documented as 0,
/// but the loaders don't check them, and the CRC32 covers only the
/// payload. So damage there goes unnoticed (both loaders), and a flag a
/// newer writer sets would be ignored rather than refused. Pinned: a file
/// with both fields changed loads like the original. When this fails,
/// upstream checks them: drop the header check in `iwdb_storage::verify`
/// (see `documentation/steps/upstream-check.md`).
#[test]
fn binary_header_flags_and_reserved_bytes_are_not_checked() {
    let mut g = G::new();
    g.apply_all(sample_batch(&g)).expect("setup");
    let meta = Attrs::new();
    let bytes = format::to_binary(&g, &meta, false).expect("save");
    assert_eq!(&bytes[..8], b"IRONWEAV");
    assert_eq!(&bytes[10..16], &[0; 6], "flags and reserved are written as 0");
    let mut changed = bytes.clone();
    changed[10] = 0x01; // a flag
    changed[12..16].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]); // reserved
    let (loaded, _) = format::from_binary(&changed).expect("the slice loader accepts it");
    assert_eq!(canonical(&loaded), canonical(&g));
    let (streamed, _) = format::from_binary_reader(&changed[..]).expect("the streaming loader accepts it");
    assert_eq!(canonical(&streamed), canonical(&g));
}

#[test]
fn property_index_lookups() {
    let mut g = G::new();
    g.apply_all(sample_batch(&g)).expect("setup");
    let age = path("age");
    assert!(g.find_nodes(&age, &Value::Int(30)).expect("lookup").is_none(), "no index yet");

    assert!(g.create_index::<GraphError>(&age).expect("create"));
    assert!(!g.create_index::<GraphError>(&age).expect("create again"));
    let found = g.find_nodes(&age, &Value::Int(30)).expect("lookup").expect("indexed");
    assert_eq!(ids(&g, found), ["alice"]);
    // Numbers match across int and float
    let found = g.find_nodes(&age, &Value::Float(42.0)).expect("lookup").expect("indexed");
    assert_eq!(ids(&g, found), ["carol"]);

    // Changes made through ops are visible before and after flushing
    g.apply(Op::SetNodeAttr { id: "bob".into(), key: "age".into(), value: Some(Value::Int(30)) }).expect("set");
    let found = g.find_nodes(&age, &Value::Int(30)).expect("lookup").expect("indexed");
    assert_eq!(ids(&g, found), ["alice", "bob"]);
    g.flush_indexes().expect("flush");
    assert!(!g.indexes_dirty());
    let found = g.find_nodes(&age, &Value::Int(30)).expect("lookup").expect("indexed");
    assert_eq!(ids(&g, found), ["alice", "bob"]);

    // Candidates for a filter: a superset, to be checked with `matches_node`
    let expr = Expr::And(vec![
        Expr::Label("Person".into()),
        Expr::Compare { path: age.clone(), op: CmpOp::Gt, value: Value::Int(29) },
        Expr::Compare { path: age.clone(), op: CmpOp::Lt, value: Value::Int(40) },
    ]);
    let candidates = g.index_candidates(&expr).expect("candidates").expect("narrowed");
    let matching: Vec<_> = candidates.into_iter().filter(|&ix| expr.matches_node(&g, ix).expect("evaluate")).collect();
    assert_eq!(ids(&g, matching), ["alice", "bob"]);
    // Not-equal can't use the index
    let ne = Expr::Compare { path: age, op: CmpOp::Ne, value: Value::Int(1) };
    assert!(g.index_candidates(&ne).expect("candidates").is_none());
}

/// A deterministic pseudo-random graph (xorshift), `n` nodes and `m` edges.
fn random_graph(n: usize, m: usize) -> G {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut g = G::with_capacity(n, m);
    let nodes: Vec<_> = (0..n).map(|i| g.add_node(format!("n{}", i), Record::default()).expect("add node")).collect();
    for _ in 0..m {
        let (a, b) = (nodes[next() as usize % n], nodes[next() as usize % n]);
        g.add_edge(a, b, Record::default()).expect("add edge");
    }
    g
}

#[test]
fn cancel_token_stops_pagerank_from_another_thread() {
    let g = random_graph(50_000, 250_000);
    let projection = Projection::build::<_, _, GraphError>(&g, Direction::Out, &EdgeCost::Unit).expect("projection");
    assert_eq!(projection.node_count(), 50_000);
    // tol = 0 runs exactly max_iter iterations: effectively forever
    let opts = PageRank { max_iter: usize::MAX, tol: 0.0, ..PageRank::default() };

    let token = Token::new();
    let canceller = {
        let token = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            token.cancel();
        })
    };
    let started = Instant::now();
    let result = cancel::run(&token, || pagerank(&projection, &opts));
    canceller.join().expect("canceller thread");
    assert_eq!(result, Err(GraphError::Interrupted));
    assert!(started.elapsed() < Duration::from_secs(30), "stopped too late: {:?}", started.elapsed());

    // The token is per computation: a fresh one runs to completion
    let opts = PageRank { max_iter: 3, tol: 0.0, ..PageRank::default() };
    let ranks = cancel::run(&Token::new(), || pagerank(&projection, &opts)).expect("not cancelled").expect("ranks");
    assert_eq!(ranks.len(), 50_000);
}

/// Binary bytes of `g` without a timestamp, so equal graphs give equal bytes.
fn save(g: &G, meta: &Attrs) -> Vec<u8> {
    let mut out = Vec::new();
    GraphWriter::new(g, &RecordCodec { meta, half: false }).with_timestamp(None).write_binary(&mut out).expect("save");
    out
}

#[test]
fn saves_are_deterministic_and_carry_index_definitions() {
    // Two graphs built the same way have attribute maps with different hash
    // seeds, so their iteration orders differ; saves are sorted anyway.
    let build = || {
        let mut g = G::new();
        g.apply_all(sample_batch(&g)).expect("setup");
        g.create_index::<GraphError>(&path("age")).expect("index");
        g
    };
    let mut meta = Attrs::new();
    for i in 0..32 {
        meta.insert(format!("k{}", i), Value::Int(i));
    }
    let (a, b) = (build(), build());
    let bytes = save(&a, &meta);
    assert_eq!(bytes, save(&b, &meta));
    assert_eq!(bytes, save(&a, &meta.clone()));

    // Index definitions travel in `metadata.indexes`; loaders recreate them
    // unflushed, and the Record convenience loaders flush them.
    let doc = LoadGraph::from_binary_slice(&bytes).expect("parse");
    assert_eq!(doc.index_paths().expect("paths"), vec![path("age")]);
    let built: G = doc
        .build(
            |n| Ok::<_, GraphError>(Record { attr: n.attr().to_attrs(), meta: n.meta().to_attrs() }),
            |e| Ok(Record { attr: e.attr().to_attrs(), meta: e.meta().to_attrs() }),
        )
        .expect("build");
    assert!(built.has_index(&path("age")));
    assert!(built.indexes_dirty());
    let (loaded, _) = format::from_binary(&bytes).expect("load");
    assert!(!loaded.indexes_dirty());
    let found = loaded.find_nodes(&path("age"), &Value::Int(30)).expect("lookup").expect("indexed");
    assert_eq!(ids(&loaded, found), ["alice"]);
}

#[test]
fn streaming_loader_matches_the_slice_loader() {
    let mut g = G::new();
    g.apply_all(sample_batch(&g)).expect("setup");
    let mut meta = Attrs::new();
    meta.insert("iwdb.seq".into(), Value::Int(7));
    let bytes = save(&g, &meta);

    let (streamed, streamed_meta) = format::from_binary_reader(&bytes[..]).expect("stream");
    let (sliced, sliced_meta) = format::from_binary(&bytes).expect("slice");
    assert_eq!(canonical(&streamed), canonical(&sliced));
    assert_eq!(streamed_meta, sliced_meta);
    assert_eq!(streamed.next_edge_id(), g.next_edge_id());

    // The checksum is checked at the end: a damaged file is an error, and
    // the partly built graph is never returned
    let mut corrupt = bytes.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xff;
    assert!(matches!(format::from_binary_reader(&corrupt[..]), Err(GraphError::Format(_))));
    assert!(matches!(format::from_binary_reader(&bytes[..bytes.len() - 3]), Err(GraphError::Format(_))));
}

#[test]
fn memory_usage_leaves_out_payloads() {
    let build = |text: &str| {
        let mut g = G::new();
        for i in 0..100 {
            g.add_node(format!("n{}", i), rec([("text", Value::String(text.repeat(1000)))])).expect("add");
        }
        g
    };
    // Attribute maps are owned by the payload: the database must add them
    assert_eq!(build("").memory_usage(), build("x").memory_usage());
}

/// Known gap (documented in the review, upstream #27): `max_visited` counts nodes
/// expanded, not edges examined, and the cancel token is checked per node.
/// A hub's whole edge list is scanned under `max_visited(1)`, even when
/// cancelled on the first edge. When this test fails, upstream has closed
/// the gap: drop the edge-counting workaround and update the review.
#[test]
fn visit_budget_counts_nodes_not_edges() {
    const FAN: usize = 10_000;
    let mut g = G::new();
    let hub = g.add_node("hub", Record::default()).expect("add");
    let leaf = g.add_node("leaf", Record::default()).expect("add");
    for _ in 0..FAN {
        g.add_edge(hub, leaf, Record::default()).expect("edge");
    }

    let calls = Cell::new(0usize);
    let counting = |_, _: &_| {
        calls.set(calls.get() + 1);
        Ok::<_, GraphError>(true)
    };
    let _ = bfs_limited(&g, hub, None, Budget::default().max_visited(1).truncate(), counting).expect("bfs");
    assert_eq!(calls.get(), FAN, "the budget does not bound edges examined");

    calls.set(0);
    let token = Token::new();
    let cancelling = |_, _: &_| {
        calls.set(calls.get() + 1);
        token.cancel();
        Ok::<_, GraphError>(true)
    };
    let _ = cancel::run(&token, || bfs_limited(&g, hub, None, Budget::UNLIMITED, cancelling));
    assert_eq!(calls.get(), FAN, "cancellation is checked per node, not per edge");

    // The workaround: count edges in the closure and fail from it
    calls.set(0);
    let bounded = |_, _: &_| {
        calls.set(calls.get() + 1);
        if calls.get() > 100 {
            return Err(GraphError::BudgetExceeded { visited: 1, results: 0 });
        }
        Ok(true)
    };
    let result = bfs_limited(&g, hub, None, Budget::UNLIMITED, bounded);
    assert!(matches!(result, Err(GraphError::BudgetExceeded { .. })));
    assert_eq!(calls.get(), 101);
}

#[test]
fn expr_and_pattern_round_trip() {
    let expr = Expr::And(vec![
        Expr::Label("Person".into()),
        Expr::Not(Box::new(Expr::Compare { path: path("age"), op: CmpOp::Lt, value: Value::Int(18) })),
        Expr::Exists { path: vec!["address".into(), "city".into()] },
    ]);
    let json = serde_json::to_string(&expr).expect("json");
    assert_eq!(serde_json::from_str::<Expr>(&json).expect("json decode"), expr);
    let bytes = postcard::to_stdvec(&expr).expect("postcard");
    assert_eq!(postcard::from_bytes::<Expr>(&bytes).expect("postcard decode"), expr);

    // Too deep is an error, not a stack overflow. Nit (in the review,
    // upstream #29): under postcard the error loses its message.
    let mut deep = Expr::Const(true);
    for _ in 0..200 {
        deep = Expr::Not(Box::new(deep));
    }
    let json_err = serde_json::to_string(&deep).expect_err("too deep").to_string();
    assert!(json_err.contains("nested more than"), "{}", json_err);
    let postcard_err = postcard::to_stdvec(&deep).expect_err("too deep").to_string();
    assert!(!postcard_err.contains("nested more than"), "{}", postcard_err);

    let pattern = Pattern::parse("(a:Person)-[:KNOWS*1..3]->(b)").expect("parse");
    assert_eq!(Pattern::parse(&pattern.to_string()).expect("reparse"), pattern);
}

fn nest(levels: usize, inner: Value) -> Value {
    (0..levels).fold(inner, |v, _| Value::List(vec![v]))
}

/// Known deviation (in the review, upstream #31): `Value`'s
/// serde counts containers entered, the file format counts value depth (a
/// scalar is depth 1, `MAX_DEPTH` = 100). They agree when the innermost
/// value is a scalar, but 100 nested lists with an empty innermost one
/// (depth 100) save and load in the file format while `Value`'s serde (and
/// so the log's postcard encoding) rejects them. The commit pipeline counts
/// an empty container as holding a scalar (`MAX_VALUE_DEPTH`), so it never
/// logs such a value. When this test fails, upstream has made the two agree.
#[test]
fn value_serde_rejects_empty_containers_at_the_depth_limit() {
    let file_ok = |v: &Value| {
        let mut g = G::new();
        g.add_node("a", rec([("k", v.clone())])).unwrap();
        format::to_binary(&g, &Attrs::new(), false).is_ok_and(|bytes| format::from_binary(&bytes).is_ok())
    };
    let serde_ok = |v: &Value| {
        let (postcard, json) = (postcard::to_stdvec(v).is_ok(), serde_json::to_string(v).is_ok());
        assert_eq!(postcard, json, "both encoders go through Value's serde");
        postcard
    };

    // A scalar at depth 100 and 101: both agree
    let at_limit = nest(99, Value::Int(1));
    assert!(file_ok(&at_limit) && serde_ok(&at_limit));
    let beyond = nest(100, Value::Int(1));
    assert!(!file_ok(&beyond) && !serde_ok(&beyond));

    // An empty list at depth 100: the file format takes it, serde doesn't
    let empty_at_limit = nest(99, Value::List(vec![]));
    assert!(file_ok(&empty_at_limit));
    assert!(!serde_ok(&empty_at_limit));
    let empty_dict_at_limit = nest(99, Value::Dict(Attrs::new()));
    assert!(file_ok(&empty_dict_at_limit));
    assert!(!serde_ok(&empty_dict_at_limit));
}
