//! What the REST adapter adds on top of the trait (ADR 0030): every route
//! served, HTTP statuses and `Error` bodies, request bodies and their
//! limits, answers as one message or NDJSON, values and filters at the
//! depth limit, single nodes and edges by id, HTTP/1.1 and HTTP/2 on the
//! gRPC port, a client that goes away, and idle connections at shutdown.

#![cfg(feature = "rest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use ironweaver_core::{Expr, Value};
use iwdb::{Embedded, QueryConfig, Store};
use iwdb_engine::Mutation;
use iwdb_query::exec::block_on;
use iwdb_query::{CommitOptions, Database, FindRequest, QueryOptions};
use iwdb_server::rest::{Input, ROUTES};
use iwdb_server::{CHUNK_BYTES, Server};
use serde_json::{Value as Json, json};
use support::{Running, options};

mod support;

const NS: &str = "default";

type Http = Client<HttpConnector, Full<Bytes>>;

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    /// The body, without serde_json's recursion limit: values may nest
    /// 100 levels, 200 JSON levels.
    fn json(&self) -> Json {
        let mut de = serde_json::Deserializer::from_slice(&self.body);
        de.disable_recursion_limit();
        serde::Deserialize::deserialize(&mut de).unwrap_or_else(|e| panic!("{}: {:?}", e, self.body))
    }

    /// The `Error` body's code.
    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_owned()
    }

    fn content_type(&self) -> &str {
        self.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default()
    }
}

fn client(http2: bool) -> Http {
    Client::builder(TokioExecutor::new()).http2_only(http2).build(HttpConnector::new())
}

/// Send a request; a JSON body gets `Content-Type: application/json`.
fn send<D: Database + 'static>(
    server: &Running<D>,
    http: &Http,
    method: Method,
    path: &str,
    body: Option<&Json>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut builder = Request::builder().method(method).uri(format!("{}{}", server.endpoint(), path));
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let bytes = body.map(|b| serde_json::to_vec(b).unwrap()).unwrap_or_default();
    let request = builder.body(Full::new(Bytes::from(bytes))).unwrap();
    server.block_on(async {
        let response = http.request(request).await.unwrap();
        let (parts, body) = response.into_parts();
        Reply { status: parts.status, headers: parts.headers, body: body.collect().await.unwrap().to_bytes() }
    })
}

fn post<D: Database + 'static>(server: &Running<D>, path: &str, body: Json) -> Reply {
    send(server, &client(false), Method::POST, path, Some(&body), &[])
}

fn get<D: Database + 'static>(server: &Running<D>, path: &str) -> Reply {
    send(server, &client(false), Method::GET, path, None, &[])
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

/// A path of `route` with its parameters filled in.
fn fill(path: &str) -> String {
    path.replace("{ns}", NS).replace("{id}", "0")
}

#[test]
fn every_route_is_served_and_other_methods_are_not_allowed() {
    let fresh = support::fresh();
    let server = &fresh.server;
    let http = client(false);
    for route in ROUTES {
        // Create and drop on a namespace of their own
        let path = if route.operation.ends_with("Namespace") && route.method != Method::GET {
            route.path.replace("{ns}", "other")
        } else if route.input == (Input::Changes { stream: true }) {
            // The stream never ends: a request its handler refuses
            format!("{}?wait=true", fill(route.path))
        } else {
            fill(route.path)
        };
        let body = matches!(route.input, Input::Body { .. }).then(|| json!({}));
        let reply = send(server, &http, route.method.clone(), &path, body.as_ref(), &[]);
        // Some answers are errors (an empty request, node 0 that doesn't
        // exist), but none is the router's own
        let text = String::from_utf8_lossy(&reply.body);
        assert!(
            reply.status != StatusCode::METHOD_NOT_ALLOWED
                && !text.contains("no such route")
                && !text.contains("has no handler"),
            "{} {}: {} {:?}",
            route.method,
            path,
            reply.status,
            reply.body
        );
        assert!(reply.content_type().starts_with("application/json"), "{} {}", route.method, path);
        if route.operation == "createNamespace" {
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        }
        for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            if ROUTES.iter().any(|r| r.path == route.path && r.method == method) {
                continue;
            }
            let reply = send(server, &http, method.clone(), &path, None, &[]);
            assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{} {}", method, path);
            assert_eq!(reply.code(), "invalid_argument");
        }
    }
    let reply = get(server, "/v1/nowhere");
    assert_eq!((reply.status, reply.code().as_str()), (StatusCode::NOT_FOUND, "invalid_argument"));
}

#[test]
fn errors_have_the_status_of_their_code_and_an_error_body() {
    let fresh = support::fresh();
    let server = &fresh.server;
    let cases: Vec<(Reply, StatusCode, &str, &str)> = vec![
        (get(server, "/v1/namespaces/nope"), StatusCode::NOT_FOUND, "not_found", "nope"),
        (
            send(server, &client(false), Method::PUT, "/v1/namespaces/default", None, &[]),
            StatusCode::CONFLICT,
            "conflict",
            "default",
        ),
        (
            post(server, "/v1/namespaces/default/find", json!({ "filter": { "Labl": "N" } })),
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "unknown variant `Labl`",
        ),
        (
            post(server, "/v1/namespaces/default/find", json!({ "filtr": { "Label": "N" } })),
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "unknown field `filtr`",
        ),
        (
            post(server, "/v1/namespaces/default/find", json!({ "namespace": "other", "filter": { "Label": "N" } })),
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "namespace 'other'",
        ),
        (
            post(server, "/v1/namespaces/default/find", json!({})),
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "the filter is missing",
        ),
        (
            get(server, "/v1/namespaces/default/catalog?max_results=0"),
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "max_results",
        ),
        (
            get(server, "/v1/namespaces/default/catalog?colour=red"),
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "unknown field `colour`",
        ),
        (
            get(server, "/v1/namespaces/default/edges/x"),
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "invalid edge id 'x'",
        ),
    ];
    for (reply, status, code, message) in cases {
        let body = reply.json();
        assert_eq!((reply.status, body["code"].as_str()), (status, Some(code)), "{}", body);
        assert!(body["message"].as_str().unwrap().contains(message), "{}: {}", message, body);
    }
    // A body that isn't JSON, or isn't said to be
    let reply = send(server, &client(false), Method::POST, "/v1/namespaces/default/find", None, &[]).status;
    assert_eq!(reply, StatusCode::BAD_REQUEST, "an empty body is the empty message");
    let raw = |content_type: &str, body: &'static str| {
        let request = Request::post(format!("{}/v1/namespaces/default/find", server.endpoint()))
            .header(header::CONTENT_TYPE, content_type)
            .body(Full::new(Bytes::from_static(body.as_bytes())))
            .unwrap();
        server.block_on(async {
            let response = client(false).request(request).await.unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Json>(&body).unwrap())
        })
    };
    let (status, body) = raw("text/plain", r#"{"filter":{"Label":"N"}}"#);
    assert_eq!((status, body["code"].as_str()), (StatusCode::UNSUPPORTED_MEDIA_TYPE, Some("invalid_argument")));
    let (status, _) = raw("application/json; charset=utf-8", r#"{"filter":{"Label":"N"}}"#);
    assert_eq!(status, StatusCode::OK);
    let (status, body) = raw("application/json", r#"{"filter":"#);
    assert_eq!((status, body["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_argument")));
    // A budget, a timeout
    let remote = fresh.server.client();
    let nodes: Vec<Mutation> = (0..5).map(|i| node(&format!("n{}", i), vec![])).collect();
    block_on(remote.commit(NS, nodes, CommitOptions::default())).unwrap();
    let reply = post(
        server,
        "/v1/namespaces/default/neighbourhood",
        json!({ "seeds": ["n0", "n1", "n2"], "options": { "limits": { "maxVisited": "1" } } }),
    );
    assert_eq!((reply.status, reply.code().as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "budget_exceeded"));
    let reply = get(server, "/v1/namespaces/default/nodes/n0?min_seq=99&timeout_ms=50");
    assert_eq!((reply.status, reply.code().as_str()), (StatusCode::GATEWAY_TIMEOUT, "timeout"));
}

#[test]
fn request_bodies_above_the_message_limit_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(store(dir.path(), 2));
    let router = Server::new(db).max_message_bytes(4096).rest_router();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let send = |body: String| {
        let request = Request::post("/v1/namespaces/default/commit")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        runtime.block_on(async {
            let response = tower_service::Service::call(&mut router.clone(), request).await.unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Json>(&body).unwrap())
        })
    };
    let commit = |text: &str| {
        json!({ "mutations": [{ "upsertNode": { "id": "a", "attr": { "t": { "String": text } } } }] }).to_string()
    };
    let (status, _) = send(commit(&"x".repeat(3000)));
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(commit(&"x".repeat(5000)));
    assert_eq!((status, body["code"].as_str()), (StatusCode::PAYLOAD_TOO_LARGE, Some("invalid_argument")));
    assert!(body["message"].as_str().unwrap().contains("4096"), "{}", body);
}

#[test]
fn large_answers_come_as_one_message_or_as_ndjson_chunks_with_the_meta_last() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(store(dir.path(), 2));
    let remote = server.client();
    let padding = Value::String("x".repeat(2000));
    let nodes: Vec<Mutation> = (0..2000).map(|i| node(&format!("n{:04}", i), vec![("p", padding.clone())])).collect();
    block_on(remote.commit(NS, nodes, CommitOptions::default())).unwrap();
    let request = json!({ "filter": { "Label": "N" }, "options": { "limits": { "maxResults": 5000 } } });
    let one = post(&server, "/v1/namespaces/default/find", request.clone());
    assert_eq!((one.status, one.content_type()), (StatusCode::OK, "application/json"));
    let one = one.json();
    assert_eq!(one["nodes"].as_array().unwrap().len(), 2000);
    let ndjson = send(
        &server,
        &client(false),
        Method::POST,
        "/v1/namespaces/default/find",
        Some(&request),
        &[("accept", "application/x-ndjson")],
    );
    assert_eq!((ndjson.status, ndjson.content_type()), (StatusCode::OK, "application/x-ndjson"));
    let text = String::from_utf8(ndjson.body.to_vec()).unwrap();
    assert!(text.ends_with('\n'));
    let lines: Vec<Json> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.len() >= 4, "{} lines", lines.len());
    let mut nodes = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(line.get("meta").is_some(), i == lines.len() - 1, "line {}", i);
        // A chunk's JSON is bigger than its protobuf, but not by much here
        assert!(line.to_string().len() <= 2 * CHUNK_BYTES);
        nodes.extend(line["nodes"].as_array().unwrap().iter().cloned());
    }
    // The chunks are the one message, cut
    assert_eq!(Json::Array(nodes), one["nodes"]);
    assert_eq!(lines.last().unwrap()["meta"], one["meta"]);
    // An empty answer is one line with the meta
    let empty = send(
        &server,
        &client(false),
        Method::POST,
        "/v1/namespaces/default/find",
        Some(&json!({ "filter": { "Label": "M" } })),
        &[("accept", "application/x-ndjson")],
    );
    let text = String::from_utf8(empty.body.to_vec()).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("\"meta\"") && !text.contains("\"nodes\""), "{}", text);
}

fn nest(depth: usize) -> Value {
    (1..depth).fold(Value::Int(1), |v, _| Value::List(vec![v]))
}

#[test]
fn values_and_filters_nested_100_levels_pass_and_101_fail_with_the_cores_message() {
    let fresh = support::fresh_rest(false);
    let server = &fresh.server;
    let deep = serde_json::to_value(nest(100)).unwrap();
    let commit = |value: &Json| json!({ "mutations": [{ "upsertNode": { "id": "a", "labels": ["N"], "attr": { "deep": value } } }] });
    let reply = post(server, "/v1/namespaces/default/commit", commit(&deep));
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    // Written back in the same form, and the same value
    let reply = get(server, "/v1/namespaces/default/nodes/a");
    assert_eq!(reply.json()["nodes"][0]["node"]["attr"]["deep"], deep);
    let read = block_on(fresh.get_nodes(NS, vec!["a".into()], QueryOptions::default())).unwrap();
    assert_eq!(read.value[0].as_ref().unwrap().attr["deep"], nest(100));
    let reply = post(server, "/v1/namespaces/default/commit", commit(&json!({ "List": [deep] })));
    assert_eq!((reply.status, reply.code().as_str()), (StatusCode::BAD_REQUEST, "invalid_argument"));
    assert!(reply.json()["message"].as_str().unwrap().contains("nested more than 100 levels"), "{:?}", reply.body);

    let filter = (1..100).fold(Expr::Label("N".into()), |e, _| Expr::And(vec![e]));
    let filter = serde_json::to_value(&filter).unwrap();
    let reply = post(server, "/v1/namespaces/default/find", json!({ "filter": filter }));
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.json()["nodes"][0]["id"], "a");
    let reply = post(server, "/v1/namespaces/default/find", json!({ "filter": { "Not": filter } }));
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.json()["message"].as_str().unwrap().contains("expression nested more than 100 levels"));
}

#[test]
fn single_nodes_and_edges_by_id() {
    let fresh = support::fresh();
    let server = &fresh.server;
    let odd = "a/b ü?#";
    let edge = Mutation::AddEdge {
        from: odd.into(),
        to: "c".into(),
        ty: Some("t".into()),
        attr: Default::default(),
        meta: Default::default(),
    };
    let result =
        block_on(fresh.commit(NS, vec![node(odd, vec![]), node("c", vec![]), edge], CommitOptions::default())).unwrap();
    let encoded = "a%2Fb%20%C3%BC%3F%23";
    let reply = get(server, &format!("/v1/namespaces/default/nodes/{}?min_seq={}", encoded, result.seq));
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let body = reply.json();
    assert_eq!(body["nodes"][0]["node"]["id"], odd);
    assert_eq!(body["meta"]["seq"], result.seq.to_string());
    let id = result.edge_ids[0].0;
    let reply = get(server, &format!("/v1/namespaces/default/edges/{}", id));
    assert_eq!(reply.json()["edges"][0]["edge"]["from"], odd);
    let reply = get(server, "/v1/namespaces/default/nodes/nobody");
    assert_eq!((reply.status, reply.code().as_str()), (StatusCode::NOT_FOUND, "not_found"));
    let reply = get(server, &format!("/v1/namespaces/default/edges/{}", id + 1));
    assert_eq!((reply.status, reply.code().as_str()), (StatusCode::NOT_FOUND, "not_found"));
    // Several by id: missing ones are empty entries
    let reply = post(server, "/v1/namespaces/default/get-nodes", json!({ "ids": ["c", "nobody"] }));
    assert_eq!(reply.json()["nodes"], json!([{ "node": { "id": "c", "labels": ["N"], "version": "1" } }, {}]));
}

#[test]
fn http2_and_http1_serve_rest_and_grpc_on_one_port() {
    let fresh = support::fresh();
    let server = &fresh.server;
    for http2 in [false, true] {
        let reply = send(server, &client(http2), Method::GET, "/v1/namespaces", None, &[]);
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["namespaces"][0]["name"], NS);
    }
    // gRPC next to it
    assert_eq!(block_on(fresh.namespaces()).unwrap().len(), 1);
    // The document is served
    let reply = get(server, "/v1/openapi.json");
    assert_eq!(reply.json()["openapi"], "3.1.0");
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
fn a_rest_client_that_goes_away_stops_its_read() {
    let dir = tempfile::tempdir().unwrap();
    // One worker: while the long read runs, nothing else can
    let server = Running::start(store(dir.path(), 1));
    let remote = server.rest_client();
    block_on(remote.commit(NS, complete(30), CommitOptions::default())).unwrap();
    let long = QueryOptions { timeout: Some(Duration::from_secs(120)), ..QueryOptions::default() }.with_limits(
        Some(usize::MAX),
        Some(usize::MAX),
        Some(usize::MAX),
    );
    let request = iwdb_query::MatchRequest::parse("(a)-[*1..10]->(b)").unwrap();
    let call = remote.match_pattern(NS, request, long);
    thread::sleep(Duration::from_millis(300));
    let busy = QueryOptions { timeout: Some(Duration::from_millis(200)), ..QueryOptions::default() };
    let e = block_on(remote.get_nodes(NS, vec!["n0".into()], busy)).unwrap_err();
    assert_eq!(e.code(), iwdb_query::Code::Timeout, "the long read holds the worker: {}", e);
    // Dropping the call closes its HTTP/1.1 connection: the server notices,
    // drops the handler and cancels the read
    drop(call);
    let start = Instant::now();
    let quick = QueryOptions { timeout: Some(Duration::from_secs(20)), ..QueryOptions::default() };
    let read = block_on(remote.get_nodes(NS, vec!["n0".into()], quick)).unwrap();
    assert!(read.value[0].is_some());
    assert!(start.elapsed() < Duration::from_secs(5), "the worker was free after {:?}", start.elapsed());
}

#[test]
fn idle_rest_connections_dont_hold_up_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(store(dir.path(), 2));
    // A keep-alive HTTP/1.1 connection, idle in the client's pool
    let http = client(false);
    assert_eq!(send(&server, &http, Method::GET, "/v1/namespaces", None, &[]).status, StatusCode::OK);
    let start = Instant::now();
    let (report, db) = server.shutdown(Duration::from_secs(30));
    assert!(report.complete && report.cancelled == 0, "{:?}", report);
    assert!(start.elapsed() < Duration::from_secs(5), "shutdown took {:?}", start.elapsed());
    db.close().unwrap();
    drop(http);
}

#[test]
fn the_rest_client_reports_a_missing_server_as_unavailable() {
    let gone = iwdb_server::client::RestRemote::connect("http://127.0.0.1:9").unwrap();
    let e = block_on(gone.find(NS, FindRequest { filter: Expr::Const(true) }, QueryOptions::default())).unwrap_err();
    assert_eq!(e.code(), iwdb_query::Code::Unavailable, "{}", e);
    assert!(iwdb_server::client::RestRemote::connect("grpc://x").is_err());
}
