//! Traces (step 16g, ADR 0057): each request is one trace with the span
//! tree of guarantees.md, over gRPC and REST; an incoming `traceparent` is
//! its parent, a malformed one starts a new root; commits show their WAL
//! append and fsync per policy; a managed job is a trace of its own,
//! linked to its `StartJob`; no span name holds an id or a value.
//!
//! One subscriber per process, exporting to the SDK's in-memory exporter;
//! the tests take turns ([`turn`]) and read only the spans of their own
//! requests.

#![cfg(all(feature = "otel", feature = "rest"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use ironweaver_core::algo::PageRank;
use iwdb::{Embedded, FsyncPolicy, Mutation, QueryConfig, Store, StoreOptions, WalOptions};
use iwdb_query::auth::Operation;
use iwdb_query::exec::block_on;
use iwdb_query::trace::SpanCounters;
use iwdb_query::{Admin, AnalyticsRequest, Database, Job, ProjectionSpec, QueryOptions};
use iwdb_server::config::OtlpProtocol;
use iwdb_server::otel::{Settings, Tracing};
use iwdb_server::proto as pb;
use iwdb_server::proto::database_service_client::DatabaseServiceClient;
use opentelemetry::trace::{SpanId, SpanKind, Status, TraceId};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use support::{Running, options};
use tonic::codegen::http::Uri;
use tracing_subscriber::prelude::*;

const NS: &str = "default";

/// The names a span may have: the `iwdb.*` phases, and the operations.
const PHASES: &[&str] = &[
    "iwdb.queue",
    "iwdb.execute",
    "iwdb.commit",
    "iwdb.prepare",
    "iwdb.wal.append",
    "iwdb.wal.fsync",
    "iwdb.apply",
    "iwdb.changes.wait",
    "iwdb.collect",
    "iwdb.algorithm",
    "iwdb.checkpoint",
    "iwdb.checkpoint.replay",
    "iwdb.checkpoint.write",
    "iwdb.checkpoint.prune",
    "iwdb.backup",
    "iwdb.verify",
    "iwdb.verify.namespace",
    "iwdb.job",
    "iwdb.import",
];

struct Traces {
    exporter: InMemorySpanExporter,
    tracing: Tracing,
}

/// The process's subscriber, and a turn of the tests.
fn turn() -> (MutexGuard<'static, ()>, &'static Traces) {
    static TRACES: OnceLock<Traces> = OnceLock::new();
    static TURN: Mutex<()> = Mutex::new(());
    let traces = TRACES.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let settings = Settings {
            endpoint: String::new(),
            protocol: OtlpProtocol::Grpc,
            sample_ratio: 1.0,
            service_name: "iwdb-test".into(),
            headers: Vec::new(),
        };
        let export = exporter.clone();
        let tracing = Tracing::with_exporter(&settings, Arc::new(SpanCounters::default()), move || Ok(export)).unwrap();
        tracing_subscriber::registry().with(tracing.layer()).init();
        Traces { exporter, tracing }
    });
    let guard = TURN.lock().unwrap_or_else(|e| e.into_inner());
    traces.tracing.flush().unwrap();
    traces.exporter.reset();
    (guard, traces)
}

impl Traces {
    /// Every span exported so far.
    fn spans(&self) -> Vec<SpanData> {
        self.tracing.flush().unwrap();
        self.exporter.get_finished_spans().unwrap()
    }

    /// The spans of trace `id`.
    fn trace(&self, id: TraceId) -> Vec<SpanData> {
        self.spans().into_iter().filter(|s| s.span_context.trace_id() == id).collect()
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| kv.value.as_str().into_owned())
}

/// The children of `parent` in `spans`, by name.
fn children<'a>(spans: &'a [SpanData], parent: &SpanData) -> BTreeMap<String, &'a SpanData> {
    let id = parent.span_context.span_id();
    spans.iter().filter(|s| s.parent_span_id == id).map(|s| (s.name.to_string(), s)).collect()
}

fn one<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let found: Vec<&SpanData> = spans.iter().filter(|s| s.name == name).collect();
    assert_eq!(found.len(), 1, "one {} in {:?}", name, spans.iter().map(|s| &s.name).collect::<Vec<_>>());
    found[0]
}

fn names(map: &BTreeMap<String, &SpanData>) -> Vec<String> {
    map.keys().cloned().collect()
}

/// A trace id and a parent span id for a `traceparent` (sampled).
fn parent(n: u8) -> (TraceId, SpanId, String) {
    let trace = TraceId::from_hex(&format!("{:032x}", 0x0af7_6519_16cd_43dd_8448_eb21_1c80_3100_u128 + n as u128)).unwrap();
    let span = SpanId::from_hex(&format!("{:016x}", 0xb7ad_6b71_6920_3300_u64 + n as u64)).unwrap();
    (trace, span, format!("00-{}-{}-01", trace, span))
}

fn server_with(policy: FsyncPolicy) -> (Running<Embedded>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let options = StoreOptions { wal: WalOptions { fsync: policy, ..options().wal }, ..options() };
    let store = Store::open(dir.path(), options).unwrap();
    store.commit(&[node("n1")]).unwrap();
    (Running::start(Embedded::new(store, QueryConfig::default()).unwrap()), dir)
}

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["N".into()],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    }
}

type Raw = DatabaseServiceClient<hyper_util::client::legacy::Client<HttpConnector, tonic::body::Body>>;

fn raw(server: &Running<Embedded>) -> Raw {
    let client =
        hyper_util::client::legacy::Client::builder(TokioExecutor::new()).http2_only(true).build(HttpConnector::new());
    DatabaseServiceClient::with_origin(client, Uri::from_str(&server.endpoint()).unwrap())
}

/// `request` with `traceparent` (and `tracestate`), if given.
fn with_parent<T>(request: T, traceparent: Option<&str>) -> tonic::Request<T> {
    let mut request = tonic::Request::new(request);
    if let Some(value) = traceparent {
        request.metadata_mut().insert("traceparent", value.parse().unwrap());
        request.metadata_mut().insert("tracestate", "vendor=1".parse().unwrap());
    }
    request
}

fn get_nodes(server: &Running<Embedded>, traceparent: Option<&str>) {
    let request = pb::GetNodesRequest { namespace: NS.into(), ids: vec!["n1".into()], options: None };
    let mut stream = server.block_on(raw(server).get_nodes(with_parent(request, traceparent))).unwrap().into_inner();
    while server.block_on(stream.message()).unwrap().is_some() {}
}

fn commit(server: &Running<Embedded>, traceparent: &str) {
    let upsert = pb::UpsertNode { id: "n2".into(), ..Default::default() };
    let request = pb::CommitRequest {
        namespace: NS.into(),
        mutations: vec![pb::Mutation { kind: Some(pb::mutation::Kind::UpsertNode(upsert)) }],
        options: None,
    };
    server.block_on(raw(server).commit(with_parent(request, Some(traceparent)))).unwrap();
}

/// `GET path` over HTTP/1.1 with `headers`: the status.
fn rest_get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut request = format!("GET {} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n", path);
    for (name, value) in headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    answer.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// A read's tree: the request (a server span, the parent's child), then
/// `iwdb.queue` and `iwdb.execute`, with the documented attributes.
fn check_read(spans: &[SpanData], op: &str, parent: SpanId) {
    let request = one(spans, op);
    assert_eq!(request.parent_span_id, parent);
    assert!(request.parent_span_is_remote);
    assert_eq!(request.span_kind, SpanKind::Server);
    assert_eq!(request.status, Status::Unset);
    assert_eq!(attr(request, "db.operation.name").as_deref(), Some(op));
    assert_eq!(attr(request, "db.system.name").as_deref(), Some("ironweaver_db"));
    assert_eq!(attr(request, "db.namespace").as_deref(), Some(NS));
    assert_eq!(attr(request, "iwdb.outcome").as_deref(), Some("ok"));
    assert!(attr(request, "iwdb.request_id").is_some_and(|id| id.parse::<u64>().is_ok()));
    assert_eq!(attr(request, "iwdb.max_results").as_deref(), Some("1000"));
    assert_eq!(attr(request, "iwdb.timeout_ms").as_deref(), Some("30000"));
    assert_eq!(attr(request, "iwdb.seq").as_deref(), Some("1"));
    assert_eq!(attr(request, "iwdb.truncated").as_deref(), Some("false"));
    // The tracestate travels with the context
    assert_eq!(request.span_context.trace_state().header(), "vendor=1");
    assert_eq!(names(&children(spans, request)), ["iwdb.execute", "iwdb.queue"]);
    assert_eq!(spans.len(), 3, "{:?}", spans.iter().map(|s| &s.name).collect::<Vec<_>>());
}

#[test]
fn a_grpc_request_is_one_trace_under_its_callers_span() {
    let (_turn, traces) = turn();
    let (server, _dir) = server_with(FsyncPolicy::Always);
    let (trace, span, header) = parent(1);
    get_nodes(&server, Some(&header));
    check_read(&traces.trace(trace), "GetNodes", span);
}

#[test]
fn a_rest_request_is_one_trace_under_its_callers_span() {
    let (_turn, traces) = turn();
    let (server, _dir) = server_with(FsyncPolicy::Always);
    let (trace, span, header) = parent(2);
    let status = rest_get(server.addr, "/v1/namespaces/default/nodes/n1", &[
        ("traceparent", &header),
        ("tracestate", "vendor=1"),
    ]);
    assert_eq!(status, 200);
    check_read(&traces.trace(trace), "GetNodes", span);
}

#[test]
fn a_malformed_or_missing_traceparent_starts_a_new_root() {
    let (_turn, traces) = turn();
    let (server, _dir) = server_with(FsyncPolicy::Always);
    for header in [Some("00-not-a-trace-01"), Some("00-00000000000000000000000000000000-b7ad6b7169203331-01"), None] {
        traces.exporter.reset();
        get_nodes(&server, header);
        let spans = traces.spans();
        let request = one(&spans, "GetNodes");
        assert_eq!(request.parent_span_id, SpanId::INVALID, "{:?}", header);
        assert!(!request.parent_span_is_remote);
        assert_eq!(names(&children(&spans, request)), ["iwdb.execute", "iwdb.queue"]);
    }
    // REST too, and the request still succeeds
    traces.exporter.reset();
    assert_eq!(rest_get(server.addr, "/v1/namespaces/default/nodes/n1", &[("traceparent", "garbage")]), 200);
    assert_eq!(one(&traces.spans(), "GetNodes").parent_span_id, SpanId::INVALID);
}

#[test]
fn a_parent_that_did_not_sample_is_followed() {
    let (_turn, traces) = turn();
    let (server, _dir) = server_with(FsyncPolicy::Always);
    let (trace, span, _) = parent(3);
    get_nodes(&server, Some(&format!("00-{}-{}-00", trace, span)));
    assert!(traces.trace(trace).is_empty());
}

/// A commit's tree under each fsync policy: the commit pipeline's phases,
/// and an fsync where the commit pays one.
#[test]
fn a_write_shows_its_commit_append_and_fsync_per_policy() {
    let (_turn, traces) = turn();
    let group = |max_batch| FsyncPolicy::Group { max_delay: Duration::from_secs(60), max_batch };
    let cases = [
        ("always", FsyncPolicy::Always, Some("always")),
        ("group closing a batch", group(1), Some("group")),
        ("group within a batch", group(64), None),
        ("off", FsyncPolicy::Off, None),
    ];
    for (n, (what, policy, fsync)) in cases.into_iter().enumerate() {
        let (server, _dir) = server_with(policy);
        let (trace, span, header) = parent(10 + n as u8);
        commit(&server, &header);
        let spans = traces.trace(trace);
        let request = one(&spans, "Commit");
        assert_eq!(request.parent_span_id, span, "{}", what);
        assert_eq!(attr(request, "iwdb.seq").as_deref(), Some("2"), "{}", what);
        let execute = children(&spans, request)["iwdb.execute"];
        let commit = children(&spans, execute)["iwdb.commit"];
        assert_eq!(attr(commit, "iwdb.seq").as_deref(), Some("2"));
        assert_eq!(attr(commit, "iwdb.ops").as_deref(), Some("1"));
        assert_eq!(names(&children(&spans, commit)), ["iwdb.apply", "iwdb.prepare", "iwdb.wal.append"], "{}", what);
        let append = children(&spans, commit)["iwdb.wal.append"];
        assert!(attr(append, "iwdb.wal.bytes").is_some_and(|b| b.parse::<u64>().unwrap() > 0));
        assert_eq!(attr(append, "iwdb.wal.synced").as_deref(), Some(if fsync.is_some() { "true" } else { "false" }), "{}", what);
        let synced = children(&spans, append);
        match fsync {
            Some(policy) => {
                let fsync = synced["iwdb.wal.fsync"];
                assert_eq!(attr(fsync, "iwdb.wal.fsync_policy").as_deref(), Some(policy), "{}", what);
                assert_eq!(attr(fsync, "iwdb.wal.batch").as_deref(), Some("1"), "{}", what);
            }
            None => assert!(synced.is_empty(), "{}: {:?}", what, names(&synced)),
        }
    }
}

/// A refused request is a span too, with its outcome as an error status.
#[test]
fn a_failed_request_records_its_code_and_an_error_status() {
    let (_turn, traces) = turn();
    let (server, _dir) = server_with(FsyncPolicy::Always);
    let (trace, _, header) = parent(20);
    let request = pb::GetNodesRequest { namespace: "missing".into(), ids: vec!["n1".into()], options: None };
    let failed = server.block_on(raw(&server).get_nodes(with_parent(request, Some(&header))));
    let status = match failed {
        Err(status) => status,
        Ok(stream) => server.block_on(stream.into_inner().message()).unwrap_err(),
    };
    assert_eq!(status.code(), tonic::Code::NotFound);
    let spans = traces.trace(trace);
    let request = one(&spans, "GetNodes");
    assert_eq!(attr(request, "iwdb.outcome").as_deref(), Some("not_found"));
    assert!(matches!(request.status, Status::Error { ref description } if description.is_empty()));
}

/// A managed job is a trace of its own, linked to the StartJob request.
#[test]
fn a_job_is_a_trace_linked_to_its_start() {
    let (_turn, traces) = turn();
    let (server, _dir) = server_with(FsyncPolicy::Always);
    let client = server.client();
    let request = AnalyticsRequest {
        projection: ProjectionSpec::default(),
        job: Job::PageRank(PageRank { max_iter: 5, ..PageRank::default() }),
    };
    let job = block_on(client.start_job(NS.into(), request, QueryOptions::default(), None)).unwrap();
    while !block_on(client.job(job.id, None)).unwrap().state.ended() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let spans = traces.spans();
    let start = one(&spans, "StartJob");
    let job_span = one(&spans, "iwdb.job");
    assert_eq!(job_span.parent_span_id, SpanId::INVALID);
    assert_ne!(job_span.span_context.trace_id(), start.span_context.trace_id());
    let links: Vec<_> = job_span.links.iter().map(|l| l.span_context.span_id()).collect();
    assert_eq!(links, [start.span_context.span_id()]);
    assert_eq!(attr(job_span, "iwdb.request_id"), Some(job.id.to_string()));
    assert_eq!(attr(job_span, "iwdb.job.kind").as_deref(), Some("page_rank"));
    assert_eq!(attr(job_span, "iwdb.outcome").as_deref(), Some("ok"));
    assert_eq!(attr(job_span, "iwdb.job.nodes").as_deref(), Some("1"));
    let mut tree = names(&children(&spans, job_span));
    tree.sort();
    assert_eq!(tree, ["iwdb.algorithm", "iwdb.collect", "iwdb.queue"]);
}

/// No span name holds an id, a namespace's name or a value: every name is
/// a phase or an operation, whatever the requests carried.
#[test]
fn no_span_name_holds_an_id_or_a_value() {
    let (_turn, traces) = turn();
    let (server, _dir) = server_with(FsyncPolicy::Always);
    let client = server.client();
    block_on(client.create_namespace("secret-namespace-name", None)).unwrap();
    block_on(client.commit("secret-namespace-name", vec![node("secret-node-id")], Default::default())).unwrap();
    block_on(client.get_nodes("secret-namespace-name", vec!["secret-node-id".into()], QueryOptions::default())).unwrap();
    block_on(client.checkpoint(Some("secret-namespace-name".into()))).unwrap();
    rest_get(server.addr, "/v1/namespaces/secret-namespace-name/nodes/secret-node-id", &[]);
    let spans = traces.spans();
    assert!(spans.len() > 10, "{}", spans.len());
    let operations: Vec<&str> = Operation::ALL.iter().map(|op| op.name()).collect();
    for span in &spans {
        assert!(
            PHASES.contains(&span.name.as_ref()) || operations.contains(&span.name.as_ref()),
            "span name {:?}",
            span.name
        );
        // Nor does any attribute hold the node's id
        for kv in &span.attributes {
            assert!(!kv.value.as_str().contains("secret-node-id"), "{} on {}", kv.key, span.name);
        }
    }
    assert!(spans.iter().any(|s| s.name == "iwdb.checkpoint"));
}
