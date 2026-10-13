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
use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, delete, get, post, put};
use http::{Method, StatusCode};
use iwdb_query::Authorized;
use iwdb_query::audit::AuditSink;
use tokio::sync::watch;

use crate::auth::{AuthMode, Caller, Served, authorized};

mod admin;
mod changes;
mod graph;
mod sessions;
mod transport;

use admin::*;
use changes::*;
use graph::*;
use sessions::*;
pub(crate) use transport::Failure;
use transport::json_response;
#[cfg(test)]
use transport::wants_ndjson;

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

/// Internal route identity; dispatch is exhaustive and OpenAPI names stay stable.
#[derive(Clone, Copy, Debug)]
enum RouteId {
    ListNamespaces,
    CreateNamespace,
    DropNamespace,
    GetNamespaceStatus,
    GetCatalog,
    GetSchema,
    CommitCatalog,
    Commit,
    WaitForSeq,
    GetNode,
    GetNodes,
    GetEdge,
    GetEdges,
    Find,
    Explain,
    Neighbourhood,
    Traverse,
    ShortestPath,
    RandomWalks,
    Subgraph,
    MatchPattern,
    Analyze,
    GetChanges,
    WatchChanges,
    Openapi,
    Login,
    Logout,
    WhoAmI,
    ListUsers,
    CreateUser,
    DeleteUser,
    SetPassword,
    SetAdmin,
    Grant,
    Revoke,
    ListTokens,
    CreateToken,
    RevokeToken,
    SetUserLimits,
    SetNamespaceLimits,
    GetLimits,
    GetServerStatus,
    ListRequests,
    CancelRequest,
    ListConsumers,
    GetMetrics,
    GetLog,
    Checkpoint,
    Backup,
    Verify,
    PruneArchive,
    StartJob,
    ListJobs,
    GetJob,
    CancelJob,
    GetJobResult,
    Live,
    Ready,
    PrometheusMetrics,
}

impl RouteId {
    const fn operation(self) -> &'static str {
        match self {
            Self::ListNamespaces => "listNamespaces",
            Self::CreateNamespace => "createNamespace",
            Self::DropNamespace => "dropNamespace",
            Self::GetNamespaceStatus => "getNamespaceStatus",
            Self::GetCatalog => "getCatalog",
            Self::GetSchema => "getSchema",
            Self::CommitCatalog => "commitCatalog",
            Self::Commit => "commit",
            Self::WaitForSeq => "waitForSeq",
            Self::GetNode => "getNode",
            Self::GetNodes => "getNodes",
            Self::GetEdge => "getEdge",
            Self::GetEdges => "getEdges",
            Self::Find => "find",
            Self::Explain => "explain",
            Self::Neighbourhood => "neighbourhood",
            Self::Traverse => "traverse",
            Self::ShortestPath => "shortestPath",
            Self::RandomWalks => "randomWalks",
            Self::Subgraph => "subgraph",
            Self::MatchPattern => "matchPattern",
            Self::Analyze => "analyze",
            Self::GetChanges => "getChanges",
            Self::WatchChanges => "watchChanges",
            Self::Openapi => "openapi",
            Self::Login => "login",
            Self::Logout => "logout",
            Self::WhoAmI => "whoAmI",
            Self::ListUsers => "listUsers",
            Self::CreateUser => "createUser",
            Self::DeleteUser => "deleteUser",
            Self::SetPassword => "setPassword",
            Self::SetAdmin => "setAdmin",
            Self::Grant => "grant",
            Self::Revoke => "revoke",
            Self::ListTokens => "listTokens",
            Self::CreateToken => "createToken",
            Self::RevokeToken => "revokeToken",
            Self::SetUserLimits => "setUserLimits",
            Self::SetNamespaceLimits => "setNamespaceLimits",
            Self::GetLimits => "getLimits",
            Self::GetServerStatus => "getServerStatus",
            Self::ListRequests => "listRequests",
            Self::CancelRequest => "cancelRequest",
            Self::ListConsumers => "listConsumers",
            Self::GetMetrics => "getMetrics",
            Self::GetLog => "getLog",
            Self::Checkpoint => "checkpoint",
            Self::Backup => "backup",
            Self::Verify => "verify",
            Self::PruneArchive => "pruneArchive",
            Self::StartJob => "startJob",
            Self::ListJobs => "listJobs",
            Self::GetJob => "getJob",
            Self::CancelJob => "cancelJob",
            Self::GetJobResult => "getJobResult",
            Self::Live => "live",
            Self::Ready => "ready",
            Self::PrometheusMetrics => "prometheusMetrics",
        }
    }
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

// One table defines both public documentation descriptors and typed dispatch.
macro_rules! routes {
    ($($id:expr => ($method:expr, $path:expr, $rpc:expr, $input:expr, $summary:expr $(,)?)),* $(,)?) => {
        /// Every REST route.
        pub const ROUTES: &[Route] = &[$(Route {
            method: $method,
            path: $path,
            rpc: $rpc,
            input: $input,
            operation: $id.operation(),
            summary: $summary,
        }),*];
        const ROUTE_IDS: &[RouteId] = &[$($id),*];
    };
}

const BODY: Input = Input::Body { required: true };
const OPTIONAL_BODY: Input = Input::Body { required: false };

routes! {
    RouteId::ListNamespaces => (
        Method::GET,
        "/v1/namespaces",
        Some("ListNamespaces"),
        Input::Nothing,
        "List the namespaces",
    ),
    RouteId::CreateNamespace => (
        Method::PUT,
        "/v1/namespaces/{ns}",
        Some("CreateNamespace"),
        OPTIONAL_BODY,
        "Create a namespace",
    ),
    RouteId::DropNamespace => (
        Method::DELETE,
        "/v1/namespaces/{ns}",
        Some("DropNamespace"),
        OPTIONAL_BODY,
        "Drop a namespace",
    ),
    RouteId::GetNamespaceStatus => (
        Method::GET,
        "/v1/namespaces/{ns}",
        Some("GetNamespaceStatus"),
        Input::Nothing,
        "A namespace's status",
    ),
    RouteId::GetCatalog => (
        Method::GET,
        "/v1/namespaces/{ns}/catalog",
        Some("GetCatalog"),
        Input::Options,
        "A namespace's catalog",
    ),
    RouteId::GetSchema => (
        Method::GET,
        "/v1/namespaces/{ns}/schema",
        Some("GetSchema"),
        Input::Options,
        "A namespace's schema, sampled",
    ),
    RouteId::CommitCatalog => (
        Method::POST,
        "/v1/namespaces/{ns}/catalog",
        Some("CommitCatalog"),
        BODY,
        "Commit a catalog change",
    ),
    RouteId::Commit => (Method::POST, "/v1/namespaces/{ns}/commit", Some("Commit"), BODY, "Commit a transaction"),
    RouteId::WaitForSeq => (
        Method::POST,
        "/v1/namespaces/{ns}/wait",
        Some("WaitForSeq"),
        BODY,
        "Wait until a seq is applied",
    ),
    RouteId::GetNode => (
        Method::GET,
        "/v1/namespaces/{ns}/nodes/{id}",
        Some("GetNodes"),
        Input::Options,
        "One node (404 if missing)",
    ),
    RouteId::GetNodes => (Method::POST, "/v1/namespaces/{ns}/get-nodes", Some("GetNodes"), BODY, "Nodes by id"),
    RouteId::GetEdge => (
        Method::GET,
        "/v1/namespaces/{ns}/edges/{id}",
        Some("GetEdges"),
        Input::Options,
        "One edge (404 if missing)",
    ),
    RouteId::GetEdges => (Method::POST, "/v1/namespaces/{ns}/get-edges", Some("GetEdges"), BODY, "Edges by id"),
    RouteId::Find => (Method::POST, "/v1/namespaces/{ns}/find", Some("Find"), BODY, "Nodes matching a filter"),
    RouteId::Explain => (
        Method::POST,
        "/v1/namespaces/{ns}/explain",
        Some("Explain"),
        BODY,
        "How find would read a filter",
    ),
    RouteId::Neighbourhood => (
        Method::POST,
        "/v1/namespaces/{ns}/neighbourhood",
        Some("Neighbourhood"),
        BODY,
        "Nodes near seeds",
    ),
    RouteId::Traverse => (
        Method::POST,
        "/v1/namespaces/{ns}/traverse",
        Some("Traverse"),
        BODY,
        "Breadth or depth first order",
    ),
    RouteId::ShortestPath => (
        Method::POST,
        "/v1/namespaces/{ns}/shortest-path",
        Some("ShortestPath"),
        BODY,
        "A shortest path",
    ),
    RouteId::RandomWalks => (
        Method::POST,
        "/v1/namespaces/{ns}/random-walks",
        Some("RandomWalks"),
        BODY,
        "Random walks",
    ),
    RouteId::Subgraph => (Method::POST, "/v1/namespaces/{ns}/subgraph", Some("Subgraph"), BODY, "An induced subgraph"),
    RouteId::MatchPattern => (Method::POST, "/v1/namespaces/{ns}/match", Some("MatchPattern"), BODY, "Match a pattern"),
    RouteId::Analyze => (Method::POST, "/v1/namespaces/{ns}/analyze", Some("Analyze"), BODY, "Run an analytics job"),
    RouteId::GetChanges => (
        Method::GET,
        "/v1/namespaces/{ns}/changes",
        Some("GetChanges"),
        Input::Changes { stream: false },
        "A batch of the change stream",
    ),
    RouteId::WatchChanges => (
        Method::GET,
        "/v1/namespaces/{ns}/changes/stream",
        Some("Watch"),
        Input::Changes { stream: true },
        "Follow the change stream (Server-Sent Events)",
    ),
    RouteId::Login => (Method::POST, crate::auth::LOGIN_PATH, Some("Login"), BODY, "Log in: start a session"),
    RouteId::Logout => (Method::POST, "/v1/auth/logout", Some("Logout"), OPTIONAL_BODY, "End the caller's session"),
    RouteId::WhoAmI => (Method::GET, "/v1/auth/whoami", Some("WhoAmI"), Input::Nothing, "The caller and its roles"),
    RouteId::ListUsers => (Method::GET, "/v1/users", Some("ListUsers"), Input::Nothing, "List the users"),
    RouteId::CreateUser => (Method::POST, "/v1/users", Some("CreateUser"), BODY, "Create a user"),
    RouteId::DeleteUser => (Method::DELETE, "/v1/users/{user}", Some("DeleteUser"), OPTIONAL_BODY, "Delete a user"),
    RouteId::SetPassword => (
        Method::PUT,
        "/v1/users/{user}/password",
        Some("SetPassword"),
        BODY,
        "Set a user's password",
    ),
    RouteId::SetAdmin => (
        Method::PUT,
        "/v1/users/{user}/admin",
        Some("SetAdmin"),
        BODY,
        "Make a user a server admin or not",
    ),
    RouteId::Grant => (
        Method::PUT,
        "/v1/users/{user}/grants/{ns}",
        Some("Grant"),
        BODY,
        "Give a user a role on a namespace",
    ),
    RouteId::Revoke => (
        Method::DELETE,
        "/v1/users/{user}/grants/{ns}",
        Some("Revoke"),
        OPTIONAL_BODY,
        "Take a user's role on a namespace away",
    ),
    RouteId::ListTokens => (
        Method::GET,
        "/v1/users/{user}/tokens",
        Some("ListTokens"),
        Input::Nothing,
        "A user's API tokens",
    ),
    RouteId::CreateToken => (Method::POST, "/v1/users/{user}/tokens", Some("CreateToken"), BODY, "Make an API token"),
    RouteId::RevokeToken => (
        Method::DELETE,
        "/v1/users/{user}/tokens/{token}",
        Some("RevokeToken"),
        OPTIONAL_BODY,
        "Revoke an API token",
    ),
    RouteId::SetUserLimits => (
        Method::PUT,
        "/v1/users/{user}/limits",
        Some("SetUserLimits"),
        BODY,
        "Set a user's limits",
    ),
    RouteId::SetNamespaceLimits => (
        Method::PUT,
        "/v1/namespaces/{ns}/limits",
        Some("SetNamespaceLimits"),
        BODY,
        "Set a namespace's limits",
    ),
    RouteId::GetLimits => (
        Method::GET,
        "/v1/limits",
        Some("GetLimits"),
        Input::Nothing,
        "The default and namespace limits",
    ),
    RouteId::GetServerStatus => (
        Method::GET,
        "/v1/status",
        Some("GetServerStatus"),
        Input::Nothing,
        "The server's status",
    ),
    RouteId::ListRequests => (
        Method::GET,
        "/v1/requests",
        Some("ListRequests"),
        Input::Query(REQUESTS_PARAMETERS),
        "The running requests",
    ),
    RouteId::CancelRequest => (
        Method::POST,
        "/v1/requests/{request}/cancel",
        Some("CancelRequest"),
        OPTIONAL_BODY,
        "Cancel a running request",
    ),
    RouteId::ListConsumers => (
        Method::GET,
        "/v1/consumers",
        Some("ListConsumers"),
        Input::Nothing,
        "The change-stream readers",
    ),
    RouteId::GetMetrics => (Method::GET, "/v1/metrics", Some("GetMetrics"), Input::Nothing, "The metrics"),
    RouteId::GetLog => (
        Method::GET,
        "/v1/log",
        Some("GetLog"),
        Input::Query(LOG_PARAMETERS),
        "The server's last log events",
    ),
    RouteId::Checkpoint => (
        Method::POST,
        "/v1/checkpoint",
        Some("Checkpoint"),
        OPTIONAL_BODY,
        "Checkpoint a namespace, or every namespace",
    ),
    RouteId::Backup => (
        Method::POST,
        "/v1/backups",
        Some("Backup"),
        BODY,
        "Back up the store into the backup directory",
    ),
    RouteId::Verify => (
        Method::POST,
        "/v1/verify",
        Some("Verify"),
        OPTIONAL_BODY,
        "Verify the store, a backup or the WAL archive",
    ),
    RouteId::PruneArchive => (
        Method::POST,
        "/v1/archive/prune",
        Some("PruneArchive"),
        BODY,
        "Prune the WAL archive before a backup",
    ),
    RouteId::StartJob => (Method::POST, "/v1/namespaces/{ns}/jobs", Some("StartJob"), BODY, "Queue an analytics job"),
    RouteId::ListJobs => (Method::GET, "/v1/jobs", Some("ListJobs"), Input::Query(JOBS_PARAMETERS), "The jobs kept"),
    RouteId::GetJob => (
        Method::GET,
        "/v1/jobs/{job}",
        Some("GetJob"),
        Input::Query(JOB_PARAMETERS),
        "A job's state and progress",
    ),
    RouteId::CancelJob => (Method::POST, "/v1/jobs/{job}/cancel", Some("CancelJob"), OPTIONAL_BODY, "Cancel a job"),
    RouteId::GetJobResult => (
        Method::GET,
        "/v1/jobs/{job}/result",
        Some("GetJobResult"),
        Input::Query(JOB_RESULT_PARAMETERS),
        "A page of a done job's result",
    ),
    RouteId::PrometheusMetrics => (
        Method::GET,
        crate::metrics::METRICS_PATH,
        Some("GetMetrics"),
        Input::Nothing,
        "The metrics in Prometheus' text format",
    ),
    RouteId::Live => (
        Method::GET,
        crate::health::LIVE_PATH,
        None,
        Input::Nothing,
        "Liveness: 200 whenever the server answers",
    ),
    RouteId::Ready => (
        Method::GET,
        crate::health::READY_PATH,
        None,
        Input::Nothing,
        "Readiness: 200 once recovery has finished, 503 before and while shutting down",
    ),
    RouteId::Openapi => (Method::GET, "/v1/openapi.json", None, Input::Nothing, "This API's OpenAPI document"),
}

/// The query parameters of `GET /v1/requests`.
pub(crate) const REQUESTS_PARAMETERS: &[(&str, &str, &str)] = &[
    ("user", "string", "Only this user's requests (others than a server admin see only their own)."),
    ("limit", "integer", "At most this many (default and maximum 1000)."),
];

/// The query parameters of `GET /v1/jobs`.
pub(crate) const JOBS_PARAMETERS: &[(&str, &str, &str)] = &[
    ("user", "string", "Only this user's jobs (others than a server admin see only their own)."),
    ("limit", "integer", "At most this many (default and maximum 1000)."),
];

/// The query parameters of `GET /v1/jobs/{job}`.
pub(crate) const JOB_PARAMETERS: &[(&str, &str, &str)] =
    &[("user", "string", "Only if it is this user's job; otherwise 404.")];

/// The query parameters of `GET /v1/jobs/{job}/result`.
pub(crate) const JOB_RESULT_PARAMETERS: &[(&str, &str, &str)] = &[
    ("user", "string", "Only if it is this user's job; otherwise 404."),
    ("offset", "integer", "The first row (default 0)."),
    ("limit", "integer", "At most this many rows (default and maximum 10000); a page also stops before about 4 MiB."),
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
    for (r, id) in ROUTES.iter().zip(ROUTE_IDS).filter(|(r, _)| !r.gate()) {
        router = router.route(r.path, handler::<D>(*id));
    }
    router
        .fallback(|| async { Failure::http(StatusCode::NOT_FOUND, "no such route; see /v1/openapi.json") })
        .method_not_allowed_fallback(|| async {
            Failure::http(StatusCode::METHOD_NOT_ALLOWED, "the route doesn't take this method")
        })
        .with_state(Arc::new(Shared { db, mode, audit, max_body, stopping }))
}

/// The handler of a registered route.
fn handler<D: Served>(id: RouteId) -> MethodRouter<Arc<Shared<D>>> {
    match id {
        RouteId::ListNamespaces => get(list_namespaces::<D>),
        RouteId::CreateNamespace => put(create_namespace::<D>),
        RouteId::DropNamespace => delete(drop_namespace::<D>),
        RouteId::GetNamespaceStatus => get(namespace_status::<D>),
        RouteId::GetCatalog => get(catalog::<D>),
        RouteId::GetSchema => get(schema::<D>),
        RouteId::CommitCatalog => post(commit_catalog::<D>),
        RouteId::Commit => post(commit::<D>),
        RouteId::WaitForSeq => post(wait_for_seq::<D>),
        RouteId::GetNode => get(get_node::<D>),
        RouteId::GetNodes => post(get_nodes::<D>),
        RouteId::GetEdge => get(get_edge::<D>),
        RouteId::GetEdges => post(get_edges::<D>),
        RouteId::Find => post(find::<D>),
        RouteId::Explain => post(explain::<D>),
        RouteId::Neighbourhood => post(neighbourhood::<D>),
        RouteId::Traverse => post(traverse::<D>),
        RouteId::ShortestPath => post(shortest_path::<D>),
        RouteId::RandomWalks => post(random_walks::<D>),
        RouteId::Subgraph => post(subgraph::<D>),
        RouteId::MatchPattern => post(match_pattern::<D>),
        RouteId::Analyze => post(analyze::<D>),
        RouteId::GetChanges => get(get_changes::<D>),
        RouteId::WatchChanges => get(watch_changes::<D>),
        RouteId::Openapi => get(openapi_document),
        RouteId::Login => post(login::<D>),
        RouteId::Logout => post(logout::<D>),
        RouteId::WhoAmI => get(who_am_i::<D>),
        RouteId::ListUsers => get(list_users::<D>),
        RouteId::CreateUser => post(create_user::<D>),
        RouteId::DeleteUser => delete(delete_user::<D>),
        RouteId::SetPassword => put(set_password::<D>),
        RouteId::SetAdmin => put(set_admin::<D>),
        RouteId::Grant => put(grant::<D>),
        RouteId::Revoke => delete(revoke::<D>),
        RouteId::ListTokens => get(list_tokens::<D>),
        RouteId::CreateToken => post(create_token::<D>),
        RouteId::RevokeToken => delete(revoke_token::<D>),
        RouteId::SetUserLimits => put(set_user_limits::<D>),
        RouteId::SetNamespaceLimits => put(set_namespace_limits::<D>),
        RouteId::GetLimits => get(get_limits::<D>),
        RouteId::GetServerStatus => get(get_server_status::<D>),
        RouteId::ListRequests => get(list_requests::<D>),
        RouteId::CancelRequest => post(cancel_request::<D>),
        RouteId::ListConsumers => get(list_consumers::<D>),
        RouteId::GetMetrics => get(get_metrics::<D>),
        RouteId::GetLog => get(get_log::<D>),
        RouteId::Checkpoint => post(checkpoint::<D>),
        RouteId::Backup => post(backup::<D>),
        RouteId::Verify => post(verify::<D>),
        RouteId::PruneArchive => post(prune_archive::<D>),
        RouteId::StartJob => post(start_job::<D>),
        RouteId::ListJobs => get(list_jobs::<D>),
        RouteId::GetJob => get(get_job::<D>),
        RouteId::CancelJob => post(cancel_job::<D>),
        RouteId::GetJobResult => get(get_job_result::<D>),
        RouteId::Live | RouteId::Ready | RouteId::PrometheusMetrics => {
            unreachable!("gate routes are served before the REST router")
        }
    }
}

async fn openapi_document() -> Response {
    json_response(openapi::document().as_bytes().to_vec(), JSON)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, header};

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
