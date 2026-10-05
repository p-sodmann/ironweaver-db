//! Serving a [`Server`] on a TCP listener, and shutting it down gracefully
//! (ADR 0027); and [`launch`]: binding first, opening the store while
//! health is already served (step 16b, ADR 0040).
//!
//! One port serves both APIs (ADR 0030): connections speak HTTP/2 or
//! HTTP/1.1 (detected per connection), and a request whose content type is
//! `application/grpc...` goes to the gRPC service, every other one to the
//! REST router.
//! Without the `rest` feature (ADR 0034) every other request is answered
//! 404 with an empty body.
//!
//! In front of both sits the gate: it answers health (gRPC
//! `grpc.health.v1.Health`, REST `/v1/health/live` and `/v1/health/ready`)
//! and, with the `console` feature turned on, the operator console's pages,
//! at any time; and database calls only once the store is open. Before
//! that they fail with `unavailable`.
//!
//! With TLS (step 15b, ADR 0048) every connection starts with a rustls
//! handshake (ALPN `h2` and `http/1.1`, at most [`HANDSHAKE_TIMEOUT`]);
//! the client certificate it verified, if any, goes to the gate with the
//! connection. Plaintext connections are served only when the server has
//! no TLS.
//!
//! The accept loop is ours rather than tonic's `transport::Server`, for two
//! reasons: tonic's server wraps every service in a `grpc-timeout` layer
//! that would race the database's own deadline and answer `CANCELLED`
//! instead of the trait's `timeout` (ADR 0026), and it spawns connections
//! where a shutdown can't reach them. Here every connection is a task of a
//! `JoinSet`: shutdown asks them to finish (HTTP/2 GOAWAY), and cancels the
//! ones still running at the end of the drain.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

#[cfg(feature = "rest")]
use axum::body::Body;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use iwdb_query::{Code, Error};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tonic_health::pb::health_server::HealthServer;
use tower_service::Service;
use tracing::Instrument;

use iwdb_query::audit::AuditSink;

use crate::auth::{AuthMode, Connection, GRPC_AUTH_PREFIX, Served};
use crate::health::{GRPC_PREFIX, GrpcHealth, Health, LIVE_PATH, Phase, READY_PATH};
use crate::tls::{ClientCertificate, ServerTls};

/// How long a client has for its TLS handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
use crate::proto::auth_service_server::AuthServiceServer;
use crate::proto::database_service_server::DatabaseServiceServer;
use crate::{Adapter, Server};

/// A body of either API: axum's with REST, tonic's without.
#[cfg(not(feature = "rest"))]
type Body = tonic::body::Body;

/// Both APIs as one service: gRPC by content type (`AuthService` by its
/// path, `DatabaseService` otherwise), REST otherwise. The gate
/// authenticates with `db` before it passes a request on.
pub(crate) struct Dispatch<D> {
    db: Arc<D>,
    mode: AuthMode,
    audit: Arc<dyn AuditSink>,
    grpc: DatabaseServiceServer<Adapter<D>>,
    auth: AuthServiceServer<Adapter<D>>,
    #[cfg(feature = "rest")]
    rest: axum::Router,
}

// Not derived: that would require `D: Clone`
impl<D> Clone for Dispatch<D> {
    fn clone(&self) -> Self {
        Dispatch {
            db: self.db.clone(),
            mode: self.mode,
            audit: self.audit.clone(),
            grpc: self.grpc.clone(),
            auth: self.auth.clone(),
            #[cfg(feature = "rest")]
            rest: self.rest.clone(),
        }
    }
}

impl<D> Dispatch<D> {
    #[cfg(feature = "rest")]
    pub(crate) fn new(
        db: Arc<D>,
        mode: AuthMode,
        audit: Arc<dyn AuditSink>,
        grpc: DatabaseServiceServer<Adapter<D>>,
        auth: AuthServiceServer<Adapter<D>>,
        rest: axum::Router,
    ) -> Self {
        Dispatch { db, mode, audit, grpc, auth, rest }
    }

    #[cfg(not(feature = "rest"))]
    pub(crate) fn new(
        db: Arc<D>,
        mode: AuthMode,
        audit: Arc<dyn AuditSink>,
        grpc: DatabaseServiceServer<Adapter<D>>,
        auth: AuthServiceServer<Adapter<D>>,
    ) -> Self {
        Dispatch { db, mode, audit, grpc, auth }
    }
}

fn is_grpc<B>(request: &http::Request<B>) -> bool {
    request.headers().get(http::header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"application/grpc"))
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl<D, B> Service<http::Request<B>> for Dispatch<D>
where
    D: Served,
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Result<Self::Response, Infallible>>;

    /// Both services are always ready (tonic's server and axum's router).
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if is_grpc(&request) && request.uri().path().starts_with(GRPC_AUTH_PREFIX) {
            let mut auth = self.auth.clone();
            Box::pin(async move {
                let response = auth.call(request).await?;
                Ok(response.map(Body::new))
            })
        } else if is_grpc(&request) {
            let mut grpc = self.grpc.clone();
            Box::pin(async move {
                let response = grpc.call(request).await?;
                Ok(response.map(Body::new))
            })
        } else {
            self.not_grpc(request)
        }
    }
}

impl<D> Dispatch<D> {
    #[cfg(feature = "rest")]
    fn not_grpc<B>(&self, request: http::Request<B>) -> BoxFuture<Result<http::Response<Body>, Infallible>>
    where
        B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
    {
        let mut rest = self.rest.clone();
        Box::pin(async move { rest.call(request.map(Body::new)).await })
    }

    /// No REST in this build: nothing but gRPC is found.
    #[cfg(not(feature = "rest"))]
    fn not_grpc<B>(&self, _request: http::Request<B>) -> BoxFuture<Result<http::Response<Body>, Infallible>> {
        Box::pin(std::future::ready(Ok(not_found())))
    }
}

#[cfg(not(feature = "rest"))]
fn not_found() -> http::Response<Body> {
    let mut response = http::Response::new(Body::empty());
    *response.status_mut() = http::StatusCode::NOT_FOUND;
    response
}

/// What the gate passes database calls to, once the store is open.
type Slot<D> = Arc<OnceLock<Dispatch<D>>>;

/// The service of every connection: health and the console at any time,
/// database calls once [`Slot`] holds the database's service.
pub(crate) struct Gate<D> {
    /// The connection (set per connection): its peer, for the login
    /// slowdown, and its TLS.
    connection: Connection,
    /// Serve TLS only, with this configuration.
    tls: Option<Arc<ServerTls>>,
    health: Health,
    grpc_health: HealthServer<GrpcHealth>,
    inner: Slot<D>,
    /// Serve the operator console's pages (feature `console`, ADR 0041).
    #[cfg_attr(not(feature = "console"), allow(dead_code))]
    console: bool,
}

impl<D> Clone for Gate<D> {
    fn clone(&self) -> Self {
        Gate {
            connection: self.connection.clone(),
            tls: self.tls.clone(),
            health: self.health.clone(),
            grpc_health: self.grpc_health.clone(),
            inner: self.inner.clone(),
            console: self.console,
        }
    }
}

impl<D> Gate<D> {
    fn new(health: Health, inner: Slot<D>, console: bool, tls: Option<Arc<ServerTls>>) -> Self {
        let connection = Connection {
            certificate_required: tls.as_ref().is_some_and(|t| t.requires_client_certificate()),
            ..Connection::default()
        };
        Gate { connection, tls, grpc_health: health.grpc_service(), health, inner, console }
    }
}

/// A database call before the store is open.
fn recovering() -> Error {
    Error::new(Code::Unavailable, "the server is recovering: the store is being opened; see /v1/health/ready")
}

impl<D, B> Service<http::Request<B>> for Gate<D>
where
    D: Served,
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let grpc = is_grpc(&request);
        let path = request.uri().path();
        if grpc && path.starts_with(GRPC_PREFIX) {
            let mut health = self.grpc_health.clone();
            return Box::pin(async move {
                let response = health.call(request).await?;
                Ok(response.map(Body::new))
            });
        }
        if !grpc && (path == LIVE_PATH || path == READY_PATH) {
            let (status, json) = if request.method() == http::Method::GET || request.method() == http::Method::HEAD {
                self.health.rest_answer(path)
            } else {
                // As the router answers a wrong method (an `Error` body)
                let error = r#"{"code":"invalid_argument","message":"the route doesn't take this method"}"#;
                (http::StatusCode::METHOD_NOT_ALLOWED, error.to_owned())
            };
            let mut response = http::Response::new(Body::new(json));
            *response.status_mut() = status;
            response
                .headers_mut()
                .insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
            response.headers_mut().insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-store"));
            return Box::pin(std::future::ready(Ok(response)));
        }
        #[cfg(feature = "console")]
        if !grpc && self.console && crate::console::serves(path) {
            return Box::pin(std::future::ready(Ok(crate::console::answer(request.method(), path))));
        }
        // Errors logged while the call runs (`internal`, `corrupt`, `io`)
        // carry its route
        let span = tracing::info_span!("request", path = %path);
        match self.inner.get() {
            Some(inner) => {
                let mut inner = inner.clone();
                let audit = inner.audit.clone();
                let credentials = match crate::auth::credentials(inner.mode, &request, grpc, &self.connection, &*audit)
                {
                    Ok(credentials) => credentials,
                    Err(e) => return Box::pin(std::future::ready(Ok(refused(grpc, e)))),
                };
                let operation = crate::auth::operation_of(&request, grpc);
                let connection = self.connection.clone();
                Box::pin(
                    async move {
                        let mut request = request;
                        let authenticated =
                            crate::auth::authenticate(&*inner.db, credentials, operation, &connection, &*audit).await;
                        match authenticated {
                            Ok(caller) => {
                                request.extensions_mut().insert(caller);
                                inner.call(request).await
                            }
                            Err(e) => Ok(refused(grpc, e)),
                        }
                    }
                    .instrument(span),
                )
            }
            None => Box::pin(std::future::ready(Ok(refused(grpc, recovering())))),
        }
    }
}

/// The answer to a request the gate refuses: a gRPC status, or a REST
/// `Error` body (an empty 404 without REST).
fn refused(grpc: bool, e: Error) -> http::Response<Body> {
    if grpc {
        return crate::status::to_status(&e).into_http::<tonic::body::Body>().map(Body::new);
    }
    #[cfg(feature = "rest")]
    {
        use axum::response::IntoResponse;
        crate::rest::Failure::from(e).into_response()
    }
    #[cfg(not(feature = "rest"))]
    {
        let _ = e;
        not_found()
    }
}

/// How a shutdown went ([`Server::serve`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drain {
    /// Every call finished within the drain.
    pub complete: bool,
    /// Connections that still had calls running at the end of the drain;
    /// they were closed, which cancelled their reads (commits that were
    /// accepted still run to the end on the database's workers).
    pub cancelled: usize,
}

/// Accept connections on `listener` and serve them with `gate` until
/// `until` completes; then close the listener, ask every connection to
/// finish (HTTP/2 GOAWAY, HTTP/1.1 close after the current request), and
/// let running calls finish until `drain()` completes. Connections still
/// open then are closed.
async fn accept<D, U, F, G>(listener: TcpListener, gate: Gate<D>, until: U, drain: F, stopping: impl FnOnce()) -> Drain
where
    D: Served,
    U: Future<Output = ()>,
    F: FnOnce() -> G,
    G: Future<Output = ()>,
{
    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    let mut connections = JoinSet::new();
    tokio::pin!(until);
    loop {
        tokio::select! {
            () = &mut until => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // Small answers shouldn't wait for Nagle's algorithm
                    let _ = stream.set_nodelay(true);
                    let mut gate = gate.clone();
                    gate.connection.client = Some(peer.ip());
                    let (builder, watcher) = (builder.clone(), graceful.watcher());
                    match gate.tls.as_ref().map(|tls| tls.acceptor()) {
                        None => {
                            let service = TowerToHyperService::new(gate);
                            let connection = watcher.watch(builder.serve_connection(TokioIo::new(stream), service).into_owned());
                            connections.spawn(async move {
                                // A connection error (a client that went
                                // away) concerns that connection only
                                let _ = connection.await;
                            });
                        }
                        Some(acceptor) => {
                            connections.spawn(async move {
                                let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                                    Ok(Ok(stream)) => stream,
                                    // A client that doesn't trust us, one we
                                    // don't trust, or not TLS at all: that
                                    // connection's concern
                                    Ok(Err(e)) => {
                                        tracing::debug!(client = %peer, error = %e, "TLS handshake failed");
                                        return;
                                    }
                                    Err(_) => {
                                        tracing::debug!(client = %peer, "TLS handshake timed out");
                                        return;
                                    }
                                };
                                let certificate = stream.get_ref().1.peer_certificates().and_then(|c| c.first());
                                gate.connection.certificate = ClientCertificate::of(certificate.map(|c| c.as_ref()));
                                gate.connection.tls = true;
                                let service = TowerToHyperService::new(gate);
                                let _ = watcher.watch(builder.serve_connection(TokioIo::new(stream), service).into_owned()).await;
                            });
                        }
                    }
                }
                Err(e) => {
                    // Out of file descriptors, a connection reset before
                    // it was accepted, ...: keep serving the others
                    tracing::warn!(error = %e, "accepting a connection failed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    stopping();
    let complete = tokio::select! {
        () = graceful.shutdown() => true,
        () = drain() => false,
    };
    while connections.try_join_next().is_some() {}
    let cancelled = connections.len();
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Drain { complete, cancelled }
}

impl<D: Served> Server<D> {
    /// Serve gRPC and REST on `listener` until `stop` completes; then shut
    /// down (ADR 0027): readiness turns off, the server goes on serving for
    /// the [`unready_delay`](Self::unready_delay) (so load balancers stop
    /// sending first), then closes the listener, sends every HTTP/2
    /// connection GOAWAY (new calls fail with `UNAVAILABLE`) and closes
    /// every HTTP/1.1 connection after its current request, and lets
    /// running calls finish until the future that `drain` returns
    /// completes (a timeout, a second signal).
    /// Connections still open then are closed. Returns once every
    /// connection is gone.
    ///
    /// The database stays open: close it after this returns (the binary
    /// takes it back with [`into_database`](Self::into_database) and closes
    /// the store, which flushes the WAL).
    ///
    /// Errors: only from the listener's local address; failed accepts are
    /// logged and retried.
    pub async fn serve<S, F, G>(&self, listener: TcpListener, stop: S, drain: F) -> io::Result<Drain>
    where
        S: Future<Output = ()>,
        F: FnOnce() -> G,
        G: Future<Output = ()>,
    {
        listener.local_addr()?;
        let slot = Arc::new(OnceLock::new());
        let _ = slot.set(self.http_service());
        if self.health.phase() == Phase::Recovering {
            self.health.set(Phase::Ready);
        }
        let gate = Gate::new(self.health.clone(), slot, self.console, self.tls.clone());
        let (health, delay) = (self.health.clone(), self.unready_delay);
        let until = async move {
            stop.await;
            unready(&health, delay).await;
        };
        // The change streams never finish on their own: end them as the
        // drain begins
        Ok(accept(listener, gate, until, drain, || {
            self.stopping.send_replace(true);
        })
        .await)
    }

    /// The database, once no call holds it any more: after
    /// [`serve`](Self::serve) returned, the calls of closed connections let
    /// go of it within moments. Waits at most `wait`; returns the shared
    /// database if something else still holds it then.
    pub async fn into_database(self, wait: Duration) -> Result<D, Arc<D>> {
        let deadline = Instant::now() + wait;
        let mut db = self.db;
        loop {
            match Arc::try_unwrap(db) {
                Ok(db) => return Ok(db),
                Err(shared) if Instant::now() >= deadline => return Err(shared),
                Err(shared) => {
                    db = shared;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }
}

/// Turn readiness off, then keep serving for `delay`.
async fn unready(health: &Health, delay: Duration) {
    health.set(Phase::Draining);
    tracing::info!(delay_ms = delay.as_millis() as u64, "shutting down: no longer ready");
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

/// How [`launch`] serves.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    /// [`Server::max_message_bytes`].
    pub max_message_bytes: usize,
    /// [`Server::unready_delay`].
    pub unready_delay: Duration,
    /// Serve the operator console (feature `console`; ignored without it).
    pub console: bool,
    /// [`Server::auth`].
    pub auth: AuthMode,
    /// [`Server::tls`]: serve TLS only (`None`: plaintext).
    pub tls: Option<Arc<ServerTls>>,
    /// What the logs call what is served (the data directory).
    pub name: String,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        LaunchOptions {
            max_message_bytes: crate::DEFAULT_MAX_MESSAGE_BYTES,
            unready_delay: Duration::ZERO,
            console: false,
            auth: AuthMode::default(),
            tls: None,
            name: String::new(),
        }
    }
}

/// What [`launch`] ended with.
pub struct Launched<D> {
    /// The server, to take the database back from
    /// ([`Server::into_database`]) and close it.
    pub server: Server<D>,
    pub drain: Drain,
}

/// Serve on `listener` from the start, and open the database meanwhile
/// (ADR 0040): health answers at once (live, not ready), database calls
/// fail with `unavailable` until `open` has returned on a blocking thread
/// (for the embedded store: recovery has finished), then the server is
/// ready and serves until `stop` completes, and shuts down as
/// [`Server::serve`] does. `health` is the server's health, in
/// [`Phase::Recovering`]; pass a clone you keep to watch it.
///
/// A `stop` during recovery waits for `open` to finish (recovery can't be
/// interrupted halfway), then shuts down without becoming ready, so the
/// caller still closes the store cleanly.
///
/// Errors: `open`'s (the listener is closed, nothing was served but
/// health), or the listener's local address.
pub async fn launch<D, O, S, F, G>(
    listener: TcpListener,
    health: Health,
    options: LaunchOptions,
    open: O,
    stop: S,
    drain: F,
) -> Result<Launched<D>, String>
where
    D: Served,
    O: FnOnce() -> Result<D, String> + Send + 'static,
    S: Future<Output = ()>,
    F: FnOnce() -> G,
    G: Future<Output = ()>,
{
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    health.set(Phase::Recovering);
    let scheme = if options.tls.is_some() { "TLS" } else { "plaintext" };
    tracing::info!(address = %address, name = %options.name, scheme, "listening; opening the store (recovery)");
    let slot: Slot<D> = Arc::new(OnceLock::new());
    let gate = Gate::new(health.clone(), slot.clone(), options.console, options.tls.clone());
    let opened: Arc<OnceLock<Result<Server<D>, String>>> = Arc::new(OnceLock::new());
    let until = {
        let (health, opened, options) = (health.clone(), opened.clone(), options.clone());
        async move {
            let started = Instant::now();
            let mut opening = tokio::task::spawn_blocking(open);
            tokio::pin!(stop);
            let mut stopped = false;
            let result = tokio::select! {
                r = &mut opening => r,
                () = &mut stop => {
                    stopped = true;
                    health.set(Phase::Draining);
                    tracing::info!("shutting down during recovery: waiting for the store to open, to close it");
                    opening.await
                }
            };
            let server = match result {
                Ok(Ok(db)) => Server::new(Arc::new(db))
                    .max_message_bytes(options.max_message_bytes)
                    .unready_delay(options.unready_delay)
                    .console(options.console)
                    .auth(options.auth)
                    .tls(options.tls.clone())
                    .with_health(health.clone()),
                Ok(Err(e)) => {
                    let _ = opened.set(Err(e));
                    return;
                }
                Err(e) => {
                    let _ = opened.set(Err(format!("opening the store failed: {}", e)));
                    return;
                }
            };
            let _ = slot.set(server.http_service());
            let stopping = server.stopping.clone();
            let _ = opened.set(Ok(server));
            if stopped {
                stopping.send_replace(true);
                return;
            }
            health.set(Phase::Ready);
            let took = started.elapsed();
            tracing::info!(
                address = %address,
                name = %options.name,
                recovery_ms = took.as_millis() as u64,
                "serving {} on {}",
                options.name,
                address
            );
            stop.await;
            unready(&health, options.unready_delay).await;
        }
    };
    let stopping = {
        let opened = opened.clone();
        move || {
            if let Some(Ok(server)) = opened.get() {
                server.stopping.send_replace(true);
            }
        }
    };
    let drain = accept(listener, gate, until, drain, stopping).await;
    let opened = Arc::try_unwrap(opened).map_err(|_| "the server is still shared".to_owned())?;
    match opened.into_inner() {
        Some(Ok(server)) => Ok(Launched { server, drain }),
        Some(Err(e)) => Err(e),
        None => Err("the store wasn't opened".to_owned()),
    }
}
