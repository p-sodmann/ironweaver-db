//! The change stream over the network (ADR 0031): gRPC `Watch` follows
//! commits with heartbeats; SSE events, resuming with `Last-Event-ID`;
//! errors before the first event are statuses; `not_retained` maps to
//! `OUT_OF_RANGE` and 410; and both streams end when the server shuts down.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, StatusCode, header};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use ironweaver_core::Value;
use iwdb::{Embedded, QueryConfig, Store, StoreOptions};
use iwdb_engine::Mutation;
use iwdb_query::exec::block_on;
use iwdb_query::{ChangesRequest, Code, CommitOptions, Database, QueryOptions};
use iwdb_server::proto as pb;
use iwdb_server::proto::database_service_client::DatabaseServiceClient;
use support::{Running, options};

mod support;

const NS: &str = "default";

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["N".into()],
        attr: [("x".to_owned(), Value::Int(1))].into_iter().collect(),
        meta: Default::default(),
        expected_version: None,
    }
}

fn commit(db: &impl Database, id: &str) -> u64 {
    block_on(db.commit(NS, vec![node(id)], CommitOptions::default())).unwrap().seq
}

fn served(store_options: StoreOptions) -> (tempfile::TempDir, Running<Embedded>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), store_options).unwrap();
    (dir, Running::start(Embedded::new(store, QueryConfig::default()).unwrap()))
}

fn watch_request(from_seq: u64, timeout_ms: u32) -> pb::WatchRequest {
    pb::WatchRequest {
        namespace: NS.into(),
        from_seq,
        options: Some(pb::QueryOptions { timeout_ms: Some(timeout_ms), ..Default::default() }),
    }
}

fn seqs(message: &pb::WatchResponse) -> Vec<u64> {
    message.events.iter().map(|e| e.seq).collect()
}

#[test]
fn watch_follows_commits_with_heartbeats() {
    let (_dir, server) = served(options());
    let client = server.client();
    for id in ["a", "b", "c"] {
        commit(&client, id);
    }
    let endpoint = server.endpoint();
    server.block_on(async {
        let mut grpc = DatabaseServiceClient::connect(endpoint).await.unwrap();
        let mut stream = grpc.watch(watch_request(2, 200)).await.unwrap().into_inner();
        let first = stream.message().await.unwrap().unwrap();
        assert_eq!(seqs(&first), [2, 3]);
        assert_eq!((first.next_seq, first.first_seq), (4, 1));
        // Nothing new: a heartbeat after about the timeout
        let start = Instant::now();
        let heartbeat = stream.message().await.unwrap().unwrap();
        assert!(heartbeat.events.is_empty() && heartbeat.next_seq == 4);
        assert!(start.elapsed() >= Duration::from_millis(100), "{:?}", start.elapsed());
        // A commit while it waits arrives at once
        let committer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            commit(&client, "d")
        });
        let next = loop {
            let message = stream.message().await.unwrap().unwrap();
            if !message.events.is_empty() {
                break message;
            }
        };
        assert_eq!(seqs(&next), [committer.join().unwrap()]);
        let Some(pb::change_event::Change::Data(data)) = &next.events[0].change else { panic!("a data change") };
        assert!(matches!(data.ops[0].kind, Some(pb::change_op::Kind::AddNode(_))));
    });
}

#[test]
fn watch_errors_end_the_stream_with_their_status() {
    let (_dir, server) = served(options());
    let endpoint = server.endpoint();
    server.block_on(async {
        let mut grpc = DatabaseServiceClient::connect(endpoint).await.unwrap();
        let mut request = watch_request(1, 200);
        request.namespace = "nope".into();
        let error = match grpc.watch(request).await {
            Ok(response) => response.into_inner().message().await.unwrap_err(),
            Err(status) => status,
        };
        assert_eq!(error.code(), tonic::Code::NotFound);
    });
}

/// A store whose first WAL segments are gone: small segments, a
/// checkpoint, and only one checkpoint kept.
fn truncated_store(dir: &std::path::Path) -> (Embedded, u64) {
    let mut o = options();
    o.checkpoint.keep = 1;
    let store = Store::open(dir, o).unwrap();
    let db = Embedded::new(store, QueryConfig::default()).unwrap();
    for i in 0..60 {
        commit(&db, &format!("n{}", i));
    }
    db.store().checkpoint().unwrap();
    let first = block_on(db.changes(NS, ChangesRequest { from_seq: 60, wait: false }, QueryOptions::default()))
        .unwrap()
        .value
        .first_seq;
    assert!(first > 1, "segments were removed");
    (db, first)
}

#[test]
fn seqs_that_are_not_retained_are_out_of_range_and_gone() {
    let dir = tempfile::tempdir().unwrap();
    let (db, first) = truncated_store(dir.path());
    let error = block_on(db.changes(NS, ChangesRequest { from_seq: 1, wait: false }, QueryOptions::default()));
    let error = error.unwrap_err();
    assert_eq!(error.code(), Code::NotRetained);
    assert!(error.message().contains(&format!("oldest retained seq is {}", first)), "{}", error);

    let server = Running::start(db);
    let endpoint = server.endpoint();
    server.block_on(async {
        let mut grpc = DatabaseServiceClient::connect(endpoint).await.unwrap();
        let request = pb::GetChangesRequest { namespace: NS.into(), from_seq: 1, ..Default::default() };
        let status = grpc.get_changes(request).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::OutOfRange);
    });
    let rest = server.rest_client();
    let error = block_on(rest.changes(NS, ChangesRequest { from_seq: 1, wait: false }, QueryOptions::default()));
    assert_eq!(error.unwrap_err().code(), Code::NotRetained);
    let (status, body) = sse(&server, "/v1/namespaces/default/changes/stream?from_seq=1", None, |_| false);
    assert_eq!(status, StatusCode::GONE);
    assert!(body.contains("not_retained"), "{}", body);
}

/// GET an SSE route and read its body until `enough` says so (or it
/// ends); the status and what was read.
fn sse(
    server: &Running<Embedded>,
    path: &str,
    last_event_id: Option<&str>,
    enough: impl Fn(&str) -> bool,
) -> (StatusCode, String) {
    let url = format!("{}{}", server.endpoint(), path);
    server.block_on(async {
        let client = Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>();
        let mut request = Request::get(url).header(header::ACCEPT, "text/event-stream");
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let response = client.request(request.body(Empty::new()).unwrap()).await.unwrap();
        let status = response.status();
        let mut body = response.into_body();
        let mut text = String::new();
        let deadline = tokio::time::sleep(Duration::from_secs(20));
        tokio::pin!(deadline);
        while !enough(&text) {
            tokio::select! {
                frame = body.frame() => match frame {
                    Some(frame) => {
                        if let Ok(data) = frame.unwrap().into_data() {
                            text.push_str(std::str::from_utf8(&data).unwrap());
                        }
                    }
                    None => break,
                },
                () = &mut deadline => panic!("no end in sight: {}", text),
            }
        }
        (status, text)
    })
}

/// The `id`s of the `change` events in an SSE body.
fn event_ids(body: &str) -> Vec<u64> {
    body.split("\n\n")
        .filter(|e| e.contains("event: change"))
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("id: ")))
        .map(|id| id.parse().unwrap())
        .collect()
}

#[test]
fn server_sent_events_resume_after_the_last_event_id() {
    let (_dir, server) = served(options());
    let client = server.client();
    for id in ["a", "b", "c", "d"] {
        commit(&client, id);
    }
    let path = "/v1/namespaces/default/changes/stream?from_seq=1&timeout_ms=200";
    let (status, body) = sse(&server, path, None, |b| event_ids(b).len() >= 4);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(event_ids(&body), [1, 2, 3, 4]);
    let data = body.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
    let event: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(event["seq"], "1");
    assert_eq!(event["data"]["ops"][0]["addNode"]["id"], "a");
    assert_eq!(event["data"]["ops"][0]["addNode"]["data"]["attr"]["x"], serde_json::json!({ "Int": 1 }));

    // What EventSource sends when it reconnects
    let (_, body) = sse(&server, path, Some("2"), |b| event_ids(b).len() >= 2);
    assert_eq!(event_ids(&body), [3, 4]);
    // Heartbeats while nothing happens
    let (_, body) = sse(&server, path, Some("4"), |b| b.matches(": next seq 5").count() >= 2);
    assert!(event_ids(&body).is_empty());
}

#[test]
fn server_sent_events_refuse_bad_requests_with_a_status() {
    let (_dir, server) = served(options());
    let cases = [
        ("/v1/namespaces/nope/changes/stream", None, StatusCode::NOT_FOUND),
        ("/v1/namespaces/default/changes/stream?wait=true", None, StatusCode::BAD_REQUEST),
        ("/v1/namespaces/default/changes/stream?max_visited=3", None, StatusCode::BAD_REQUEST),
        ("/v1/namespaces/default/changes/stream", Some("x"), StatusCode::BAD_REQUEST),
    ];
    for (path, id, expected) in cases {
        let (status, body) = sse(&server, path, id, |_| false);
        assert_eq!(status, expected, "{}: {}", path, body);
        assert!(body.contains("\"code\""), "{}", body);
    }
}

#[test]
fn streams_end_when_the_server_shuts_down() {
    let (_dir, server) = served(options());
    commit(&server.client(), "a");
    let endpoint = server.endpoint();
    let url = format!("{}/v1/namespaces/default/changes/stream?from_seq=1&timeout_ms=60000", endpoint);
    let (grpc_done, sse_done) = (std::sync::mpsc::channel(), std::sync::mpsc::channel());
    let (grpc_tx, sse_tx) = (grpc_done.0, sse_done.0);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.spawn(async move {
        let mut grpc = DatabaseServiceClient::connect(endpoint).await.unwrap();
        let mut stream = grpc.watch(watch_request(1, 60_000)).await.unwrap().into_inner();
        let end = loop {
            match stream.message().await {
                Ok(Some(_)) => {}
                Ok(None) => break None,
                Err(status) => break Some(status.code()),
            }
        };
        grpc_tx.send(end).unwrap();
    });
    runtime.spawn(async move {
        let client = Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>();
        let response = client.request(Request::get(url).body(Empty::new()).unwrap()).await.unwrap();
        let body = response.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
        sse_tx.send(String::from_utf8_lossy(&body).into_owned()).unwrap();
    });
    // Both streams are open and waiting (60 s rounds)
    std::thread::sleep(Duration::from_millis(500));
    let start = Instant::now();
    let (drain, _db) = server.shutdown(Duration::from_secs(30));
    assert!(start.elapsed() < Duration::from_secs(10), "the streams held up shutdown: {:?}", start.elapsed());
    assert!(drain.complete, "{:?}", drain);
    let grpc_end = grpc_done.1.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(grpc_end, Some(tonic::Code::Unavailable));
    let body = sse_done.1.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(body.contains("event: error") && body.contains("unavailable"), "{}", body);
    assert_eq!(event_ids(&body), [1]);
    runtime.shutdown_timeout(Duration::from_secs(1));
}
