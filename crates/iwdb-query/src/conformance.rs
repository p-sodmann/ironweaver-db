//! The conformance suite: what every implementation of [`Database`]
//! must do, written against the trait only.
//!
//! An implementation runs it with [`conformance_tests!`](crate::conformance_tests),
//! given an expression that makes a fresh, empty database (with only the
//! `default` namespace) for each test, as a value that dereferences to the
//! database (so it can own a temporary directory, a server, ...):
//!
//! ```ignore
//! iwdb_query::conformance_tests!(support::fresh_db());
//! ```
//!
//! The suite assumes the default [`LimitConfig`](crate::LimitConfig).
//! Every read operation has a test that it can't run unbounded
//! (`*_is_bounded`, and `every_read_times_out`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::assert_matches;
use std::time::Duration;

use ironweaver_core::algo::PageRank;
use ironweaver_core::pathfinding::{Coords, EdgeCost, Metric};
use ironweaver_core::{CmpOp, Direction, EdgeId, Expr, Op, Value};
use iwdb_engine::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label};
use iwdb_engine::reserved::VERSION_KEY;
use iwdb_engine::{CatalogChange, Change, DbRecord, IdempotencyKey, Mutation, Target};
use iwdb_storage::HistoryId;

use crate::admin::MAX_LIST;
use crate::auth::Operation;
use crate::metrics::{self as m, METRICS};
use crate::{Admin, Listed};
use crate::{
    AnalyticsRequest, Answer, ChangeEvent, ChangesRequest, Code, CommitOptions, Database, Error, ExplainRequest,
    FindRequest, Job, JobResult, MatchRequest, NeighbourhoodRequest, Order, PathMethod, PathRequest, Plan,
    ProjectionSpec, QueryOptions, SubgraphRequest, TraverseRequest, WalkRequest,
};

pub use crate::exec::block_on;

/// Expand to one `#[test]` per conformance case, each running the case
/// against a fresh database made by `$fixture` (an expression evaluated
/// per test, dereferencing to a [`Database`]).
#[macro_export]
macro_rules! conformance_tests {
    ($fixture:expr) => {
        $crate::conformance_tests!(@cases $fixture;
            commit_then_get_nodes_and_edges,
            get_is_bounded,
            errors_have_stable_codes,
            read_your_writes,
            every_read_times_out,
            find_paginates_by_id_with_or_without_an_index,
            find_is_bounded,
            defaults_apply_and_caps_lower_limits,
            cursors_expire_and_belong_to_their_request,
            explain_reports_the_index_and_the_scan_size,
            neighbourhood_follows_direction_types_and_filters,
            neighbourhood_is_bounded,
            traverse_in_breadth_and_depth_first_order,
            traverse_is_bounded,
            shortest_paths_by_bfs_dijkstra_and_astar,
            shortest_path_is_bounded,
            random_walks_start_at_the_start,
            random_walks_are_bounded,
            subgraph_holds_the_induced_edges,
            subgraph_is_bounded,
            match_rows_are_sorted_filtered_and_paginated,
            match_is_bounded,
            analytics_rank_their_results,
            analytics_are_bounded,
            catalog_changes_show_in_catalog_and_status,
            schema_counts_labels_and_samples_keys_and_types,
            schema_is_bounded,
            namespaces_are_created_and_dropped_once,
            changes_return_every_commit_as_logged,
            changes_resume_in_batches_without_gaps,
            changes_wait_for_a_commit_or_answer_empty,
            changes_report_their_errors,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                let fixture = $fixture;
                $crate::conformance::block_on($crate::conformance::$case(&*fixture));
            }
        )*
    };
}

/// Expand to one `#[test]` per case of the [`Admin`] conformance suite,
/// like [`conformance_tests!`] (`$fixture` dereferences to a
/// [`Database`] and [`Admin`] whose calls are registered: a server's
/// client, or the embedded store through
/// [`Authorized`](crate::Authorized)). The store must archive its WAL
/// and have a backup directory (step 16e), both empty.
#[macro_export]
macro_rules! admin_conformance_tests {
    ($fixture:expr) => {
        $crate::conformance_tests!(@cases $fixture;
            server_status_reports_the_database,
            metrics_hold_every_metric,
            a_running_request_is_listed_and_cancelled,
            admin_reads_are_bounded,
            consumers_report_their_lag,
            checkpoints_are_written_and_reported,
            backups_are_named_verified_and_never_overwrite,
            the_running_store_verifies,
            the_archive_is_pruned_before_a_backup,
        );
    };
}

const NS: &str = "default";

fn options() -> QueryOptions {
    QueryOptions::default()
}

fn limits(max_results: Option<usize>, max_visited: Option<usize>, max_edges: Option<usize>) -> QueryOptions {
    QueryOptions::default().with_limits(max_results, max_visited, max_edges)
}

fn partial(options: QueryOptions) -> QueryOptions {
    QueryOptions { partial: true, ..options }
}

fn node(id: &str, labels: &[&str], attr: &[(&str, Value)]) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        attr: attr.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(),
        meta: Default::default(),
        expected_version: None,
    }
}

fn edge(from: &str, to: &str, ty: &str, attr: &[(&str, Value)]) -> Mutation {
    Mutation::AddEdge {
        from: from.into(),
        to: to.into(),
        ty: Some(ty.into()),
        attr: attr.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(),
        meta: Default::default(),
    }
}

fn int(path: &str, op: CmpOp, v: i64) -> Expr {
    Expr::Compare { path: vec![path.into()], op, value: Value::Int(v) }
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> Code {
    match result {
        Ok(v) => panic!("expected an error, got {:?}", v),
        Err(e) => e.code(),
    }
}

async fn commit<D: Database>(db: &D, mutations: Vec<Mutation>) -> u64 {
    db.commit(NS, mutations, CommitOptions::default()).await.expect("commit").seq
}

/// ann -knows-> bob -knows-> cat -knows-> ann, bob -knows-> dan,
/// ann -works_at-> acme, bob -works_at-> acme; ages 30, 25, 40, 19.
async fn people<D: Database>(db: &D) -> u64 {
    let mut m = vec![
        node("ann", &["Person"], &[("age", Value::Int(30))]),
        node("bob", &["Person"], &[("age", Value::Int(25))]),
        node("cat", &["Person"], &[("age", Value::Int(40))]),
        node("dan", &["Person"], &[("age", Value::Int(19))]),
        node("acme", &["Company"], &[]),
    ];
    for (a, b, t) in
        [("ann", "bob", "knows"), ("bob", "cat", "knows"), ("cat", "ann", "knows"), ("bob", "dan", "knows")]
    {
        m.push(edge(a, b, t, &[]));
    }
    m.push(edge("ann", "acme", "works_at", &[]));
    m.push(edge("bob", "acme", "works_at", &[]));
    commit(db, m).await
}

/// A hub with edges to `n` leaves `l0000...`, and a chain from the last
/// leaf on: big enough that small limits stop every search.
async fn hub<D: Database>(db: &D, n: usize) {
    let mut m = vec![node("hub", &["Hub"], &[])];
    for i in 0..n {
        let leaf = format!("l{:04}", i);
        m.push(node(&leaf, &["Leaf"], &[("i", Value::Int(i as i64))]));
        m.push(edge("hub", &leaf, "to", &[]));
    }
    commit(db, m).await;
}

fn ids<T>(answer: &Answer<Vec<T>>, id: impl Fn(&T) -> &str) -> Vec<String> {
    answer.value.iter().map(|x| id(x).to_owned()).collect()
}

// ---- writes and lookups ----

pub async fn commit_then_get_nodes_and_edges<D: Database>(db: &D) {
    let result = db
        .commit(
            NS,
            vec![node("a", &["X"], &[("n", Value::Int(1))]), node("b", &[], &[]), edge("a", "b", "t", &[])],
            CommitOptions::default(),
        )
        .await
        .expect("commit");
    assert_eq!(result.edge_ids.len(), 1);
    let nodes = db.get_nodes(NS, vec!["b".into(), "nope".into(), "a".into()], options()).await.expect("get");
    assert_eq!(nodes.seq, result.seq);
    let got: Vec<Option<&str>> = nodes.value.iter().map(|n| n.as_ref().map(|n| n.id.as_str())).collect();
    assert_eq!(got, [Some("b"), None, Some("a")]);
    let a = nodes.value[2].as_ref().expect("a");
    assert_eq!((a.labels.clone(), a.attr.get("n"), a.version), (vec!["X".to_owned()], Some(&Value::Int(1)), 1));
    let edges = db.get_edges(NS, vec![result.edge_ids[0], EdgeId(u64::MAX)], options()).await.expect("get");
    let e = edges.value[0].as_ref().expect("edge");
    assert_eq!((e.from.as_str(), e.to.as_str(), e.ty.as_deref()), ("a", "b", Some("t")));
    assert!(edges.value[1].is_none());
}

pub async fn get_is_bounded<D: Database>(db: &D) {
    people(db).await;
    let ids: Vec<String> = ["ann", "bob", "cat"].iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(code(db.get_nodes(NS, ids.clone(), limits(Some(2), None, None)).await), Code::BudgetExceeded);
    assert_eq!(code(db.get_edges(NS, vec![EdgeId(0); 3], limits(Some(2), None, None)).await), Code::BudgetExceeded);
    assert_eq!(db.get_nodes(NS, ids, limits(Some(3), None, None)).await.expect("get").value.len(), 3);
}

pub async fn errors_have_stable_codes<D: Database>(db: &D) {
    people(db).await;
    assert_eq!(code(db.get_nodes("nope", vec![], options()).await), Code::NotFound);
    assert_eq!(code(db.commit("nope", vec![node("x", &[], &[])], CommitOptions::default()).await), Code::NotFound);
    let stale = Mutation::UpsertNode {
        id: "ann".into(),
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: Some(7),
    };
    assert_eq!(code(db.commit(NS, vec![stale], CommitOptions::default()).await), Code::Conflict);
    assert_eq!(code(db.commit(NS, vec![], CommitOptions::default()).await), Code::InvalidArgument);
    let missing = Mutation::DeleteNode { id: "nobody".into(), expected_version: None };
    assert_eq!(code(db.commit(NS, vec![missing], CommitOptions::default()).await), Code::NotFound);
    let unique = Constraint {
        kind: ConstraintKind::Unique,
        label: Label::new("Person").expect("label"),
        path: AttrPath::new(["name"]).expect("path"),
    };
    db.commit_catalog(NS, CatalogChange::AddConstraint(unique), CommitOptions::default()).await.expect("constraint");
    let twins = vec![
        node("x", &["Person"], &[("name", Value::from("same"))]),
        node("y", &["Person"], &[("name", Value::from("same"))]),
    ];
    assert_eq!(code(db.commit(NS, twins, CommitOptions::default()).await), Code::ConstraintViolation);
    assert_eq!(code(db.create_namespace(NS, None).await), Code::Conflict);
    assert_eq!(code(db.create_namespace("bad name!", None).await), Code::InvalidArgument);
    assert_eq!(code(db.get_nodes(NS, vec![], limits(Some(0), None, None)).await), Code::InvalidArgument);
    let bad = MatchRequest::parse("(a)-[").map(|_| ());
    assert_eq!(bad.map_err(|e| e.code()), Err(Code::InvalidArgument));
}

pub async fn read_your_writes<D: Database>(db: &D) {
    let seq = people(db).await;
    let answer = db.get_nodes(NS, vec!["ann".into()], QueryOptions::min_seq(seq)).await.expect("read");
    assert!(answer.seq >= seq && answer.value[0].is_some());
    assert_eq!(db.wait_for_seq(NS, seq, options()).await.expect("wait"), seq);
    let soon = QueryOptions { timeout: Some(Duration::from_millis(50)), ..QueryOptions::min_seq(seq + 1) };
    assert_eq!(code(db.wait_for_seq(NS, seq + 1, soon.clone()).await), Code::Timeout);
    assert_eq!(code(db.get_nodes(NS, vec!["ann".into()], soon).await), Code::Timeout);
}

/// The timeout bounds every read: one that is over before the read starts
/// fails every operation with `timeout`.
pub async fn every_read_times_out<D: Database>(db: &D) {
    people(db).await;
    let o = || QueryOptions { timeout: Some(Duration::ZERO), ..QueryOptions::default() };
    let f = FindRequest { filter: Expr::Label("Person".into()) };
    let results = [
        code(db.get_nodes(NS, vec!["ann".into()], o()).await),
        code(db.get_edges(NS, vec![EdgeId(0)], o()).await),
        code(db.find(NS, f.clone(), o()).await),
        code(db.explain(NS, ExplainRequest { filter: f.filter.clone(), analyze: true }, o()).await),
        code(db.neighbourhood(NS, NeighbourhoodRequest::new(["ann"], 2), o()).await),
        code(db.traverse(NS, TraverseRequest::new("ann", Order::Bfs), o()).await),
        code(db.shortest_path(NS, PathRequest::bfs("ann", "dan"), o()).await),
        code(db.random_walks(NS, WalkRequest::new("ann", 3, 2), o()).await),
        code(db.subgraph(NS, SubgraphRequest::new(["ann"], 1), o()).await),
        code(db.match_pattern(NS, MatchRequest::parse("(a)-->(b)").expect("pattern"), o()).await),
        code(
            db.analyze(
                NS,
                AnalyticsRequest { projection: ProjectionSpec::default(), job: Job::WeaklyConnectedComponents },
                o(),
            )
            .await,
        ),
        code(db.catalog(NS, o()).await),
        code(db.schema(NS, o()).await),
        code(db.wait_for_seq(NS, 1, o()).await),
        code(db.changes(NS, ChangesRequest { from_seq: 1, wait: false }, o()).await),
    ];
    assert!(results.iter().all(|c| *c == Code::Timeout), "{:?}", results);
}

// ---- find, cursors, explain ----

async fn all_pages<D: Database>(db: &D, request: &FindRequest, page: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let o = QueryOptions { cursor, ..limits(Some(page), None, None) };
        let answer = db.find(NS, request.clone(), o).await.expect("find");
        assert!(answer.value.len() <= page);
        out.extend(ids(&answer, |n| &n.id));
        pages += 1;
        match answer.next {
            Some(next) => cursor = Some(next),
            None => return (out, pages),
        }
    }
}

pub async fn find_paginates_by_id_with_or_without_an_index<D: Database>(db: &D) {
    people(db).await;
    let request = FindRequest { filter: int("age", CmpOp::Ge, 20) };
    let (scanned, pages) = all_pages(db, &request, 2).await;
    assert_eq!(scanned, ["ann", "bob", "cat"]);
    assert_eq!(pages, 2);
    let index = CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["age"]).expect("path") });
    db.commit_catalog(NS, index, CommitOptions::default()).await.expect("index");
    assert_eq!(all_pages(db, &request, 1).await.0, scanned);
    let labelled = FindRequest { filter: Expr::Label("Company".into()) };
    assert_eq!(all_pages(db, &labelled, 10).await, (vec!["acme".to_owned()], 1));
}

pub async fn find_is_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    let request = FindRequest { filter: int("i", CmpOp::Ge, 0) };
    assert_eq!(code(db.find(NS, request.clone(), limits(None, Some(50), None)).await), Code::BudgetExceeded);
    let answer = db.find(NS, request.clone(), partial(limits(Some(10), Some(50), None))).await.expect("partial");
    assert!(answer.truncated && answer.next.is_none() && answer.value.len() == 10);
    assert!(answer.work.visited <= 50);
    let all = db.find(NS, request, limits(None, Some(1000), None)).await.expect("find");
    assert_eq!((all.value.len(), all.truncated), (200, false));
}

pub async fn defaults_apply_and_caps_lower_limits<D: Database>(db: &D) {
    let m: Vec<Mutation> = (0..1500).map(|i| node(&format!("n{:04}", i), &["N"], &[])).collect();
    commit(db, m).await;
    let request = FindRequest { filter: Expr::Label("N".into()) };
    let first = db.find(NS, request.clone(), options()).await.expect("default page");
    assert_eq!(first.value.len(), crate::LimitConfig::DEFAULT_LIMITS.max_results);
    assert!(first.next.is_some());
    let huge = db.find(NS, request, limits(Some(usize::MAX), Some(usize::MAX), None)).await.expect("capped");
    assert_eq!((huge.value.len(), huge.next.is_none()), (1500, true));
}

pub async fn cursors_expire_and_belong_to_their_request<D: Database>(db: &D) {
    people(db).await;
    let request = FindRequest { filter: Expr::Label("Person".into()) };
    let first = db.find(NS, request.clone(), limits(Some(1), None, None)).await.expect("find");
    let cursor = first.next.clone().expect("more pages");
    let other = FindRequest { filter: Expr::Label("Company".into()) };
    let with = |c| QueryOptions { cursor: Some(c), ..limits(Some(1), None, None) };
    assert_eq!(code(db.find(NS, other, with(cursor.clone())).await), Code::InvalidArgument);
    assert_eq!(code(db.find(NS, request.clone(), with(crate::Cursor::new("garbage"))).await), Code::InvalidArgument);
    let second = db.find(NS, request.clone(), with(cursor.clone())).await.expect("next page");
    assert_eq!((ids(&second, |n| &n.id), second.seq), (vec!["bob".to_owned()], first.seq));
    commit(db, vec![node("eve", &["Person"], &[])]).await;
    assert_eq!(code(db.find(NS, request, with(cursor)).await), Code::CursorExpired);
}

pub async fn explain_reports_the_index_and_the_scan_size<D: Database>(db: &D) {
    people(db).await;
    let request = |filter, analyze| ExplainRequest { filter, analyze };
    let scan = db.explain(NS, request(int("age", CmpOp::Eq, 25), false), options()).await.expect("explain");
    assert_eq!((scan.value.plan.clone(), scan.value.estimated_candidates, scan.value.nodes), (Plan::Scan, 5, 5));
    let index = CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["age"]).expect("path") });
    db.commit_catalog(NS, index, CommitOptions::default()).await.expect("index");
    let point = db.explain(NS, request(int("age", CmpOp::Eq, 25), true), options()).await.expect("explain");
    assert_matches!(&point.value.plan, Plan::Index { path, .. } if path == &["age".to_owned()]);
    assert_eq!((point.value.estimated_candidates, point.value.candidates), (1, Some(1)));
    let label = db.explain(NS, request(Expr::Label("Person".into()), false), options()).await.expect("explain");
    assert_eq!(
        (label.value.plan.clone(), label.value.estimated_candidates),
        (Plan::Label { label: "Person".into() }, 4)
    );
}

// ---- graph searches ----

pub async fn neighbourhood_follows_direction_types_and_filters<D: Database>(db: &D) {
    people(db).await;
    let get = |request: NeighbourhoodRequest| async move {
        let answer = db.neighbourhood(NS, request, options()).await.expect("neighbourhood");
        ids(&answer, |n| &n.id)
    };
    assert_eq!(get(NeighbourhoodRequest::new(["ann"], 0)).await, ["ann"]);
    assert_eq!(get(NeighbourhoodRequest::new(["ann"], 1)).await, ["acme", "ann", "bob"]);
    assert_eq!(get(NeighbourhoodRequest::new(["ann"], 2)).await, ["acme", "ann", "bob", "cat", "dan"]);
    let incoming = NeighbourhoodRequest { direction: Direction::In, ..NeighbourhoodRequest::new(["acme"], 1) };
    assert_eq!(get(incoming).await, ["acme", "ann", "bob"]);
    let knows = NeighbourhoodRequest { edge_types: vec!["knows".into()], ..NeighbourhoodRequest::new(["ann"], 1) };
    assert_eq!(get(knows).await, ["ann", "bob"]);
    let filtered = NeighbourhoodRequest {
        edge_filter: Some(Expr::Type("works_at".into())),
        node_filter: Some(Expr::Label("Company".into())),
        ..NeighbourhoodRequest::new(["ann", "bob", "nobody"], 3)
    };
    assert_eq!(get(filtered).await, ["acme"]);
    let mut pages = Vec::new();
    let mut cursor = None;
    loop {
        let o = QueryOptions { cursor, ..limits(Some(2), None, None) };
        let answer = db.neighbourhood(NS, NeighbourhoodRequest::new(["ann"], 2), o).await.expect("page");
        pages.push(ids(&answer, |n| &n.id));
        cursor = answer.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(pages, [vec!["acme", "ann"], vec!["bob", "cat"], vec!["dan"]]);
}

pub async fn neighbourhood_is_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    for edge_types in [vec![], vec!["to".to_owned()]] {
        let request = NeighbourhoodRequest { edge_types, ..NeighbourhoodRequest::new(["hub"], 1) };
        assert_eq!(
            code(db.neighbourhood(NS, request.clone(), limits(None, None, Some(20))).await),
            Code::BudgetExceeded
        );
        let answer =
            db.neighbourhood(NS, request.clone(), partial(limits(None, None, Some(20)))).await.expect("partial");
        assert!(answer.truncated && answer.next.is_none() && answer.value.len() <= 21 && answer.work.edges <= 20);
        let seeds: Vec<String> = (0..20).map(|i| format!("l{:04}", i)).collect();
        let many = NeighbourhoodRequest { seeds, ..request };
        assert_eq!(code(db.neighbourhood(NS, many, limits(None, Some(10), None)).await), Code::BudgetExceeded);
    }
}

pub async fn traverse_in_breadth_and_depth_first_order<D: Database>(db: &D) {
    people(db).await;
    let bfs = db.traverse(NS, TraverseRequest::new("ann", Order::Bfs), options()).await.expect("bfs");
    assert_eq!(bfs.value[0], "ann");
    let mut level1 = bfs.value[1..3].to_vec();
    level1.sort();
    assert_eq!(level1, ["acme", "bob"]);
    let mut rest = bfs.value[3..].to_vec();
    rest.sort();
    assert_eq!(rest, ["cat", "dan"]);
    let knows =
        TraverseRequest { edge_types: vec!["knows".into()], depth: Some(1), ..TraverseRequest::new("ann", Order::Dfs) };
    assert_eq!(db.traverse(NS, knows, options()).await.expect("dfs").value, ["ann", "bob"]);
    let dfs = db.traverse(NS, TraverseRequest::new("cat", Order::Dfs), options()).await.expect("dfs");
    assert_eq!(dfs.value.len(), 5);
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };
    let incoming = TraverseRequest { direction: Direction::In, ..TraverseRequest::new("acme", Order::Bfs) };
    let reached = db.traverse(NS, incoming.clone(), options()).await.expect("bfs").value;
    assert_eq!(sorted(reached), ["acme", "ann", "bob", "cat"]);
    let employees = TraverseRequest { edge_types: vec!["works_at".into()], ..incoming };
    assert_eq!(sorted(db.traverse(NS, employees, options()).await.expect("bfs").value), ["acme", "ann", "bob"]);
    let both =
        TraverseRequest { direction: Direction::Both, depth: Some(1), ..TraverseRequest::new("dan", Order::Dfs) };
    assert_eq!(db.traverse(NS, both, options()).await.expect("dfs").value, ["dan", "bob"]);
    assert_eq!(code(db.traverse(NS, TraverseRequest::new("nobody", Order::Bfs), options()).await), Code::NotFound);
}

pub async fn traverse_is_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    for order in [Order::Bfs, Order::Dfs] {
        let request = TraverseRequest::new("hub", order);
        for o in [limits(Some(10), None, None), limits(None, None, Some(10))] {
            assert_eq!(code(db.traverse(NS, request.clone(), o.clone()).await), Code::BudgetExceeded);
            let answer = db.traverse(NS, request.clone(), partial(o)).await.expect("partial");
            assert!(answer.truncated && answer.value.len() <= 11, "{:?}", answer);
        }
    }
}

pub async fn shortest_paths_by_bfs_dijkstra_and_astar<D: Database>(db: &D) {
    let at = |x: f64| ("pos", Value::List(vec![Value::Float(x), Value::Float(0.0)]));
    let m = vec![
        node("a", &[], &[at(0.0)]),
        node("b", &[], &[at(1.0)]),
        node("c", &[], &[at(2.0)]),
        node("d", &[], &[at(3.0)]),
        edge("a", "d", "r", &[("w", Value::Float(10.0))]),
        edge("a", "b", "r", &[("w", Value::Float(1.0))]),
        edge("b", "c", "r", &[("w", Value::Float(1.0))]),
        edge("c", "d", "r", &[("w", Value::Float(1.0))]),
    ];
    commit(db, m).await;
    let bfs = db.shortest_path(NS, PathRequest::bfs("a", "d"), options()).await.expect("bfs").value.expect("path");
    assert_eq!((bfs.nodes.clone(), bfs.cost), (vec!["a".to_owned(), "d".to_owned()], 1.0));
    let weighted = EdgeCost::Weighted { key: "w".into(), default: 1.0 };
    let dijkstra = PathRequest { method: PathMethod::Dijkstra, cost: weighted.clone(), ..PathRequest::bfs("a", "d") };
    let path = db.shortest_path(NS, dijkstra.clone(), options()).await.expect("dijkstra").value.expect("path");
    assert_eq!((path.nodes, path.cost), (vec!["a".to_owned(), "b".into(), "c".into(), "d".into()], 3.0));
    let astar = PathRequest {
        method: PathMethod::AStar { coords: Coords::Sequence(vec!["pos".into()]), metric: Metric::Euclidean },
        ..dijkstra.clone()
    };
    assert_eq!(db.shortest_path(NS, astar, options()).await.expect("astar").value.map(|p| p.cost), Some(3.0));
    let back = db.shortest_path(NS, PathRequest::bfs("d", "a"), options()).await.expect("none");
    assert_eq!(back.value, None);
    assert_eq!(code(db.shortest_path(NS, PathRequest::bfs("a", "nobody"), options()).await), Code::NotFound);
    let bad = PathRequest { cost: weighted, ..PathRequest::bfs("a", "d") };
    assert_eq!(code(db.shortest_path(NS, bad, options()).await), Code::InvalidArgument);
}

pub async fn shortest_path_is_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    commit(db, vec![node("far", &[], &[]), edge("l0199", "far", "to", &[])]).await;
    let weighted = EdgeCost::Weighted { key: "w".into(), default: 1.0 };
    let dijkstra = PathRequest { method: PathMethod::Dijkstra, cost: weighted, ..PathRequest::bfs("hub", "far") };
    // The hub's 200 edges stop both methods (BFS expands only two nodes)
    for request in [PathRequest::bfs("hub", "far"), dijkstra] {
        assert_eq!(
            code(db.shortest_path(NS, request.clone(), limits(None, None, Some(20))).await),
            Code::BudgetExceeded
        );
        let answer =
            db.shortest_path(NS, request.clone(), partial(limits(None, None, Some(20)))).await.expect("partial");
        assert!(answer.truncated && answer.value.is_none());
        let found = db.shortest_path(NS, request, options()).await.expect("path");
        assert!(found.work.edges > 200, "{:?}", found.work);
        assert_eq!(found.value.map(|p| p.nodes.len()), Some(3));
    }
    // Dijkstra settles each leaf it reaches
    let dijkstra = PathRequest { method: PathMethod::Dijkstra, ..PathRequest::bfs("hub", "far") };
    assert_eq!(code(db.shortest_path(NS, dijkstra, limits(None, Some(20), None)).await), Code::BudgetExceeded);
}

pub async fn random_walks_start_at_the_start<D: Database>(db: &D) {
    people(db).await;
    let request = WalkRequest { seed: Some(7), ..WalkRequest::new("ann", 4, 5) };
    let walks = db.random_walks(NS, request, options()).await.expect("walks");
    assert!(!walks.value.is_empty() && walks.value.len() <= 5);
    for walk in &walks.value {
        assert!(walk[0] == "ann" && walk.len() <= 4);
    }
    assert_eq!(code(db.random_walks(NS, WalkRequest::new("nobody", 4, 5), options()).await), Code::NotFound);
}

pub async fn random_walks_are_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    commit(db, vec![node("x", &[], &[]), node("y", &[], &[]), edge("x", "y", "to", &[])]).await;
    // Planning reads what walks can reach (x and y), not the whole graph
    let small = WalkRequest { seed: Some(1), ..WalkRequest::new("x", 2, 3) };
    let answer = db.random_walks(NS, small, limits(None, Some(20), Some(20))).await.expect("walks");
    assert!(answer.value == [["x", "y"]] && answer.work.visited <= 20, "{:?}", answer);
    // Planning from the hub reads its 200 edges; a plan is all or nothing
    let request = WalkRequest { allow_revisit: true, seed: Some(1), ..WalkRequest::new("hub", 2, 100) };
    assert_eq!(code(db.random_walks(NS, request.clone(), limits(None, None, Some(100))).await), Code::BudgetExceeded);
    let answer = db.random_walks(NS, request.clone(), partial(limits(None, None, Some(100)))).await.expect("partial");
    assert!(answer.truncated && answer.value.is_empty(), "{:?}", answer);
    // Walking: 100 walks asked for, 5 results allowed
    assert_eq!(code(db.random_walks(NS, request.clone(), limits(Some(5), None, None)).await), Code::BudgetExceeded);
    let answer = db.random_walks(NS, request, partial(limits(Some(5), None, None))).await.expect("partial");
    assert!(answer.truncated && answer.value.len() == 5);
}

pub async fn subgraph_holds_the_induced_edges<D: Database>(db: &D) {
    people(db).await;
    let answer = db.subgraph(NS, SubgraphRequest::new(["ann"], 1), options()).await.expect("subgraph");
    let nodes: Vec<&str> = answer.value.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(nodes, ["acme", "ann", "bob"]);
    let mut edges: Vec<(&str, &str)> = answer.value.edges.iter().map(|e| (e.from.as_str(), e.to.as_str())).collect();
    edges.sort();
    assert_eq!(edges, [("ann", "acme"), ("ann", "bob"), ("bob", "acme")]);
    let ids: Vec<EdgeId> = answer.value.edges.iter().map(|e| e.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    let knows = SubgraphRequest { edge_types: vec!["knows".into()], ..SubgraphRequest::new(["ann"], 3) };
    let answer = db.subgraph(NS, knows, options()).await.expect("subgraph");
    assert_eq!((answer.value.nodes.len(), answer.value.edges.len()), (4, 4));
}

pub async fn subgraph_is_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    let request = SubgraphRequest::new(["hub"], 1);
    assert_eq!(code(db.subgraph(NS, request.clone(), limits(Some(50), None, None)).await), Code::BudgetExceeded);
    assert_eq!(code(db.subgraph(NS, request.clone(), limits(None, None, Some(20))).await), Code::BudgetExceeded);
    let answer = db.subgraph(NS, request, partial(limits(Some(50), None, None))).await.expect("partial");
    assert!(answer.truncated && answer.value.nodes.len() + answer.value.edges.len() <= 50);
}

// ---- patterns ----

pub async fn match_rows_are_sorted_filtered_and_paginated<D: Database>(db: &D) {
    people(db).await;
    let knows = MatchRequest::parse("(a:Person)-[:knows]->(b:Person)").expect("pattern");
    let all = db.match_pattern(NS, knows.clone(), options()).await.expect("match");
    let pairs: Vec<(String, String)> = all.value.iter().map(|r| (r.nodes[0].clone(), r.nodes[1].clone())).collect();
    let expected = [("ann", "bob"), ("bob", "cat"), ("bob", "dan"), ("cat", "ann")];
    assert_eq!(pairs, expected.map(|(a, b)| (a.to_owned(), b.to_owned())));
    assert!(all.value.iter().all(|r| r.edges.len() == 1 && r.edges[0].len() == 1));
    let older = MatchRequest { filters: vec![("b".into(), int("age", CmpOp::Ge, 30))], ..knows.clone() };
    let rows = db.match_pattern(NS, older, options()).await.expect("where");
    assert_eq!(rows.value.iter().map(|r| r.nodes[1].as_str()).collect::<Vec<_>>(), ["cat", "ann"]);
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let o = QueryOptions { cursor, ..limits(Some(3), None, None) };
        let answer = db.match_pattern(NS, knows.clone(), o).await.expect("page");
        paged.extend(answer.value);
        cursor = answer.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(paged, all.value);
    let paths = MatchRequest::parse("(a:Person {age: 30})-[:knows*1..3]->(b)").expect("pattern");
    let rows = db.match_pattern(NS, paths, options()).await.expect("paths");
    let ends: Vec<(&str, usize)> = rows.value.iter().map(|r| (r.nodes[1].as_str(), r.edges[0].len())).collect();
    assert_eq!(ends, [("ann", 3), ("bob", 1), ("cat", 2), ("dan", 2)]);
    let unknown = MatchRequest { filters: vec![("zz".into(), Expr::Const(true))], ..knows };
    assert_eq!(code(db.match_pattern(NS, unknown, options()).await), Code::InvalidArgument);
}

pub async fn match_is_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    let request = MatchRequest::parse("(h:Hub)-->(l:Leaf)").expect("pattern");
    assert_eq!(code(db.match_pattern(NS, request.clone(), limits(None, Some(50), None)).await), Code::BudgetExceeded);
    let answer =
        db.match_pattern(NS, request.clone(), partial(limits(Some(10), Some(50), None))).await.expect("partial");
    assert!(answer.truncated && answer.next.is_none() && answer.value.len() == 10 && answer.work.visited <= 50);
    let page = db.match_pattern(NS, request, limits(Some(10), None, None)).await.expect("page");
    assert!(!page.truncated && page.next.is_some() && page.value.len() == 10);
}

// ---- analytics ----

pub async fn analytics_rank_their_results<D: Database>(db: &D) {
    people(db).await;
    let run = |job, o| db.analyze(NS, AnalyticsRequest { projection: ProjectionSpec::default(), job }, o);
    let ranks = run(Job::PageRank(PageRank::default()), options()).await.expect("pagerank");
    let JobResult::Scores(scores) = &ranks.value else { panic!("scores") };
    assert_eq!(scores.len(), 5);
    assert!(scores.windows(2).all(|w| w[0].1 >= w[1].1));
    assert!((scores.iter().map(|s| s.1).sum::<f64>() - 1.0).abs() < 1e-6);
    let top = run(Job::PageRank(PageRank::default()), limits(Some(2), None, None)).await.expect("top 2");
    assert!(top.truncated);
    assert_matches!(&top.value, JobResult::Scores(s) if s.len() == 2 && s[0] == scores[0]);
    let wcc = run(Job::WeaklyConnectedComponents, options()).await.expect("wcc");
    assert_matches!(&wcc.value, JobResult::Groups(g) if g.len() == 1 && g[0].len() == 5);
    let scc = run(Job::StronglyConnectedComponents, options()).await.expect("scc");
    let JobResult::Groups(groups) = &scc.value else { panic!("groups") };
    assert_eq!(groups[0], ["ann", "bob", "cat"]);
}

pub async fn analytics_are_bounded<D: Database>(db: &D) {
    hub(db, 200).await;
    let request = || AnalyticsRequest { projection: ProjectionSpec::default(), job: Job::Triangles };
    assert_eq!(code(db.analyze(NS, request(), limits(None, Some(100), None)).await), Code::BudgetExceeded);
    assert_eq!(code(db.analyze(NS, request(), limits(None, None, Some(100))).await), Code::BudgetExceeded);
    let answer = db.analyze(NS, request(), limits(Some(3), None, None)).await.expect("top");
    assert!(answer.truncated && matches!(&answer.value, JobResult::Counts(c) if c.len() == 3));
}

// ---- catalog and namespaces ----

pub async fn catalog_changes_show_in_catalog_and_status<D: Database>(db: &D) {
    people(db).await;
    let path = AttrPath::new(["age"]).expect("path");
    let key = IdempotencyKey::new("idx-age").expect("key");
    let change = CatalogChange::CreateIndex(IndexDef { path: path.clone() });
    let options_with_key = CommitOptions { idempotency_key: Some(key) };
    let first = db.commit_catalog(NS, change.clone(), options_with_key.clone()).await.expect("index");
    let again = db.commit_catalog(NS, change.clone(), options_with_key).await.expect("retry");
    assert!(again.deduplicated && again.seq == first.seq);
    assert_eq!(code(db.commit_catalog(NS, change, CommitOptions::default()).await), Code::Conflict);
    let catalog = db.catalog(NS, QueryOptions::min_seq(first.seq)).await.expect("catalog");
    assert!(catalog.value.indexes().any(|i| i.path == path));
    let status = db.namespace_status(NS).await.expect("status");
    assert_eq!((status.name.as_str(), status.seq, status.nodes, status.edges), (NS, first.seq, 5, 6));
    let index = status.indexes.iter().find(|i| i.path == path).expect("index status");
    assert_eq!(index.size.map(|s| s.entries), Some(4));
}

pub async fn schema_counts_labels_and_samples_keys_and_types<D: Database>(db: &D) {
    people(db).await;
    let robot = Label::new("Robot").expect("label");
    let path = AttrPath::new(["serial"]).expect("path");
    let change = CatalogChange::AddConstraint(Constraint { kind: ConstraintKind::Required, label: robot, path });
    let seq = db.commit_catalog(NS, change, CommitOptions::default()).await.expect("constraint").seq;
    let answer = db.schema(NS, QueryOptions::min_seq(seq)).await.expect("schema");
    let s = &answer.value;
    assert_eq!((answer.seq, answer.truncated), (seq, false));
    assert_eq!((s.nodes, s.edges, s.sampled_nodes, s.sampled_edges), (5, 6, 5, 6));
    // Every label, the constraint's too, with its exact count
    let labels: Vec<(&str, usize)> = s.labels.iter().map(|l| (l.name.as_str(), l.count)).collect();
    assert_eq!(labels, [("Company", 1), ("Person", 4), ("Robot", 0)]);
    let person = &s.labels[1];
    assert_eq!((person.sampled, person.keys.len()), (4, 1));
    assert_eq!((person.keys[0].name.as_str(), person.keys[0].kinds.clone()), ("age", vec![("Int".to_owned(), 4)]));
    assert!(s.labels[0].keys.is_empty() && !person.more_keys);
    let types: Vec<(Option<&str>, usize)> = s.types.iter().map(|t| (t.name.as_deref(), t.count)).collect();
    assert_eq!(types, [(Some("knows"), 4), (Some("works_at"), 2)]);
}

pub async fn schema_is_bounded<D: Database>(db: &D) {
    hub(db, 50).await;
    // A limit ends the sample, not the read; label counts stay exact
    let answer = db.schema(NS, limits(None, Some(3), Some(2))).await.expect("sampled");
    let s = &answer.value;
    assert_eq!((s.nodes, s.edges, s.sampled_nodes, s.sampled_edges), (51, 50, 3, 2));
    assert_eq!((answer.work.visited, answer.work.edges, answer.truncated), (3, 2, false));
    assert!(!s.labels.is_empty());
    for label in &s.labels {
        assert_eq!(label.count, if label.name == "Hub" { 1 } else { 50 }, "{}", label.name);
    }
    assert_eq!(s.types.iter().map(|t| t.count).sum::<usize>(), 2);
    let all = db.schema(NS, options()).await.expect("all").value;
    assert_eq!((all.sampled_nodes, all.sampled_edges, all.labels.len()), (51, 50, 2));
    assert_eq!(code(db.schema(NS, limits(None, Some(0), None)).await), Code::InvalidArgument);
}

pub async fn namespaces_are_created_and_dropped_once<D: Database>(db: &D) {
    let key = || Some(IdempotencyKey::new("make-other").expect("key"));
    let made = db.create_namespace("other", key()).await.expect("create");
    let again = db.create_namespace("other", key()).await.expect("retry");
    assert!(!made.deduplicated && again.deduplicated && again.event.id == made.event.id);
    let names: Vec<String> = db.namespaces().await.expect("list").iter().map(|n| n.name.to_string()).collect();
    assert_eq!(names, ["default", "other"]);
    let seq = db.commit("other", vec![node("x", &[], &[])], CommitOptions::default()).await.expect("commit").seq;
    assert_eq!(seq, 1);
    assert!(db.get_nodes(NS, vec!["x".into()], options()).await.expect("get").value[0].is_none());
    db.drop_namespace("other", None).await.expect("drop");
    assert_eq!(code(db.get_nodes("other", vec!["x".into()], options()).await), Code::NotFound);
    assert_eq!(code(db.drop_namespace("other", None).await), Code::NotFound);
    assert_eq!(code(db.drop_namespace(NS, None).await), Code::InvalidArgument);
}

// ---- the change stream (ADR 0031) ----

fn changes_from(from_seq: u64) -> ChangesRequest {
    ChangesRequest { from_seq, wait: false }
}

/// Every event of the namespace from `from_seq` on, in batches of `batch`.
async fn all_changes<D: Database>(db: &D, from_seq: u64, batch: usize) -> Vec<ChangeEvent> {
    let mut events = Vec::new();
    let mut next = from_seq.max(1);
    loop {
        let answer = db.changes(NS, changes_from(next), limits(Some(batch), None, None)).await.expect("changes");
        let got = &answer.value;
        assert!(got.events.len() <= batch);
        if got.events.is_empty() {
            assert_eq!(got.next_seq, next);
            return events;
        }
        let seqs: Vec<u64> = got.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (next..next + seqs.len() as u64).collect::<Vec<_>>(), "no gaps");
        assert_eq!(got.next_seq, next + seqs.len() as u64);
        assert!(answer.seq >= got.next_seq - 1);
        next = got.next_seq;
        events.extend(answer.value.events);
    }
}

pub async fn changes_return_every_commit_as_logged<D: Database>(db: &D) {
    people(db).await;
    let key = IdempotencyKey::new("change-1").expect("key");
    let keyed = CommitOptions { idempotency_key: Some(key.clone()) };
    let set = Mutation::SetAttr {
        target: Target::Node("ann".into()),
        key: "age".into(),
        value: Value::Int(31),
        expected_version: None,
    };
    db.commit(NS, vec![set], keyed).await.expect("set");
    let index = CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["age"]).expect("path") });
    db.commit_catalog(NS, index.clone(), CommitOptions::default()).await.expect("index");
    let remove = Mutation::RemoveAttr { target: Target::Node("bob".into()), key: "age".into(), expected_version: None };
    commit(db, vec![remove, Mutation::DeleteNode { id: "dan".into(), expected_version: None }]).await;

    let events = all_changes(db, 0, 100).await;
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert!(events.iter().all(|e| e.time.is_some()));
    assert_eq!(events.iter().map(|e| e.key.clone()).collect::<Vec<_>>(), [None, Some(key), None, None]);

    let Change::Data(first) = &events[0].change else { panic!("a data change: {:?}", events[0].change) };
    let ann = DbRecord { version: 1, ..DbRecord::with_attr([("age", Value::Int(30))]) };
    assert!(first.contains(&Op::AddNode { id: "ann".into(), labels: vec!["Person".into()], data: ann }));
    assert_eq!(first.iter().filter(|op| matches!(op, Op::AddEdge { .. })).count(), 6);
    // The attribute op, then the version op (ADR 0004)
    let version = |v| Some(Value::Int(v));
    assert_eq!(
        events[1].change,
        Change::Data(vec![
            Op::SetNodeAttr { id: "ann".into(), key: "age".into(), value: Some(Value::Int(31)) },
            Op::SetNodeAttr { id: "ann".into(), key: VERSION_KEY.into(), value: version(2) },
        ])
    );
    assert_eq!(events[2].change, Change::Catalog(index));
    let Change::Data(last) = &events[3].change else { panic!("a data change") };
    assert!(last.contains(&Op::SetNodeAttr { id: "bob".into(), key: "age".into(), value: None }));
    assert!(last.contains(&Op::RemoveNode { id: "dan".into() }));
}

pub async fn changes_resume_in_batches_without_gaps<D: Database>(db: &D) {
    for i in 0..10 {
        commit(db, vec![node(&format!("n{}", i), &[], &[("i", Value::Int(i))])]).await;
    }
    let whole = all_changes(db, 0, 100).await;
    assert_eq!(whole.len(), 10);
    for batch in [1, 3, 7] {
        assert_eq!(all_changes(db, 0, batch).await, whole, "batches of {}", batch);
    }
    assert_eq!(all_changes(db, 6, 2).await, whole[5..]);
    let answer = db.changes(NS, changes_from(4), limits(Some(2), None, None)).await.expect("changes");
    assert_eq!((answer.value.next_seq, answer.value.first_seq), (6, 1));
    // Past the end: nothing, and where to go on from
    let answer = db.changes(NS, changes_from(50), options()).await.expect("changes");
    assert_eq!((answer.value.events.len(), answer.value.next_seq), (0, 50));
}

pub async fn changes_wait_for_a_commit_or_answer_empty<D: Database>(db: &D) {
    let seq = people(db).await;
    let wait = ChangesRequest { from_seq: seq + 1, wait: true };
    let soon = QueryOptions { timeout: Some(Duration::from_millis(200)), ..QueryOptions::default() };
    let start = std::time::Instant::now();
    let answer = db.changes(NS, wait, soon).await.expect("an empty batch, not a timeout");
    assert!(answer.value.events.is_empty() && answer.value.next_seq == seq + 1);
    assert!(start.elapsed() >= Duration::from_millis(100), "it waited: {:?}", start.elapsed());

    // A commit made while it waits ends the wait
    let long = QueryOptions { timeout: Some(Duration::from_secs(60)), ..QueryOptions::default() };
    let start = std::time::Instant::now();
    let answer = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            block_on(commit(db, vec![node("eve", &[], &[])]));
        });
        block_on(db.changes(NS, wait, long))
    })
    .expect("changes");
    assert!(start.elapsed() < Duration::from_secs(30), "woken by the commit: {:?}", start.elapsed());
    assert_eq!(answer.value.events.iter().map(|e| e.seq).collect::<Vec<_>>(), [seq + 1]);
}

pub async fn changes_report_their_errors<D: Database>(db: &D) {
    people(db).await;
    assert_eq!(code(db.changes("nope", changes_from(1), options()).await), Code::NotFound);
    let other = QueryOptions { history: Some(HistoryId::random()), ..QueryOptions::default() };
    assert_eq!(code(db.changes(NS, changes_from(1), other).await), Code::InvalidArgument);
    assert_eq!(code(db.changes(NS, changes_from(1), limits(Some(0), None, None)).await), Code::InvalidArgument);
}

// ---- the operator's reads (step 16c, `Admin`) ----

fn namespace<'a>(status: &'a crate::ServerStatus, name: &str) -> &'a crate::NamespaceStatus {
    status.namespaces.iter().find(|n| n.name == name).expect("the namespace's status")
}

pub async fn server_status_reports_the_database<D: Database + Admin>(db: &D) {
    let before = db.server_status().await.unwrap();
    assert!(!before.version.is_empty() && before.ready, "{:?}", before);
    assert!(["always", "group", "off"].contains(&before.fsync.as_str()), "{}", before.fsync);
    let seq = commit(db, vec![node("a", &["P"], &[])]).await;
    let after = db.server_status().await.unwrap();
    assert_eq!(after.started, before.started);
    let (was, now) = (namespace(&before, NS), namespace(&after, NS));
    assert_eq!((now.seq, now.nodes), (seq, was.nodes + 1));
    assert_eq!(now.since_checkpoint, now.seq - now.checkpoint.unwrap_or(0));
    assert!(after.disk.wal_bytes > 0, "{:?}", after.disk);
    // Memory is server-wide: the listed namespaces, the system namespace
    // and those the caller can't see (ADR 0054)
    let m = &after.memory;
    let graphs: u64 = after.namespaces.iter().map(|n| n.memory_bytes as u64).sum();
    assert!(m.graph_bytes >= graphs, "{:?}", m);
    assert_eq!(m.used_bytes, m.graph_bytes + m.payload_bytes + m.checkpoint_bytes + m.working_bytes);
    assert_eq!(m.limit_bytes.is_some(), m.refuse_writes_bytes.is_some());
    // The commit and the first status at least
    assert!(after.requests.total >= before.requests.total + 2, "{:?} then {:?}", before.requests, after.requests);
    assert!(after.requests.active >= 1, "this call runs");
}

pub async fn metrics_hold_every_metric<D: Database + Admin>(db: &D) {
    commit(db, vec![node("a", &["P"], &[])]).await;
    let metrics = db.metrics().await.unwrap();
    let names: Vec<&str> = metrics.families.iter().map(|f| f.name.as_str()).collect();
    assert!(names.iter().copied().eq(METRICS.iter().map(|d| d.name)), "{:?}", names);
    for (family, def) in metrics.families.iter().zip(METRICS) {
        assert_eq!(family.kind, def.kind, "{}", def.name);
        for sample in &family.samples {
            let labels: Vec<&str> = sample.labels.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(labels, def.labels, "{}", def.name);
        }
    }
    let gauge = |name: &str, ns: &str| {
        let family = metrics.family(name).unwrap();
        match family.samples.iter().find(|s| s.label("namespace") == Some(ns)).map(|s| &s.value) {
            Some(m::Value::Gauge(x)) => *x,
            other => panic!("{} of {}: {:?}", name, ns, other),
        }
    };
    assert_eq!(gauge(m::NAMESPACE_NODES, NS), 1.0);
    assert!(gauge(m::WAL_BYTES, NS) > 0.0);
    let commits = metrics.family(m::COMMIT_DURATION).unwrap();
    assert!(matches!(&commits.samples[0].value, m::Value::Histogram(h) if h.count() >= 1), "{:?}", commits);
    let ok_commits = metrics.family(m::REQUESTS).unwrap().samples.iter().any(|s| {
        s.label("operation") == Some(Operation::Commit.name())
            && s.label("code") == Some("ok")
            && matches!(s.value, m::Value::Counter(n) if n >= 1)
    });
    assert!(ok_commits, "{:?}", metrics.family(m::REQUESTS));
    let text = metrics.to_prometheus();
    for def in METRICS {
        assert!(text.contains(&format!("# TYPE {} {}", def.name, def.kind.as_str())), "{}", def.name);
    }
}

pub async fn a_running_request_is_listed_and_cancelled<D: Database + Admin>(db: &D) {
    let seq = commit(db, vec![node("a", &["P"], &[])]).await;
    std::thread::scope(|s| {
        // A long poll from a seq no commit reaches: it runs until cancelled
        let poll = s.spawn(|| {
            let options = QueryOptions { timeout: Some(Duration::from_secs(30)), ..options() };
            block_on(db.changes(NS, ChangesRequest { from_seq: seq + 1_000_000, wait: true }, options))
        });
        let start = std::time::Instant::now();
        let running = loop {
            let Listed { items, .. } = block_on(db.active_requests(None, None)).unwrap();
            if let Some(r) = items.into_iter().find(|r| r.operation == Operation::Changes) {
                break r;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "the long poll was never listed");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(running.namespace.as_deref(), Some(NS));
        assert!(running.cancellable);
        let cancelled = block_on(db.cancel_request(running.id, None)).unwrap();
        assert_eq!(cancelled.id, running.id);
        let e = poll.join().unwrap().unwrap_err();
        assert_eq!(e.code(), Code::Cancelled, "{}", e);
        // It ended: no longer listed, and a second cancel finds nothing
        assert_eq!(code(block_on(db.cancel_request(running.id, None))), Code::NotFound);
        let Listed { items, .. } = block_on(db.active_requests(None, None)).unwrap();
        assert!(items.iter().all(|r| r.id != running.id), "{:?}", items);
    });
    let status = db.server_status().await.unwrap();
    assert!(status.requests.cancelled >= 1, "{:?}", status.requests);
}

pub async fn admin_reads_are_bounded<D: Database + Admin>(db: &D) {
    // This call itself runs: a limit of 0 cuts it
    let none = db.active_requests(None, Some(0)).await.unwrap();
    assert!(none.items.is_empty() && none.truncated, "{:?}", none);
    let all = db.active_requests(None, Some(usize::MAX)).await.unwrap();
    assert!(!all.items.is_empty() && all.items.len() <= MAX_LIST);
    let mine = db.active_requests(Some("no-such-user".into()), None).await.unwrap();
    assert!(mine.items.is_empty() && !mine.truncated);
    let log = db.log(0, Some(usize::MAX)).await.unwrap();
    assert!(log.events.len() <= crate::log::MAX_READ);
    assert!(log.events.windows(2).all(|w| w[0].seq < w[1].seq));
    assert!(db.consumers().await.unwrap().len() <= crate::requests::MAX_CONSUMERS);
}

pub async fn consumers_report_their_lag<D: Database + Admin>(db: &D) {
    for id in ["a", "b", "c"] {
        commit(db, vec![node(id, &["P"], &[])]).await;
    }
    let batch = db.changes(NS, changes_from(1), options()).await.unwrap();
    let next = batch.value.next_seq;
    let reader = |list: Vec<crate::requests::ConsumerInfo>| {
        list.into_iter().find(|c| c.namespace == NS && c.next_seq == next).expect("the reader")
    };
    let first = reader(db.consumers().await.unwrap());
    assert_eq!((first.lag, first.polls), (0, 1), "{:?}", first);
    commit(db, vec![node("d", &["P"], &[])]).await;
    commit(db, vec![node("e", &["P"], &[])]).await;
    assert_eq!(reader(db.consumers().await.unwrap()).lag, 2);
}

// ---- the admin writes (step 16e, `Admin`) ----

pub async fn checkpoints_are_written_and_reported<D: Database + Admin>(db: &D) {
    let seq = commit(db, vec![node("a", &["P"], &[])]).await;
    let all = db.checkpoint(None).await.unwrap();
    let default = all.iter().find(|c| c.namespace == NS).expect("the default namespace");
    assert_eq!((default.outcome.seq, default.outcome.written), (seq, true), "{:?}", all);
    // Nothing new: nothing written
    let again = db.checkpoint(Some(NS.into())).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!((again[0].namespace.as_str(), again[0].outcome.written), (NS, false));
    assert_eq!(code(db.checkpoint(Some("nope".into())).await), Code::NotFound);
    let status = db.namespace_status(NS).await.unwrap();
    assert_eq!((status.checkpoint, status.since_checkpoint), (Some(seq), 0));
}

pub async fn backups_are_named_verified_and_never_overwrite<D: Database + Admin>(db: &D) {
    let seq = commit(db, vec![node("a", &["P"], &[])]).await;
    let request = |name: &str| crate::BackupRequest { name: name.into(), max_bytes_per_second: None, verify: true };
    let done = db.backup(request("b-1")).await.unwrap();
    let ns = done.report.namespace(NS).expect("the default namespace");
    assert_eq!(ns.seq, seq);
    assert!(done.report.path.ends_with("b-1") && done.report.bytes > 0, "{:?}", done.report);
    let verified = done.verify.expect("verified");
    assert!(verified.is_ok() && verified.kind == crate::admin::Kind::Backup, "{:?}", verified);
    // Never over anything
    assert_eq!(code(db.backup(request("b-1")).await), Code::Conflict);
    for bad in ["", "../b", "a/b", ".hidden", "..", "b\\c", "/abs", &"x".repeat(129)] {
        assert_eq!(code(db.backup(request(bad)).await), Code::InvalidArgument, "{:?}", bad);
    }
    // Throttled, and unverified
    let slow = crate::BackupRequest { name: "b-2".into(), max_bytes_per_second: Some(1 << 30), verify: false };
    assert!(db.backup(slow).await.unwrap().verify.is_none());
    // Verified again by name; a missing one isn't found
    let again = db.verify(crate::VerifyTarget::Backup("b-1".into())).await.unwrap();
    let named = again.namespaces.iter().find(|n| n.name == NS).map(|n| n.seq);
    assert!(again.is_ok() && named == Some(Some(seq)), "{:?}", again);
    assert_eq!(code(db.verify(crate::VerifyTarget::Backup("missing".into())).await), Code::NotFound);
    assert_eq!(code(db.verify(crate::VerifyTarget::Backup("../b-1".into())).await), Code::InvalidArgument);
}

pub async fn the_running_store_verifies<D: Database + Admin>(db: &D) {
    let seq = commit(db, vec![node("a", &["P"], &[])]).await;
    db.checkpoint(None).await.unwrap();
    commit(db, vec![node("b", &["P"], &[])]).await;
    let report = db.verify(crate::VerifyTarget::Store).await.unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!(report.kind, crate::admin::Kind::DataDir);
    let ns = report.namespaces.iter().find(|n| n.name == NS).expect("the default namespace");
    assert_eq!((ns.seq, ns.checkpoints_checked), (Some(seq + 1), 1), "{:?}", ns);
    let archive = db.verify(crate::VerifyTarget::Archive).await.unwrap();
    assert!(archive.is_ok() && archive.kind == crate::admin::Kind::Archive, "{:?}", archive);
}

pub async fn the_archive_is_pruned_before_a_backup<D: Database + Admin>(db: &D) {
    // Segments reach the archive when checkpoints remove them
    for round in 0..4 {
        for i in 0..20 {
            let id = format!("n{}-{}", round, i);
            commit(db, vec![node(&id, &["P"], &[("pad", Value::String("x".repeat(200)))])]).await;
        }
        db.checkpoint(Some(NS.into())).await.unwrap();
    }
    let request = crate::BackupRequest { name: "kept".into(), max_bytes_per_second: None, verify: false };
    db.backup(request).await.unwrap();
    let dry = db.prune_archive("kept".into(), true).await.unwrap();
    assert!(dry.dry_run && dry.backup.ends_with("kept"), "{:?}", dry);
    let pruned = db.prune_archive("kept".into(), false).await.unwrap();
    assert!(!pruned.dry_run);
    assert_eq!(pruned.namespaces, dry.namespaces, "a dry run says what goes");
    let again = db.prune_archive("kept".into(), false).await.unwrap();
    assert!(again.namespaces.iter().all(|n| n.removed_segments.is_empty()), "{:?}", again);
    let archive = db.verify(crate::VerifyTarget::Archive).await.unwrap();
    assert!(archive.is_ok(), "{:#?}", archive.problems);
    assert_eq!(code(db.prune_archive("missing".into(), false).await), Code::NotFound);
    assert_eq!(code(db.prune_archive("..".into(), false).await), Code::InvalidArgument);
}
