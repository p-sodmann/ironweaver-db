//! The REST/JSON API (ADR 0030): the operations of `proto/ironweaver_db/v1`
//! as resource-style HTTP routes ([`ROUTES`]), with the proto messages in
//! their proto3 JSON form (pbjson) as bodies, so REST and gRPC share one
//! schema. Each handler reads its request message, runs its operation in
//! `crate::ops` (one trait call, design rule 8), and writes the answer.
//!
//! - **Requests.** A route's body is its RPC's request message; the
//!   namespace comes from the path (a body that names another one is
//!   `invalid_argument`). An empty body is the empty message. A non-empty
//!   body must be `Content-Type: application/json` (otherwise 415): a
//!   browser can't send that cross-site without a CORS preflight, which the
//!   server doesn't answer. Unknown fields are refused. GET reads take their
//!   `QueryOptions` as query parameters.
//! - **Answers.** Unary operations answer with their response message.
//!   Streamed operations answer with their chunks merged into one message,
//!   or with `Accept: application/x-ndjson` as NDJSON: one chunk per line,
//!   `meta` in the last (ADR 0025).
//! - **Errors.** The HTTP status of the error's code
//!   ([`crate::status::http_status`]) and an `Error` message as the body.
//!   Requests that match no route, a wrong method or media type, or a body
//!   over the size limit are `invalid_argument` with 404, 405, 415 or 413.
//! - **Deadlines.** `QueryOptions.timeout_ms`; HTTP has no deadline header.
//!
//! Values, filters and patterns are the core's JSON form (the private
//! `json` module).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, post, put};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use iwdb_query::{Code, Database, Error};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::ops;
use crate::proto as pb;
use crate::status::http_status;

mod json;
pub mod openapi;

/// The media type of JSON bodies.
pub const JSON: &str = "application/json";
/// The media type of streamed answers: one JSON message per line.
pub const NDJSON: &str = "application/x-ndjson";
/// The media type of the change stream's Server-Sent Events.
pub const EVENT_STREAM: &str = "text/event-stream";

/// What a route takes besides its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The RPC's request message as a JSON body; `required: false` for
    /// routes whose message is all optional (an empty body is fine).
    Body { required: bool },
    /// `QueryOptions` as query parameters.
    Options,
    /// The change stream's query parameters (`from_seq`, `wait`, options); with
    /// `stream`, as Server-Sent Events (and without `wait`).
    Changes { stream: bool },
    /// Nothing.
    Nothing,
}

/// A REST route: the table the router is built from and the OpenAPI
/// document describes.
#[derive(Clone, Debug)]
pub struct Route {
    pub method: Method,
    /// With `{ns}` (a namespace) and `{id}` (a node id, or an edge id for
    /// `GetEdges`) parameters.
    pub path: &'static str,
    /// The operation's RPC in `DatabaseService`: its request and response
    /// messages are the route's; `None` for the OpenAPI document.
    pub rpc: Option<&'static str>,
    pub input: Input,
    /// The OpenAPI `operationId`.
    pub operation: &'static str,
    /// One line for the OpenAPI document; the RPC's comment (or its request
    /// message's) is the longer description.
    pub summary: &'static str,
}

impl Route {
    /// A health route: served by the server's gate, not the router.
    pub fn health(&self) -> bool {
        matches!(self.operation, "live" | "ready")
    }
}

const fn route(
    method: Method,
    path: &'static str,
    rpc: &'static str,
    input: Input,
    operation: &'static str,
    summary: &'static str,
) -> Route {
    Route { method, path, rpc: Some(rpc), input, operation, summary }
}

const BODY: Input = Input::Body { required: true };
const OPTIONAL_BODY: Input = Input::Body { required: false };

/// Every REST route.
pub const ROUTES: &[Route] = &[
    route(Method::GET, "/v1/namespaces", "ListNamespaces", Input::Nothing, "listNamespaces", "List the namespaces"),
    route(
        Method::PUT,
        "/v1/namespaces/{ns}",
        "CreateNamespace",
        OPTIONAL_BODY,
        "createNamespace",
        "Create a namespace",
    ),
    route(Method::DELETE, "/v1/namespaces/{ns}", "DropNamespace", OPTIONAL_BODY, "dropNamespace", "Drop a namespace"),
    route(
        Method::GET,
        "/v1/namespaces/{ns}",
        "GetNamespaceStatus",
        Input::Nothing,
        "getNamespaceStatus",
        "A namespace's status",
    ),
    route(
        Method::GET,
        "/v1/namespaces/{ns}/catalog",
        "GetCatalog",
        Input::Options,
        "getCatalog",
        "A namespace's catalog",
    ),
    route(
        Method::POST,
        "/v1/namespaces/{ns}/catalog",
        "CommitCatalog",
        BODY,
        "commitCatalog",
        "Commit a catalog change",
    ),
    route(Method::POST, "/v1/namespaces/{ns}/commit", "Commit", BODY, "commit", "Commit a transaction"),
    route(Method::POST, "/v1/namespaces/{ns}/wait", "WaitForSeq", BODY, "waitForSeq", "Wait until a seq is applied"),
    route(
        Method::GET,
        "/v1/namespaces/{ns}/nodes/{id}",
        "GetNodes",
        Input::Options,
        "getNode",
        "One node (404 if missing)",
    ),
    route(Method::POST, "/v1/namespaces/{ns}/get-nodes", "GetNodes", BODY, "getNodes", "Nodes by id"),
    route(
        Method::GET,
        "/v1/namespaces/{ns}/edges/{id}",
        "GetEdges",
        Input::Options,
        "getEdge",
        "One edge (404 if missing)",
    ),
    route(Method::POST, "/v1/namespaces/{ns}/get-edges", "GetEdges", BODY, "getEdges", "Edges by id"),
    route(Method::POST, "/v1/namespaces/{ns}/find", "Find", BODY, "find", "Nodes matching a filter"),
    route(Method::POST, "/v1/namespaces/{ns}/explain", "Explain", BODY, "explain", "How find would read a filter"),
    route(
        Method::POST,
        "/v1/namespaces/{ns}/neighbourhood",
        "Neighbourhood",
        BODY,
        "neighbourhood",
        "Nodes near seeds",
    ),
    route(Method::POST, "/v1/namespaces/{ns}/traverse", "Traverse", BODY, "traverse", "Breadth or depth first order"),
    route(Method::POST, "/v1/namespaces/{ns}/shortest-path", "ShortestPath", BODY, "shortestPath", "A shortest path"),
    route(Method::POST, "/v1/namespaces/{ns}/random-walks", "RandomWalks", BODY, "randomWalks", "Random walks"),
    route(Method::POST, "/v1/namespaces/{ns}/subgraph", "Subgraph", BODY, "subgraph", "An induced subgraph"),
    route(Method::POST, "/v1/namespaces/{ns}/match", "MatchPattern", BODY, "matchPattern", "Match a pattern"),
    route(Method::POST, "/v1/namespaces/{ns}/analyze", "Analyze", BODY, "analyze", "Run an analytics job"),
    route(
        Method::GET,
        "/v1/namespaces/{ns}/changes",
        "GetChanges",
        Input::Changes { stream: false },
        "getChanges",
        "A batch of the change stream",
    ),
    route(
        Method::GET,
        "/v1/namespaces/{ns}/changes/stream",
        "Watch",
        Input::Changes { stream: true },
        "watchChanges",
        "Follow the change stream (Server-Sent Events)",
    ),
    // Served before the router, in every build and during recovery
    // (crate::health); here for the OpenAPI document
    Route {
        method: Method::GET,
        path: crate::health::LIVE_PATH,
        rpc: None,
        input: Input::Nothing,
        operation: "live",
        summary: "Liveness: 200 whenever the server answers",
    },
    Route {
        method: Method::GET,
        path: crate::health::READY_PATH,
        rpc: None,
        input: Input::Nothing,
        operation: "ready",
        summary: "Readiness: 200 once recovery has finished, 503 before and while shutting down",
    },
    Route {
        method: Method::GET,
        path: "/v1/openapi.json",
        rpc: None,
        input: Input::Nothing,
        operation: "openapi",
        summary: "This API's OpenAPI document",
    },
];

/// What the handlers share.
struct Shared<D> {
    db: Arc<D>,
    max_body: usize,
    /// Turns true when the server shuts down: change streams end.
    stopping: watch::Receiver<bool>,
}

type St<D> = State<Arc<Shared<D>>>;

/// The REST routes over `db`, with request bodies of at most `max_body`
/// bytes. Requests that match no route are answered here too (404). The
/// change streams end when `stopping` turns true.
pub fn router<D: Database + 'static>(db: Arc<D>, max_body: usize, stopping: watch::Receiver<bool>) -> Router {
    let mut router = Router::new();
    for r in ROUTES.iter().filter(|r| !r.health()) {
        router = router.route(r.path, handler::<D>(r));
    }
    router
        .fallback(|| async { Failure::http(StatusCode::NOT_FOUND, "no such route; see /v1/openapi.json") })
        .method_not_allowed_fallback(|| async {
            Failure::http(StatusCode::METHOD_NOT_ALLOWED, "the route doesn't take this method")
        })
        .with_state(Arc::new(Shared { db, max_body, stopping }))
}

/// The handler of `r`.
fn handler<D: Database + 'static>(r: &Route) -> MethodRouter<Arc<Shared<D>>> {
    match r.operation {
        "listNamespaces" => get(list_namespaces::<D>),
        "createNamespace" => put(create_namespace::<D>),
        "dropNamespace" => delete(drop_namespace::<D>),
        "getNamespaceStatus" => get(namespace_status::<D>),
        "getCatalog" => get(catalog::<D>),
        "commitCatalog" => post(commit_catalog::<D>),
        "commit" => post(commit::<D>),
        "waitForSeq" => post(wait_for_seq::<D>),
        "getNode" => get(get_node::<D>),
        "getNodes" => post(get_nodes::<D>),
        "getEdge" => get(get_edge::<D>),
        "getEdges" => post(get_edges::<D>),
        "find" => post(find::<D>),
        "explain" => post(explain::<D>),
        "neighbourhood" => post(neighbourhood::<D>),
        "traverse" => post(traverse::<D>),
        "shortestPath" => post(shortest_path::<D>),
        "randomWalks" => post(random_walks::<D>),
        "subgraph" => post(subgraph::<D>),
        "matchPattern" => post(match_pattern::<D>),
        "analyze" => post(analyze::<D>),
        "getChanges" => get(get_changes::<D>),
        "watchChanges" => get(watch_changes::<D>),
        "openapi" => get(openapi_document),
        other => get(move || async move { Failure::from(Error::internal(format!("route {} has no handler", other))) }),
    }
}

// ---- errors ----

/// A failed request: the error, and the HTTP status if it isn't the code's
/// (HTTP-layer errors: 404, 405, 413, 415).
#[derive(Debug)]
pub(crate) struct Failure(Error, Option<StatusCode>);

impl Failure {
    fn http(status: StatusCode, message: impl Into<String>) -> Failure {
        Failure(Error::new(Code::InvalidArgument, message), Some(status))
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure(e, None)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let Failure(e, status) = self;
        crate::status::log_server_error(&e);
        let body = pb::Error { code: e.code().as_str().to_owned(), message: e.message().to_owned() };
        // Two strings: writing them can't fail
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        (status.unwrap_or_else(|| http_status(e.code())), [(header::CONTENT_TYPE, JSON)], bytes).into_response()
    }
}

type Answer = Result<Response, Failure>;

// ---- requests ----

/// The request message of a body (an empty body is the empty message).
async fn read<T: DeserializeOwned + Default>(headers: &HeaderMap, body: Body, limit: usize) -> Result<T, Failure> {
    let bytes = axum::body::to_bytes(body, limit).await.map_err(|e| {
        let too_large = std::error::Error::source(&e).is_some_and(|s| s.is::<http_body_util::LengthLimitError>());
        if too_large {
            Failure::http(StatusCode::PAYLOAD_TOO_LARGE, format!("the request body is larger than {} bytes", limit))
        } else {
            Failure::from(Error::invalid(format!("can't read the request body: {}", e)))
        }
    })?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case(JSON));
    if !json {
        return Err(Failure::http(StatusCode::UNSUPPORTED_MEDIA_TYPE, "a request body must be application/json"));
    }
    serde_json::from_slice(&bytes).map_err(|e| Failure::from(Error::invalid(format!("invalid request body: {}", e))))
}

/// The namespace of a request: the path's.
fn namespace(field: &mut String, path: String) -> Result<(), Failure> {
    if !field.is_empty() && *field != path {
        return Err(Error::invalid(format!("the body names namespace '{}', the path '{}'", field, path)).into());
    }
    *field = path;
    Ok(())
}

fn path<T>(p: Result<Path<T>, PathRejection>) -> Result<T, Failure> {
    p.map(|Path(t)| t).map_err(|e| Error::invalid(format!("invalid path: {}", e)).into())
}

/// `QueryOptions` as query parameters (GET reads).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionsQuery {
    min_seq: Option<u64>,
    history: Option<String>,
    timeout_ms: Option<u32>,
    max_results: Option<u64>,
    max_visited: Option<u64>,
    max_edges: Option<u64>,
    partial: Option<bool>,
    cursor: Option<String>,
}

/// The query parameters of [`Input::Options`] routes, for the OpenAPI
/// document: name, type, description.
pub(crate) const OPTION_PARAMETERS: &[(&str, &str, &str)] = &[
    ("min_seq", "integer", "Read-your-writes: wait until the namespace has applied this seq."),
    ("history", "string", "The history `min_seq` belongs to (32 hex digits)."),
    ("timeout_ms", "integer", "How long the read may take, in milliseconds."),
    ("max_results", "integer", "Most results in the answer."),
    ("max_visited", "integer", "Most nodes the read may visit."),
    ("max_edges", "integer", "Most edges the read may examine."),
    ("partial", "boolean", "Answer with what was found when a limit is reached."),
    ("cursor", "string", "Continue a paginated read: the `next` of the previous page."),
];

fn options(q: Result<Query<OptionsQuery>, QueryRejection>) -> Result<Option<pb::QueryOptions>, Failure> {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    let limits = pb::Limits { max_results: q.max_results, max_visited: q.max_visited, max_edges: q.max_edges };
    Ok(Some(pb::QueryOptions {
        min_seq: q.min_seq,
        history: q.history.unwrap_or_default(),
        timeout_ms: q.timeout_ms,
        limits: Some(limits),
        partial: q.partial.unwrap_or_default(),
        cursor: q.cursor.unwrap_or_default(),
    }))
}

/// The query parameters of the change stream's routes.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangesQuery {
    from_seq: Option<u64>,
    wait: Option<bool>,
    min_seq: Option<u64>,
    history: Option<String>,
    timeout_ms: Option<u32>,
    max_results: Option<u64>,
}

/// The query parameters of [`Input::Changes`] routes, for the OpenAPI
/// document: name, type, description (`wait` only without `stream`).
pub(crate) const CHANGES_PARAMETERS: &[(&str, &str, &str)] = &[
    ("from_seq", "integer", "The first seq to return; resume with the `nextSeq` of the last batch."),
    ("wait", "boolean", "If there is no commit yet, wait for one for about the timeout (a long poll)."),
    ("min_seq", "integer", "Wait until the namespace has applied this seq first."),
    ("history", "string", "The history `from_seq` belongs to (32 hex digits); another one is refused."),
    ("timeout_ms", "integer", "How long the request may take (for a stream: each round), in milliseconds."),
    ("max_results", "integer", "Most commits in a batch."),
];

/// `from_seq`, `wait` and the options of a change stream route.
fn changes_query(
    q: Result<Query<ChangesQuery>, QueryRejection>,
    stream: bool,
) -> Result<(u64, bool, Option<pb::QueryOptions>), Failure> {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    if stream && q.wait.is_some() {
        return Err(Error::invalid("the change stream always waits: leave `wait` out").into());
    }
    let options = pb::QueryOptions {
        min_seq: q.min_seq,
        history: q.history.unwrap_or_default(),
        timeout_ms: q.timeout_ms,
        limits: Some(pb::Limits { max_results: q.max_results, ..pb::Limits::default() }),
        ..pb::QueryOptions::default()
    };
    Ok((q.from_seq.unwrap_or_default(), q.wait.unwrap_or_default(), Some(options)))
}

// ---- answers ----

fn json_response(bytes: Vec<u8>, media: &'static str) -> Response {
    ([(header::CONTENT_TYPE, HeaderValue::from_static(media))], bytes).into_response()
}

fn to_json<T: serde::Serialize>(message: &T) -> Result<Vec<u8>, Failure> {
    serde_json::to_vec(message).map_err(|e| Error::internal(format!("can't write the answer: {}", e)).into())
}

fn unary<T: serde::Serialize>(message: Result<T, Error>) -> Answer {
    Ok(json_response(to_json(&message?)?, JSON))
}

fn wants_ndjson(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.split(',').any(|t| t.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case(NDJSON))))
}

/// A streamed answer: NDJSON if asked for, otherwise its chunks merged into
/// one message (protobuf merge: repeated fields concatenate, `meta` is the
/// last chunk's).
fn streamed<T>(headers: &HeaderMap, chunks: Result<Vec<T>, Error>) -> Answer
where
    T: prost::Message + serde::Serialize + Default,
{
    let mut chunks = chunks?;
    if wants_ndjson(headers) {
        let mut lines = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let mut line = to_json(chunk)?;
            line.push(b'\n');
            lines.push(Ok::<_, std::convert::Infallible>(Bytes::from(line)));
        }
        let body = Body::from_stream(tokio_stream::iter(lines));
        return Ok(([(header::CONTENT_TYPE, HeaderValue::from_static(NDJSON))], body).into_response());
    }
    let one = if chunks.len() == 1 {
        chunks.pop().unwrap_or_default()
    } else {
        let mut all = T::default();
        for chunk in chunks {
            all.merge(chunk.encode_to_vec().as_slice())
                .map_err(|e| Error::internal(format!("can't merge the answer: {}", e)))?;
        }
        all
    };
    unary(Ok(one))
}

// ---- handlers ----

async fn list_namespaces<D: Database + 'static>(State(s): St<D>) -> Answer {
    unary(ops::list_namespaces(&*s.db).await)
}

async fn create_namespace<D: Database + 'static>(
    State(s): St<D>,
    p: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let mut r: pb::CreateNamespaceRequest = read(&headers, body, s.max_body).await?;
    namespace(&mut r.name, path(p)?)?;
    unary(ops::create_namespace(&*s.db, r).await)
}

async fn drop_namespace<D: Database + 'static>(
    State(s): St<D>,
    p: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let mut r: pb::DropNamespaceRequest = read(&headers, body, s.max_body).await?;
    namespace(&mut r.name, path(p)?)?;
    unary(ops::drop_namespace(&*s.db, r).await)
}

async fn namespace_status<D: Database + 'static>(State(s): St<D>, p: Result<Path<String>, PathRejection>) -> Answer {
    let r = pb::GetNamespaceStatusRequest { namespace: path(p)? };
    unary(ops::get_namespace_status(&*s.db, r).await)
}

async fn catalog<D: Database + 'static>(
    State(s): St<D>,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let r = pb::GetCatalogRequest { namespace: path(p)?, options: options(q)? };
    unary(ops::get_catalog(&*s.db, r, None).await)
}

/// A handler that reads its body, takes the namespace from the path, and
/// runs its operation.
macro_rules! body_handler {
    ($name:ident, $request:ty, |$s:ident, $r:ident, $headers:ident| $run:expr) => {
        async fn $name<D: Database + 'static>(
            State($s): St<D>,
            p: Result<Path<String>, PathRejection>,
            $headers: HeaderMap,
            body: Body,
        ) -> Answer {
            let mut $r: $request = read(&$headers, body, $s.max_body).await?;
            namespace(&mut $r.namespace, path(p)?)?;
            $run
        }
    };
}

body_handler!(commit_catalog, pb::CommitCatalogRequest, |s, r, _h| unary(ops::commit_catalog(&*s.db, r).await));
body_handler!(commit, pb::CommitRequest, |s, r, _h| unary(ops::commit(&*s.db, r).await));
body_handler!(wait_for_seq, pb::WaitForSeqRequest, |s, r, _h| unary(ops::wait_for_seq(&*s.db, r, None).await));
body_handler!(get_nodes, pb::GetNodesRequest, |s, r, h| streamed(&h, ops::get_nodes(&*s.db, r, None).await));
body_handler!(get_edges, pb::GetEdgesRequest, |s, r, h| streamed(&h, ops::get_edges(&*s.db, r, None).await));
body_handler!(find, pb::FindRequest, |s, r, h| streamed(&h, ops::find(&*s.db, r, None).await));
body_handler!(explain, pb::ExplainRequest, |s, r, _h| unary(ops::explain(&*s.db, r, None).await));
body_handler!(neighbourhood, pb::NeighbourhoodRequest, |s, r, h| streamed(
    &h,
    ops::neighbourhood(&*s.db, r, None).await
));
body_handler!(traverse, pb::TraverseRequest, |s, r, h| streamed(&h, ops::traverse(&*s.db, r, None).await));
body_handler!(shortest_path, pb::ShortestPathRequest, |s, r, _h| unary(ops::shortest_path(&*s.db, r, None).await));
body_handler!(random_walks, pb::RandomWalksRequest, |s, r, h| streamed(&h, ops::random_walks(&*s.db, r, None).await));
body_handler!(subgraph, pb::SubgraphRequest, |s, r, h| streamed(&h, ops::subgraph(&*s.db, r, None).await));
body_handler!(match_pattern, pb::MatchPatternRequest, |s, r, h| streamed(
    &h,
    ops::match_pattern(&*s.db, r, None).await
));
body_handler!(analyze, pb::AnalyzeRequest, |s, r, h| streamed(&h, ops::analyze(&*s.db, r, None).await));

/// One node by id: its `GetNodesResponse`, or 404 if it doesn't exist.
async fn get_node<D: Database + 'static>(
    State(s): St<D>,
    p: Result<Path<(String, String)>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let (ns, id) = path(p)?;
    let r = pb::GetNodesRequest { namespace: ns.clone(), ids: vec![id.clone()], options: options(q)? };
    let answer = merged(ops::get_nodes(&*s.db, r, None).await?)?;
    if answer.nodes.iter().all(|n| n.node.is_none()) {
        return Err(Error::new(Code::NotFound, format!("no node '{}' in namespace '{}'", id, ns)).into());
    }
    unary(Ok(answer))
}

/// One edge by id: its `GetEdgesResponse`, or 404 if it doesn't exist.
async fn get_edge<D: Database + 'static>(
    State(s): St<D>,
    p: Result<Path<(String, String)>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let (ns, id) = path(p)?;
    let edge: u64 = id.parse().map_err(|_| Error::invalid(format!("invalid edge id '{}'", id)))?;
    let r = pb::GetEdgesRequest { namespace: ns.clone(), ids: vec![edge], options: options(q)? };
    let answer = merged(ops::get_edges(&*s.db, r, None).await?)?;
    if answer.edges.iter().all(|e| e.edge.is_none()) {
        return Err(Error::new(Code::NotFound, format!("no edge {} in namespace '{}'", edge, ns)).into());
    }
    unary(Ok(answer))
}

async fn get_changes<D: Database + 'static>(
    State(s): St<D>,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<ChangesQuery>, QueryRejection>,
) -> Answer {
    let (from_seq, wait, options) = changes_query(q, false)?;
    let r = pb::GetChangesRequest { namespace: path(p)?, from_seq, wait, options };
    unary(ops::get_changes(&*s.db, r, None).await)
}

/// The change stream as Server-Sent Events (ADR 0031): a `change` event per
/// commit, with the seq as its `id` and the `ChangeEvent` as its data; a
/// comment line as heartbeat after a round without commits; an `error`
/// event with an `Error` before the stream ends on an error (`unavailable`
/// at shutdown). A `Last-Event-ID` header (what `EventSource` sends when it
/// reconnects) resumes after that seq, instead of `from_seq`. An error in
/// the first batch (no namespace, `not_retained`, ...) is the answer's
/// status instead.
async fn watch_changes<D: Database + 'static>(
    State(s): St<D>,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<ChangesQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Answer {
    let (mut from_seq, _, options) = changes_query(q, true)?;
    if let Some(id) = headers.get("last-event-id") {
        let seq = id.to_str().ok().and_then(|id| id.trim().parse::<u64>().ok());
        let seq = seq.ok_or_else(|| Error::invalid("Last-Event-ID must be the seq of an event"))?;
        from_seq = seq.saturating_add(1);
    }
    let r = pb::WatchRequest { namespace: path(p)?, from_seq, options };
    let mut batches = ops::follow(s.db.clone(), r, None, s.stopping.clone());
    let first = match batches.recv().await {
        Some(first) => first?,
        None => return Err(Error::unavailable("the server is shutting down").into()),
    };
    let events = tokio_stream::once(Ok(first)).chain(ReceiverStream::new(batches)).map(|batch| {
        Ok::<_, std::convert::Infallible>(Bytes::from(match batch {
            Ok(batch) => sse_events(&batch),
            Err(e) => sse_error(&e),
        }))
    });
    let headers = [
        (header::CONTENT_TYPE, HeaderValue::from_static(EVENT_STREAM)),
        (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
    ];
    Ok((headers, Body::from_stream(events)).into_response())
}

/// A batch as SSE events, or a heartbeat comment if it has none.
fn sse_events(batch: &pb::GetChangesResponse) -> Vec<u8> {
    if batch.events.is_empty() {
        return format!(": next seq {}\n\n", batch.next_seq).into_bytes();
    }
    let mut out = Vec::new();
    for event in &batch.events {
        match serde_json::to_vec(event) {
            Ok(json) => {
                out.extend_from_slice(format!("id: {}\nevent: change\ndata: ", event.seq).as_bytes());
                // JSON has no raw newlines: one data line
                out.extend_from_slice(&json);
                out.extend_from_slice(b"\n\n");
            }
            Err(e) => {
                out.extend(sse_error(&Error::internal(format!("can't write change {}: {}", event.seq, e))));
                break;
            }
        }
    }
    out
}

fn sse_error(e: &Error) -> Vec<u8> {
    let body = pb::Error { code: e.code().as_str().to_owned(), message: e.message().to_owned() };
    let json = serde_json::to_vec(&body).unwrap_or_default();
    let mut out = b"event: error\ndata: ".to_vec();
    out.extend_from_slice(&json);
    out.extend_from_slice(b"\n\n");
    out
}

/// The chunks of an answer of one item: there is one.
fn merged<T: Default>(mut chunks: Vec<T>) -> Result<T, Failure> {
    match chunks.len() {
        1 => Ok(chunks.pop().unwrap_or_default()),
        n => Err(Error::internal(format!("an answer of one item came in {} chunks", n)).into()),
    }
}

async fn openapi_document() -> Response {
    json_response(openapi::document().as_bytes().to_vec(), JSON)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_headers_ask_for_ndjson() {
        let accept = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT, HeaderValue::from_str(v).expect("ascii"));
            wants_ndjson(&h)
        };
        assert!(accept("application/x-ndjson"));
        assert!(accept("application/json;q=0.5, Application/X-NDJSON; q=1"));
        assert!(!accept("application/json"));
        assert!(!accept("*/*"));
        assert!(!wants_ndjson(&HeaderMap::new()));
    }

    #[test]
    fn every_route_has_a_unique_operation() {
        let mut operations: Vec<_> = ROUTES.iter().map(|r| r.operation).collect();
        operations.sort_unstable();
        operations.dedup();
        assert_eq!(operations.len(), ROUTES.len());
    }
}
