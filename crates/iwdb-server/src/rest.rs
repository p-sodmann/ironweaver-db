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

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, post, put};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use iwdb_query::audit::AuditSink;
use iwdb_query::{Authorized, Code, Error};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::auth::{AuthMode, Caller, SESSION_COOKIE, Served, audit_of, authorized};
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
    /// These query parameters (name, type, description): the request
    /// message's fields.
    Query(&'static [(&'static str, &'static str, &'static str)]),
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

    /// A route the server serves before the router, in every build: health
    /// and the Prometheus metrics.
    pub fn gate(&self) -> bool {
        self.health() || self.operation == "prometheusMetrics"
    }

    /// A route that needs no credentials: health, login, the OpenAPI
    /// document (step 15a).
    pub fn open(&self) -> bool {
        self.health() || matches!(self.operation, "login" | "openapi")
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
        Method::GET,
        "/v1/namespaces/{ns}/schema",
        "GetSchema",
        Input::Options,
        "getSchema",
        "A namespace's schema, sampled",
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
    // Authentication, users, grants, tokens (auth.proto, step 15a)
    route(Method::POST, crate::auth::LOGIN_PATH, "Login", BODY, "login", "Log in: start a session"),
    route(Method::POST, "/v1/auth/logout", "Logout", OPTIONAL_BODY, "logout", "End the caller's session"),
    route(Method::GET, "/v1/auth/whoami", "WhoAmI", Input::Nothing, "whoAmI", "The caller and its roles"),
    route(Method::GET, "/v1/users", "ListUsers", Input::Nothing, "listUsers", "List the users"),
    route(Method::POST, "/v1/users", "CreateUser", BODY, "createUser", "Create a user"),
    route(Method::DELETE, "/v1/users/{user}", "DeleteUser", OPTIONAL_BODY, "deleteUser", "Delete a user"),
    route(Method::PUT, "/v1/users/{user}/password", "SetPassword", BODY, "setPassword", "Set a user's password"),
    route(Method::PUT, "/v1/users/{user}/admin", "SetAdmin", BODY, "setAdmin", "Make a user a server admin or not"),
    route(Method::PUT, "/v1/users/{user}/grants/{ns}", "Grant", BODY, "grant", "Give a user a role on a namespace"),
    route(
        Method::DELETE,
        "/v1/users/{user}/grants/{ns}",
        "Revoke",
        OPTIONAL_BODY,
        "revoke",
        "Take a user's role on a namespace away",
    ),
    route(Method::GET, "/v1/users/{user}/tokens", "ListTokens", Input::Nothing, "listTokens", "A user's API tokens"),
    route(Method::POST, "/v1/users/{user}/tokens", "CreateToken", BODY, "createToken", "Make an API token"),
    route(
        Method::DELETE,
        "/v1/users/{user}/tokens/{token}",
        "RevokeToken",
        OPTIONAL_BODY,
        "revokeToken",
        "Revoke an API token",
    ),
    // The operator's reads (admin.proto, step 16c)
    route(Method::GET, "/v1/status", "GetServerStatus", Input::Nothing, "getServerStatus", "The server's status"),
    route(
        Method::GET,
        "/v1/requests",
        "ListRequests",
        Input::Query(REQUESTS_PARAMETERS),
        "listRequests",
        "The running requests",
    ),
    route(
        Method::POST,
        "/v1/requests/{request}/cancel",
        "CancelRequest",
        OPTIONAL_BODY,
        "cancelRequest",
        "Cancel a running request",
    ),
    route(Method::GET, "/v1/consumers", "ListConsumers", Input::Nothing, "listConsumers", "The change-stream readers"),
    route(Method::GET, "/v1/metrics", "GetMetrics", Input::Nothing, "getMetrics", "The metrics"),
    route(Method::GET, "/v1/log", "GetLog", Input::Query(LOG_PARAMETERS), "getLog", "The server's last log events"),
    // Served by the gate in every build, also without `rest`
    route(
        Method::GET,
        crate::metrics::METRICS_PATH,
        "GetMetrics",
        Input::Nothing,
        "prometheusMetrics",
        "The metrics in Prometheus' text format",
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

/// The query parameters of `GET /v1/requests`.
pub(crate) const REQUESTS_PARAMETERS: &[(&str, &str, &str)] = &[
    ("user", "string", "Only this user's requests (others than a server admin see only their own)."),
    ("limit", "integer", "At most this many (default and maximum 1000)."),
];

/// The query parameters of `GET /v1/log`.
pub(crate) const LOG_PARAMETERS: &[(&str, &str, &str)] = &[
    ("after", "integer", "The events after this one (0 or none: from the oldest kept)."),
    ("limit", "integer", "At most this many (default and maximum 1000)."),
];

/// What the handlers share.
struct Shared<D> {
    db: Arc<D>,
    mode: AuthMode,
    audit: Arc<dyn AuditSink>,
    max_body: usize,
    /// Turns true when the server shuts down: change streams end.
    stopping: watch::Receiver<bool>,
}

type St<D> = State<Arc<Shared<D>>>;

/// The caller the gate attached (none when the router runs without it).
type Caller_ = Option<Extension<Caller>>;

impl<D: iwdb_query::Admin> Shared<D> {
    /// The database as the caller may use it (design rule 8, ADR 0045).
    fn db(&self, caller: &Caller_) -> Result<Authorized<D>, Failure> {
        Ok(authorized(&self.db, self.mode, caller.as_ref().map(|Extension(c)| c), &self.audit)?)
    }
}

/// The operation of the route `method` and `path` match (for the audit
/// entry of a request the gate refuses); `None` if none does.
pub(crate) fn operation_of(method: &Method, path: &str) -> Option<iwdb_query::Operation> {
    let segments: Vec<&str> = path.split('/').collect();
    let route = ROUTES.iter().find(|r| {
        let pattern: Vec<&str> = r.path.split('/').collect();
        r.method == *method
            && pattern.len() == segments.len()
            && pattern.iter().zip(&segments).all(|(p, s)| p == s || (p.starts_with('{') && !s.is_empty()))
    })?;
    iwdb_query::Operation::from_rpc(route.rpc?)
}

/// The REST routes over `db`, with request bodies of at most `max_body`
/// bytes, auditing into `audit`. Requests that match no route are answered
/// here too (404). The change streams end when `stopping` turns true.
pub fn router<D: Served>(
    db: Arc<D>,
    max_body: usize,
    stopping: watch::Receiver<bool>,
    mode: AuthMode,
    audit: Arc<dyn AuditSink>,
) -> Router {
    let mut router = Router::new();
    for r in ROUTES.iter().filter(|r| !r.gate()) {
        router = router.route(r.path, handler::<D>(r));
    }
    router
        .fallback(|| async { Failure::http(StatusCode::NOT_FOUND, "no such route; see /v1/openapi.json") })
        .method_not_allowed_fallback(|| async {
            Failure::http(StatusCode::METHOD_NOT_ALLOWED, "the route doesn't take this method")
        })
        .with_state(Arc::new(Shared { db, mode, audit, max_body, stopping }))
}

/// The handler of `r`.
fn handler<D: Served>(r: &Route) -> MethodRouter<Arc<Shared<D>>> {
    match r.operation {
        "listNamespaces" => get(list_namespaces::<D>),
        "createNamespace" => put(create_namespace::<D>),
        "dropNamespace" => delete(drop_namespace::<D>),
        "getNamespaceStatus" => get(namespace_status::<D>),
        "getCatalog" => get(catalog::<D>),
        "getSchema" => get(schema::<D>),
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
        "login" => post(login::<D>),
        "logout" => post(logout::<D>),
        "whoAmI" => get(who_am_i::<D>),
        "listUsers" => get(list_users::<D>),
        "createUser" => post(create_user::<D>),
        "deleteUser" => delete(delete_user::<D>),
        "setPassword" => put(set_password::<D>),
        "setAdmin" => put(set_admin::<D>),
        "grant" => put(grant::<D>),
        "revoke" => delete(revoke::<D>),
        "listTokens" => get(list_tokens::<D>),
        "createToken" => post(create_token::<D>),
        "revokeToken" => delete(revoke_token::<D>),
        "getServerStatus" => get(get_server_status::<D>),
        "listRequests" => get(list_requests::<D>),
        "cancelRequest" => post(cancel_request::<D>),
        "listConsumers" => get(list_consumers::<D>),
        "getMetrics" => get(get_metrics::<D>),
        "getLog" => get(get_log::<D>),
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

async fn list_namespaces<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::list_namespaces(&s.db(&caller)?).await)
}

async fn create_namespace<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let mut r: pb::CreateNamespaceRequest = read(&headers, body, s.max_body).await?;
    namespace(&mut r.name, path(p)?)?;
    unary(ops::create_namespace(&s.db(&caller)?, r).await)
}

async fn drop_namespace<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let mut r: pb::DropNamespaceRequest = read(&headers, body, s.max_body).await?;
    namespace(&mut r.name, path(p)?)?;
    unary(ops::drop_namespace(&s.db(&caller)?, r).await)
}

async fn namespace_status<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
) -> Answer {
    let r = pb::GetNamespaceStatusRequest { namespace: path(p)? };
    unary(ops::get_namespace_status(&s.db(&caller)?, r).await)
}

async fn catalog<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let r = pb::GetCatalogRequest { namespace: path(p)?, options: options(q)? };
    unary(ops::get_catalog(&s.db(&caller)?, r, None).await)
}

async fn schema<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let r = pb::GetSchemaRequest { namespace: path(p)?, options: options(q)? };
    unary(ops::get_schema(&s.db(&caller)?, r, None).await)
}

/// A handler that reads its body, takes the namespace from the path, and
/// runs its operation.
macro_rules! body_handler {
    ($name:ident, $request:ty, |$db:ident, $r:ident, $headers:ident| $run:expr) => {
        async fn $name<D: Served>(
            State(s): St<D>,
            caller: Caller_,
            p: Result<Path<String>, PathRejection>,
            $headers: HeaderMap,
            body: Body,
        ) -> Answer {
            let $db = s.db(&caller)?;
            let mut $r: $request = read(&$headers, body, s.max_body).await?;
            namespace(&mut $r.namespace, path(p)?)?;
            $run
        }
    };
}

body_handler!(commit_catalog, pb::CommitCatalogRequest, |db, r, _h| unary(ops::commit_catalog(&db, r).await));
body_handler!(commit, pb::CommitRequest, |db, r, _h| unary(ops::commit(&db, r).await));
body_handler!(wait_for_seq, pb::WaitForSeqRequest, |db, r, _h| unary(ops::wait_for_seq(&db, r, None).await));
body_handler!(get_nodes, pb::GetNodesRequest, |db, r, h| streamed(&h, ops::get_nodes(&db, r, None).await));
body_handler!(get_edges, pb::GetEdgesRequest, |db, r, h| streamed(&h, ops::get_edges(&db, r, None).await));
body_handler!(find, pb::FindRequest, |db, r, h| streamed(&h, ops::find(&db, r, None).await));
body_handler!(explain, pb::ExplainRequest, |db, r, _h| unary(ops::explain(&db, r, None).await));
body_handler!(neighbourhood, pb::NeighbourhoodRequest, |db, r, h| streamed(&h, ops::neighbourhood(&db, r, None).await));
body_handler!(traverse, pb::TraverseRequest, |db, r, h| streamed(&h, ops::traverse(&db, r, None).await));
body_handler!(shortest_path, pb::ShortestPathRequest, |db, r, _h| unary(ops::shortest_path(&db, r, None).await));
body_handler!(random_walks, pb::RandomWalksRequest, |db, r, h| streamed(&h, ops::random_walks(&db, r, None).await));
body_handler!(subgraph, pb::SubgraphRequest, |db, r, h| streamed(&h, ops::subgraph(&db, r, None).await));
body_handler!(match_pattern, pb::MatchPatternRequest, |db, r, h| streamed(&h, ops::match_pattern(&db, r, None).await));
body_handler!(analyze, pb::AnalyzeRequest, |db, r, h| streamed(&h, ops::analyze(&db, r, None).await));

/// One node by id: its `GetNodesResponse`, or 404 if it doesn't exist.
async fn get_node<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<(String, String)>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let (ns, id) = path(p)?;
    let r = pb::GetNodesRequest { namespace: ns.clone(), ids: vec![id.clone()], options: options(q)? };
    let answer = merged(ops::get_nodes(&s.db(&caller)?, r, None).await?)?;
    if answer.nodes.iter().all(|n| n.node.is_none()) {
        return Err(Error::new(Code::NotFound, format!("no node '{}' in namespace '{}'", id, ns)).into());
    }
    unary(Ok(answer))
}

/// One edge by id: its `GetEdgesResponse`, or 404 if it doesn't exist.
async fn get_edge<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<(String, String)>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let (ns, id) = path(p)?;
    let edge: u64 = id.parse().map_err(|_| Error::invalid(format!("invalid edge id '{}'", id)))?;
    let r = pb::GetEdgesRequest { namespace: ns.clone(), ids: vec![edge], options: options(q)? };
    let answer = merged(ops::get_edges(&s.db(&caller)?, r, None).await?)?;
    if answer.edges.iter().all(|e| e.edge.is_none()) {
        return Err(Error::new(Code::NotFound, format!("no edge {} in namespace '{}'", edge, ns)).into());
    }
    unary(Ok(answer))
}

async fn get_changes<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<ChangesQuery>, QueryRejection>,
) -> Answer {
    let (from_seq, wait, options) = changes_query(q, false)?;
    let r = pb::GetChangesRequest { namespace: path(p)?, from_seq, wait, options };
    unary(ops::get_changes(&s.db(&caller)?, r, None).await)
}

/// The change stream as Server-Sent Events (ADR 0031): a `change` event per
/// commit, with the seq as its `id` and the `ChangeEvent` as its data; a
/// comment line as heartbeat after a round without commits; an `error`
/// event with an `Error` before the stream ends on an error (`unavailable`
/// at shutdown). A `Last-Event-ID` header (what `EventSource` sends when it
/// reconnects) resumes after that seq, instead of `from_seq`. An error in
/// the first batch (no namespace, `not_retained`, ...) is the answer's
/// status instead.
async fn watch_changes<D: Served>(
    State(s): St<D>,
    caller: Caller_,
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
    let mut batches = ops::follow(Arc::new(s.db(&caller)?), r, None, s.stopping.clone());
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

// ---- authentication, users, grants, tokens (step 15a) ----

/// A field of a body that names what the path names already.
fn same(field: &mut String, path: &str, what: &str) -> Result<(), Failure> {
    if !field.is_empty() && field != path {
        return Err(Error::invalid(format!("the body names {} '{}', the path '{}'", what, field, path)).into());
    }
    *field = path.to_owned();
    Ok(())
}

/// The session cookie (ADR 0046): HttpOnly (no script reads it),
/// SameSite=Strict (no other site sends it), and `Secure` (sent over
/// HTTPS only) when the request came over TLS (step 15b): browsers drop
/// Secure cookies set over plain HTTP, which a server with
/// `[tls] enabled = false` speaks.
fn session_cookie(token: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let text = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_COOKIE,
        token,
        max_age_secs,
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("iwdb_session=; Max-Age=0"))
}

/// `POST /v1/auth/login`: the session's token in the body, or with `cookie`
/// as the session cookie.
async fn login<D: Served>(State(s): St<D>, caller: Caller_, headers: HeaderMap, body: Body) -> Answer {
    let r: pb::LoginRequest = read(&headers, body, s.max_body).await?;
    let cookie = r.cookie;
    let audit = audit_of(caller.as_ref().map(|Extension(c)| c), &s.audit);
    let secure = caller.as_ref().is_some_and(|Extension(c)| c.tls);
    let (response, session) = ops::login(&*s.db, r, &audit).await?;
    let mut answer = unary(Ok(response))?;
    if cookie {
        let max_age = session.expires_ms.saturating_sub(iwdb::auth::now_ms()) / 1000;
        answer.headers_mut().insert(header::SET_COOKIE, session_cookie(session.token.expose(), max_age, secure));
    }
    answer.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(answer)
}

/// `POST /v1/auth/logout`: ends the session and clears the cookie.
async fn logout<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    let db = s.db(&caller)?;
    let token = caller.as_ref().and_then(|Extension(c)| c.token.clone());
    let secure = caller.as_ref().is_some_and(|Extension(c)| c.tls);
    let mut answer = unary(ops::logout(&db, token.as_ref()).await)?;
    answer.headers_mut().insert(header::SET_COOKIE, session_cookie("", 0, secure));
    Ok(answer)
}

async fn who_am_i<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(Ok(ops::who_am_i(&s.db(&caller)?, s.mode)))
}

// ---- the operator's reads (step 16c) ----

async fn get_server_status<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::get_server_status(&s.db(&caller)?).await)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestsQuery {
    user: Option<String>,
    limit: Option<u32>,
}

async fn list_requests<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    q: Result<Query<RequestsQuery>, QueryRejection>,
) -> Answer {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    unary(ops::list_requests(&s.db(&caller)?, pb::ListRequestsRequest { user: q.user, limit: q.limit }).await)
}

async fn cancel_request<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<u64>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let db = s.db(&caller)?;
    let mut r: pb::CancelRequestRequest = read(&headers, body, s.max_body).await?;
    let id = path(p)?;
    if r.id != 0 && r.id != id {
        return Err(Error::invalid(format!("the body names request {}, the path {}", r.id, id)).into());
    }
    r.id = id;
    unary(ops::cancel_request(&db, r).await)
}

async fn list_consumers<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::list_consumers(&s.db(&caller)?).await)
}

async fn get_metrics<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::get_metrics(&s.db(&caller)?).await)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogQuery {
    after: Option<u64>,
    limit: Option<u32>,
}

async fn get_log<D: Served>(State(s): St<D>, caller: Caller_, q: Result<Query<LogQuery>, QueryRejection>) -> Answer {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    unary(ops::get_log(&s.db(&caller)?, pb::GetLogRequest { after: q.after.unwrap_or(0), limit: q.limit }).await)
}

async fn list_users<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::list_users(&s.db(&caller)?).await)
}

async fn create_user<D: Served>(State(s): St<D>, caller: Caller_, headers: HeaderMap, body: Body) -> Answer {
    let db = s.db(&caller)?;
    let r: pb::CreateUserRequest = read(&headers, body, s.max_body).await?;
    unary(ops::create_user(&db, r).await)
}

/// A handler of a route under `/v1/users/{user}` whose body is the RPC's
/// request message: the user (and namespace or token name) from the path.
macro_rules! user_handler {
    ($name:ident, $request:ty, $params:ty, |$r:ident, $p:ident| $fill:block, $op:ident) => {
        async fn $name<D: Served>(
            State(s): St<D>,
            caller: Caller_,
            p: Result<Path<$params>, PathRejection>,
            headers: HeaderMap,
            body: Body,
        ) -> Answer {
            let db = s.db(&caller)?;
            let mut $r: $request = read(&headers, body, s.max_body).await?;
            let $p = path(p)?;
            $fill
            unary(ops::$op(&db, $r).await)
        }
    };
}

user_handler!(
    delete_user,
    pb::DeleteUserRequest,
    String,
    |r, user| {
        same(&mut r.name, &user, "user")?;
    },
    delete_user
);
user_handler!(
    set_password,
    pb::SetPasswordRequest,
    String,
    |r, user| {
        same(&mut r.name, &user, "user")?;
    },
    set_password
);
user_handler!(
    set_admin,
    pb::SetAdminRequest,
    String,
    |r, user| {
        same(&mut r.name, &user, "user")?;
    },
    set_admin
);
user_handler!(
    grant,
    pb::GrantRequest,
    (String, String),
    |r, p| {
        same(&mut r.name, &p.0, "user")?;
        namespace(&mut r.namespace, p.1)?;
    },
    grant
);
user_handler!(
    revoke,
    pb::RevokeRequest,
    (String, String),
    |r, p| {
        same(&mut r.name, &p.0, "user")?;
        namespace(&mut r.namespace, p.1)?;
    },
    revoke
);
user_handler!(
    create_token,
    pb::CreateTokenRequest,
    String,
    |r, user| {
        same(&mut r.user, &user, "user")?;
    },
    create_token
);
user_handler!(
    revoke_token,
    pb::RevokeTokenRequest,
    (String, String),
    |r, p| {
        same(&mut r.user, &p.0, "user")?;
        same(&mut r.name, &p.1, "token")?;
    },
    revoke_token
);

async fn list_tokens<D: Served>(State(s): St<D>, caller: Caller_, p: Result<Path<String>, PathRejection>) -> Answer {
    let db = s.db(&caller)?;
    unary(ops::list_tokens(&db, pb::ListTokensRequest { user: path(p)? }).await)
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
