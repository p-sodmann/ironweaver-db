//! What the gRPC adapter adds on top of the trait: error codes in the
//! trailer, `grpc-timeout`, values and filters at the depth limit, answers
//! streamed in chunks, a client that goes away, a commit that outlives its
//! client, and many clients writing one namespace.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use ironweaver_core::{Expr, Value};
use iwdb::{Embedded, QueryConfig, Store};
use iwdb_engine::{IdempotencyKey, Mutation};
use iwdb_query::exec::block_on;
use iwdb_query::{Code, CommitOptions, Database, FindRequest, MatchRequest, QueryOptions};
use iwdb_server::CHUNK_BYTES;
use iwdb_server::proto as pb;
use iwdb_server::proto::database_service_client::DatabaseServiceClient;
use iwdb_server::status::CODE_KEY;
use iwdb_storage::failpoint::{Action, Call, FailFs, Rule, When};
use support::{Running, fresh, options};
use tonic::codegen::http::Uri;

mod support;

const NS: &str = "default";

type Raw = DatabaseServiceClient<hyper_util::client::legacy::Client<HttpConnector, tonic::body::Body>>;

/// A client without tonic's channel: it sends `grpc-timeout` without
/// enforcing it, and whatever bytes a test puts in a message.
fn raw<D: Database + 'static>(server: &Running<D>) -> Raw {
    let client =
        hyper_util::client::legacy::Client::builder(TokioExecutor::new()).http2_only(true).build(HttpConnector::new());
    DatabaseServiceClient::with_origin(client, Uri::from_str(&server.endpoint()).unwrap())
}

fn node(id: &str, attr: Vec<(&str, Value)>) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["N".into()],
        attr: attr.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        meta: Default::default(),
        expected_version: None,
    }
}

fn store(dir: &std::path::Path, workers: usize) -> Embedded {
    let config = QueryConfig { workers, ..QueryConfig::default() };
    Embedded::new(Store::open(dir, options()).unwrap(), config).unwrap()
}

fn code_of(status: &tonic::Status) -> Option<&str> {
    status.metadata().get(CODE_KEY).and_then(|v| v.to_str().ok())
}

fn postcard_value(bytes: Vec<u8>) -> pb::Value {
    pb::Value { form: Some(pb::value::Form::Postcard(bytes)) }
}

#[test]
fn errors_carry_their_code_in_the_trailer() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(store(dir.path(), 2));
    let mut client = raw(&server);
    // A message that isn't complete is the request's fault (the other codes:
    // `status.rs` and the conformance suite)
    let empty = pb::CommitRequest { namespace: NS.into(), mutations: vec![pb::Mutation { kind: None }], options: None };
    let status = server.block_on(client.commit(empty)).unwrap_err();
    assert_eq!((status.code(), code_of(&status)), (tonic::Code::InvalidArgument, Some("invalid_argument")));
    assert!(status.message().contains("kind of a mutation"), "{}", status.message());
}

#[test]
fn grpc_timeout_bounds_a_read_and_the_smaller_timeout_wins() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(store(dir.path(), 2));
    let client = raw(&server);
    let wait = |timeout_ms: Option<u32>, header: &str| {
        let options = pb::QueryOptions { timeout_ms, ..Default::default() };
        let mut request =
            tonic::Request::new(pb::WaitForSeqRequest { namespace: NS.into(), seq: 1_000, options: Some(options) });
        request.metadata_mut().insert("grpc-timeout", header.parse().unwrap());
        let start = Instant::now();
        let status = server.block_on(client.clone().wait_for_seq(request)).unwrap_err();
        (status, start.elapsed())
    };
    for (timeout_ms, header, expected) in [(None, "150m", 150), (Some(150), "10S", 150), (Some(60_000), "150m", 150)] {
        let (status, took) = wait(timeout_ms, header);
        assert_eq!((status.code(), code_of(&status)), (tonic::Code::DeadlineExceeded, Some("timeout")));
        let expected = Duration::from_millis(expected);
        assert!(took >= expected && took < expected + Duration::from_secs(5), "{:?} for {:?}", took, header);
    }
    // The server's maximum (5 minutes) caps an hour
    let (status, took) = wait(Some(100), "1H");
    assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
    assert!(took < Duration::from_secs(5));
}

fn nest(depth: usize) -> Value {
    (1..depth).fold(Value::Int(1), |v, _| Value::List(vec![v]))
}

/// A filter of `depth` levels.
fn nest_expr(depth: usize) -> Expr {
    (1..depth).fold(Expr::Exists { path: vec!["deep".into()] }, |e, _| Expr::Not(Box::new(Expr::Not(Box::new(e)))))
}

#[test]
fn values_and_filters_nested_100_levels_pass_and_101_fail_with_the_cores_message() {
    let db = fresh();
    let deep = nest(100);
    block_on(db.commit(NS, vec![node("a", vec![("deep", deep.clone())])], CommitOptions::default())).unwrap();
    let read = block_on(db.get_nodes(NS, vec!["a".into()], QueryOptions::default())).unwrap();
    assert_eq!(read.value[0].as_ref().unwrap().attr.get("deep"), Some(&deep));
    // A filter 99 levels deep, with a value 100 levels deep inside
    let filter = (1..50).fold(
        Expr::Compare { path: vec!["deep".into()], op: ironweaver_core::CmpOp::Eq, value: deep.clone() },
        |e, _| Expr::Not(Box::new(Expr::Not(Box::new(e)))),
    );
    assert_eq!(filter.depth(), 99);
    let found = block_on(db.find(NS, FindRequest { filter }, QueryOptions::default())).unwrap();
    assert_eq!(found.value.len(), 1);
    // Too deep: the client refuses to encode it, with the core's message
    let e = block_on(db.commit(NS, vec![node("b", vec![("deep", nest(101))])], CommitOptions::default())).unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    assert!(e.message().contains("nested more than 100 levels"), "{}", e);
    let e = block_on(db.find(NS, FindRequest { filter: nest_expr(51) }, QueryOptions::default())).unwrap_err();
    assert!(e.message().contains("nested more than 100 levels"), "{}", e);
}

#[test]
fn the_server_refuses_bytes_nested_101_levels_with_the_cores_message() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(store(dir.path(), 2));
    let mut client = raw(&server);
    // A list of one item is the variant index, a length of 1, then the item
    let list = postcard::to_stdvec(&Value::List(vec![])).unwrap()[0];
    let mut bytes = [list, 1].repeat(100);
    bytes.extend_from_slice(&postcard::to_stdvec(&Value::Int(1)).unwrap());
    let upsert = pb::UpsertNode {
        id: "x".into(),
        attr: [("deep".to_owned(), postcard_value(bytes))].into(),
        ..Default::default()
    };
    let commit = pb::CommitRequest {
        namespace: NS.into(),
        mutations: vec![pb::Mutation { kind: Some(pb::mutation::Kind::UpsertNode(upsert)) }],
        options: None,
    };
    let status = server.block_on(client.commit(commit)).unwrap_err();
    assert_eq!((status.code(), code_of(&status)), (tonic::Code::InvalidArgument, Some("invalid_argument")));
    assert!(status.message().contains("value of 'deep'"), "{}", status.message());
    assert!(status.message().contains("nested more than 100 levels"), "#29: {}", status.message());
    // A filter: `Not` is variant 8 of `Expr`
    let not = postcard::to_stdvec(&Expr::Not(Box::new(Expr::Const(true)))).unwrap()[0];
    let mut bytes = vec![not; 100];
    bytes.extend_from_slice(&postcard::to_stdvec(&Expr::Const(true)).unwrap());
    let find = pb::FindRequest {
        namespace: NS.into(),
        filter: Some(pb::Expr { form: Some(pb::expr::Form::Postcard(bytes)) }),
        options: None,
    };
    let status = server.block_on(client.find(find)).unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("expression nested more than 100 levels"), "#29: {}", status.message());
}

#[test]
fn large_answers_stream_in_chunks_with_the_meta_last() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(store(dir.path(), 2));
    let remote = server.client();
    let padding = Value::String("x".repeat(2000));
    let nodes: Vec<Mutation> = (0..2000).map(|i| node(&format!("n{:04}", i), vec![("p", padding.clone())])).collect();
    block_on(remote.commit(NS, nodes, CommitOptions::default())).unwrap();
    // About 4 MB of nodes: more than the default gRPC message limit
    let request = FindRequest { filter: Expr::Label("N".into()) };
    let o = QueryOptions::default().with_limits(Some(5000), None, None);
    let answer = block_on(remote.find(NS, request.clone(), o.clone())).unwrap();
    assert_eq!(answer.value.len(), 2000);
    assert!(answer.value.windows(2).all(|w| w[0].id < w[1].id));
    assert!(answer.next.is_none() && !answer.truncated);
    let mut client = raw(&server);
    let find = pb::FindRequest {
        namespace: NS.into(),
        filter: Some(pb::Expr { form: Some(pb::expr::Form::Postcard(postcard::to_stdvec(&request.filter).unwrap())) }),
        options: Some(pb::QueryOptions {
            limits: Some(pb::Limits { max_results: Some(5000), ..Default::default() }),
            ..Default::default()
        }),
    };
    let chunks: Vec<pb::FindResponse> = server.block_on(async {
        let mut stream = client.find(find).await.unwrap().into_inner();
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.message().await.unwrap() {
            chunks.push(chunk);
        }
        chunks
    });
    assert!(chunks.len() >= 4, "{} chunks", chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.meta.is_some(), i == chunks.len() - 1);
        assert!(prost::Message::encoded_len(chunk) <= CHUNK_BYTES + 1024);
    }
    assert_eq!(chunks.iter().map(|c| c.nodes.len()).sum::<usize>(), 2000);
    // An empty answer is one message with the meta
    let none = pb::FindRequest {
        namespace: NS.into(),
        filter: Some(pb::Expr {
            form: Some(pb::expr::Form::Postcard(postcard::to_stdvec(&Expr::Label("M".into())).unwrap())),
        }),
        options: None,
    };
    let chunks = server.block_on(async {
        let mut stream = client.find(none).await.unwrap().into_inner();
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.message().await.unwrap() {
            chunks.push(chunk);
        }
        chunks
    });
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].nodes.is_empty() && chunks[0].meta.is_some());
}

/// A complete graph of `n` nodes: billions of trails for `(a)-[*1..10]->(b)`.
fn complete(n: usize) -> Vec<Mutation> {
    let mut m: Vec<Mutation> = (0..n).map(|i| node(&format!("n{}", i), vec![])).collect();
    for a in 0..n {
        for b in 0..n {
            if a != b {
                m.push(Mutation::AddEdge {
                    from: format!("n{}", a),
                    to: format!("n{}", b),
                    ty: None,
                    attr: Default::default(),
                    meta: Default::default(),
                });
            }
        }
    }
    m
}

#[test]
fn a_client_that_goes_away_stops_its_read() {
    let dir = tempfile::tempdir().unwrap();
    // One worker: while the long read runs, nothing else can
    let server = Running::start(store(dir.path(), 1));
    let remote = server.client();
    block_on(remote.commit(NS, complete(30), CommitOptions::default())).unwrap();
    let long = QueryOptions { timeout: Some(Duration::from_secs(120)), ..QueryOptions::default() }.with_limits(
        Some(usize::MAX),
        Some(usize::MAX),
        Some(usize::MAX),
    );
    let request = MatchRequest::parse("(a)-[*1..10]->(b)").unwrap();
    // The call starts at once; it holds the only worker
    let call = remote.match_pattern(NS, request, long);
    thread::sleep(Duration::from_millis(300));
    // A read behind it times out at its deadline (it waits for the worker)
    let busy = QueryOptions { timeout: Some(Duration::from_millis(200)), ..QueryOptions::default() };
    let start = Instant::now();
    let e = block_on(remote.get_nodes(NS, vec!["n0".into()], busy)).unwrap_err();
    assert_eq!(e.code(), Code::Timeout, "the long read holds the worker: {}", e);
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
    // The client goes away: the server drops the call and cancels the read
    drop(call);
    let start = Instant::now();
    let quick = QueryOptions { timeout: Some(Duration::from_secs(20)), ..QueryOptions::default() };
    let read = block_on(remote.get_nodes(NS, vec!["n0".into()], quick)).unwrap();
    assert!(read.value[0].is_some());
    assert!(start.elapsed() < Duration::from_secs(5), "the worker was free after {:?}", start.elapsed());
}

#[test]
fn a_commit_runs_to_the_end_after_its_client_went_away() {
    let dir = tempfile::tempdir().unwrap();
    let fs = FailFs::new();
    let db = support::embedded(fs.clone(), dir.path(), options());
    let server = Running::start(db);
    let remote = server.client();
    // The next WAL fsync pauses until the test lets it go on
    let (paused_tx, paused) = mpsc::channel::<()>();
    let (resume, resumed) = mpsc::channel::<()>();
    let (paused_tx, resumed) = (Mutex::new(paused_tx), Mutex::new(resumed));
    fs.set_pause(Some(Arc::new(move |_, _| {
        let _ = paused_tx.lock().unwrap().send(());
        let _ = resumed.lock().unwrap().recv();
    })));
    fs.add(Rule::new(Call::Sync, When::Before, Action::Pause).path("/wal/"));
    let key = IdempotencyKey::new("commit-1").unwrap();
    let keyed = || CommitOptions { idempotency_key: Some(key.clone()) };
    let call = remote.commit(NS, vec![node("a", vec![])], keyed());
    paused.recv_timeout(Duration::from_secs(10)).expect("the commit reached its fsync");
    // The client gives up (a deadline, a lost connection): the server drops
    // the call, but the commit, whose record is written, goes on
    drop(call);
    thread::sleep(Duration::from_millis(100));
    resume.send(()).unwrap();
    let o = QueryOptions { timeout: Some(Duration::from_secs(10)), ..QueryOptions::min_seq(1) };
    let read = block_on(remote.get_nodes(NS, vec!["a".into()], o)).unwrap();
    assert!(read.value[0].is_some(), "the commit was applied");
    // A retry with the key finds it
    let retry = block_on(remote.commit(NS, vec![node("a", vec![])], keyed())).unwrap();
    assert!(retry.deduplicated && retry.seq == 1);
}

#[test]
fn many_clients_write_and_read_one_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(store(dir.path(), 4));
    let endpoint = server.endpoint();
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let endpoint = endpoint.clone();
            thread::spawn(move || {
                let writer = iwdb_server::client::Remote::connect(&endpoint).unwrap();
                let reader = iwdb_server::client::Remote::connect(&endpoint).unwrap();
                for i in 0..25 {
                    let id = format!("t{}-{:02}", t, i);
                    let key = Some(IdempotencyKey::new(format!("k-{}", id)).unwrap());
                    let options = CommitOptions { idempotency_key: key };
                    let mutations = vec![node(&id, vec![("i", Value::Int(i))])];
                    let seq = block_on(writer.commit(NS, mutations.clone(), options.clone())).unwrap().seq;
                    // Read-your-writes through another connection
                    let read = block_on(reader.get_nodes(NS, vec![id.clone()], QueryOptions::min_seq(seq))).unwrap();
                    assert!(read.seq >= seq && read.value[0].is_some(), "{}", id);
                    // A retry applies nothing
                    let again = block_on(writer.commit(NS, mutations, options)).unwrap();
                    assert!(again.deduplicated && again.seq == seq);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let remote = server.client();
    let all = block_on(remote.find(NS, FindRequest { filter: Expr::Label("N".into()) }, QueryOptions::default()));
    let all = all.unwrap();
    assert_eq!((all.value.len(), all.seq), (200, 200));
}

/// A server built without `rest` (ADR 0034) serves gRPC only: any other
/// request, on HTTP/1.1 too, is answered 404 and the connection stays usable.
#[cfg(not(feature = "rest"))]
#[test]
fn without_rest_every_other_request_is_not_found() {
    use std::io::{Read, Write};
    let db = fresh();
    let address = db.server.endpoint().trim_start_matches("http://").to_owned();
    let mut stream = std::net::TcpStream::connect(&address).unwrap();
    stream.write_all(b"GET /v1/openapi.json HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 404"), "{}", answer);
    assert_eq!(block_on(db.namespaces()).unwrap().len(), 1);
}
