//! [`Server`]: the `DatabaseService` of `proto/ironweaver_db/v1` over any
//! [`Database`]. Every RPC is its operation in [`crate::ops`] (one trait
//! call, design rule 8) with the `grpc-timeout` header as its deadline; a
//! failure is the error's status ([`crate::status`]).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use iwdb_query::audit::AuditSink;
use iwdb_query::{Authorized, Error};
use tokio::sync::watch;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use crate::auth::{AuthMode, Caller, Served, audit_of, authorized};
use crate::convert::watch_response;
use crate::health::{Health, Phase};
use crate::ops;
use crate::proto as pb;
use crate::proto::admin_service_server::{AdminService, AdminServiceServer};
use crate::proto::auth_service_server::{AuthService, AuthServiceServer};
use crate::proto::database_service_server::{DatabaseService, DatabaseServiceServer};
use crate::status::to_status;

/// The default limit of a request's and an answer message's size: a commit
/// as large as the WAL's largest record (64 MiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 << 20;

/// The gRPC server of a [`Database`]: build it with [`Server::new`], then
/// [`serve`](Server::serve) it on a listener, or take its
/// [`service`](Server::service) to serve it together with other services.
///
/// Limits, timeouts and errors are the database's: a request's limits pass
/// through, so the database's `LimitConfig` gives the defaults and caps
/// (design rule 5). Reads honour the smaller of `grpc-timeout` and the
/// request's `timeout_ms` (ADR 0026). A client that goes away drops its
/// call, which cancels the read (ADR 0025).
pub struct Server<D> {
    pub(crate) db: Arc<D>,
    max_message_bytes: usize,
    /// Turns true when [`serve`](Self::serve) starts shutting down: the
    /// change streams end then (ADR 0031).
    pub(crate) stopping: watch::Sender<bool>,
    /// Where the server is in its life (ADR 0040).
    pub(crate) health: Health,
    pub(crate) unready_delay: Duration,
    /// Serve the operator console (feature `console`, ADR 0041).
    pub(crate) console: bool,
    /// Check credentials (step 15a).
    pub(crate) auth: AuthMode,
    /// Serve TLS only (step 15b).
    pub(crate) tls: Option<Arc<crate::tls::ServerTls>>,
    /// Where audit entries go (step 15c).
    pub(crate) audit: Arc<dyn AuditSink>,
}

impl<D: Served> Server<D> {
    pub fn new(db: Arc<D>) -> Self {
        Server {
            db,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            stopping: watch::Sender::new(false),
            health: Health::new(Phase::Ready),
            unready_delay: Duration::ZERO,
            console: false,
            auth: AuthMode::default(),
            tls: None,
            audit: Arc::new(crate::audit::LogAudit),
        }
    }

    /// Where audit entries go (step 15c, ADR 0049): by default
    /// [`LogAudit`](crate::audit::LogAudit), the log's `iwdb::audit`
    /// target.
    pub fn audit(mut self, sink: Arc<dyn AuditSink>) -> Self {
        self.audit = sink;
        self
    }

    /// Serve TLS only, with `tls` (step 15b, ADR 0048); `None` (the
    /// default for a `Server` made in code): plaintext. The binary turns it
    /// on per `[tls] enabled`. Reloading `tls` changes the certificate of
    /// new connections.
    pub fn tls(mut self, tls: Option<Arc<crate::tls::ServerTls>>) -> Self {
        self.tls = tls;
        self
    }

    /// Whether [`serve`](Self::serve) speaks TLS.
    pub fn serves_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// Check credentials (step 15a): every call but login and health needs
    /// a token, and runs with its principal's roles. Off by default for a
    /// `Server` made in code; the binary turns it on per `[auth] enabled`.
    pub fn auth(mut self, mode: AuthMode) -> Self {
        self.auth = mode;
        self
    }

    /// How long the server goes on serving after a shutdown began, with
    /// readiness off, before it drains (default 0): time for load
    /// balancers to see it unready and stop sending (ADR 0040).
    pub fn unready_delay(mut self, delay: Duration) -> Self {
        self.unready_delay = delay;
        self
    }

    /// Serve the operator console's pages at `/console/` (ADR 0041). Only
    /// with the `console` feature; without it this does nothing.
    pub fn console(mut self, on: bool) -> Self {
        self.console = on;
        self
    }

    /// Report into `health` instead of a health of its own (the server is
    /// ready when [`serve`](Self::serve) starts).
    pub fn with_health(mut self, health: Health) -> Self {
        self.health = health;
        self
    }

    /// The server's health.
    pub fn health(&self) -> &Health {
        &self.health
    }

    /// Requests and answer messages above this size fail (default
    /// [`DEFAULT_MAX_MESSAGE_BYTES`]); over REST, request bodies above it.
    /// Streamed answers are sent in chunks of about
    /// [`CHUNK_BYTES`](crate::CHUNK_BYTES).
    pub fn max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes;
        self
    }

    pub fn database(&self) -> &Arc<D> {
        &self.db
    }

    /// The tonic service (a tower `Service` of HTTP requests), to serve
    /// together with other services.
    pub fn service(&self) -> DatabaseServiceServer<Adapter<D>> {
        DatabaseServiceServer::from_arc(self.adapter())
            .max_decoding_message_size(self.max_message_bytes)
            .max_encoding_message_size(self.max_message_bytes)
    }

    /// The tonic service of `AdminService` (step 16c).
    pub fn admin_service(&self) -> AdminServiceServer<Adapter<D>> {
        AdminServiceServer::from_arc(self.adapter())
            .max_decoding_message_size(self.max_message_bytes)
            .max_encoding_message_size(self.max_message_bytes)
    }

    /// The tonic service of `AuthService` (step 15a).
    pub fn auth_service(&self) -> AuthServiceServer<Adapter<D>> {
        AuthServiceServer::from_arc(self.adapter())
            .max_decoding_message_size(self.max_message_bytes)
            .max_encoding_message_size(self.max_message_bytes)
    }

    fn adapter(&self) -> Arc<Adapter<D>> {
        Arc::new(Adapter {
            db: self.db.clone(),
            stopping: self.stopping.subscribe(),
            mode: self.auth,
            audit: self.audit.clone(),
        })
    }

    /// The REST routes (ADR 0030), with request bodies up to the message
    /// size limit.
    #[cfg(feature = "rest")]
    pub fn rest_router(&self) -> axum::Router {
        crate::rest::router(
            self.db.clone(),
            self.max_message_bytes,
            self.stopping.subscribe(),
            self.auth,
            self.audit.clone(),
        )
    }

    /// gRPC and REST as one service, as [`serve`](Self::serve) serves them.
    #[cfg(feature = "rest")]
    pub(crate) fn http_service(&self) -> crate::serve::Dispatch<D> {
        crate::serve::Dispatch::new(
            self.db.clone(),
            self.auth,
            self.audit.clone(),
            self.service(),
            self.auth_service(),
            self.admin_service(),
            self.rest_router(),
        )
    }

    /// gRPC alone, as [`serve`](Self::serve) serves it without `rest`.
    #[cfg(not(feature = "rest"))]
    pub(crate) fn http_service(&self) -> crate::serve::Dispatch<D> {
        crate::serve::Dispatch::new(
            self.db.clone(),
            self.auth,
            self.audit.clone(),
            self.service(),
            self.auth_service(),
            self.admin_service(),
        )
    }
}

/// The handlers of `DatabaseService`, `AuthService` and `AdminService` over a database
/// ([`Server::service`]). Every call runs through [`Authorized`], built from
/// the [`Caller`] the gate attached (design rule 8, ADR 0045).
pub struct Adapter<D> {
    db: Arc<D>,
    stopping: watch::Receiver<bool>,
    mode: AuthMode,
    audit: Arc<dyn AuditSink>,
}

impl<D: iwdb_query::Admin> Adapter<D> {
    /// The database as the request's caller may use it.
    fn db<T>(&self, request: &Request<T>) -> Result<Authorized<D>, Status> {
        authorized(&self.db, self.mode, request.extensions().get::<Caller>(), &self.audit).map_err(fail)
    }

    fn caller<T>(request: &Request<T>) -> Option<Caller> {
        request.extensions().get::<Caller>().cloned()
    }
}

type Res<T> = Result<Response<T>, Status>;

/// A streamed answer: its chunks, all built before the first is sent.
type Chunks<T> = tokio_stream::Iter<std::vec::IntoIter<Result<T, Status>>>;

fn fail(e: Error) -> Status {
    to_status(&e)
}

/// The client's deadline from the `grpc-timeout` header (gRPC over HTTP/2:
/// at most 8 digits and a unit). An invalid header is ignored.
fn grpc_timeout(metadata: &MetadataMap) -> Option<Duration> {
    let text = metadata.get("grpc-timeout")?.to_str().ok()?;
    let (digits, unit) = text.split_at(text.len().checked_sub(1)?);
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n * 3600),
        "M" => Duration::from_secs(n * 60),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// A streamed answer: its chunks (ADR 0025).
fn stream<T>(chunks: Vec<T>) -> Chunks<T> {
    tokio_stream::iter(chunks.into_iter().map(Ok).collect::<Vec<_>>())
}

#[tonic::async_trait]
impl<D: Served> DatabaseService for Adapter<D> {
    async fn commit(&self, request: Request<pb::CommitRequest>) -> Res<pb::CommitResponse> {
        Ok(Response::new(ops::commit(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn commit_catalog(&self, request: Request<pb::CommitCatalogRequest>) -> Res<pb::CommitCatalogResponse> {
        Ok(Response::new(ops::commit_catalog(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn wait_for_seq(&self, request: Request<pb::WaitForSeqRequest>) -> Res<pb::WaitForSeqResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::wait_for_seq(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type GetNodesStream = Chunks<pb::GetNodesResponse>;

    async fn get_nodes(&self, request: Request<pb::GetNodesRequest>) -> Res<Self::GetNodesStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(
            ops::get_nodes(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?,
        )))
    }

    type GetEdgesStream = Chunks<pb::GetEdgesResponse>;

    async fn get_edges(&self, request: Request<pb::GetEdgesRequest>) -> Res<Self::GetEdgesStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(
            ops::get_edges(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?,
        )))
    }

    type FindStream = Chunks<pb::FindResponse>;

    async fn find(&self, request: Request<pb::FindRequest>) -> Res<Self::FindStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(ops::find(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?)))
    }

    async fn explain(&self, request: Request<pb::ExplainRequest>) -> Res<pb::ExplainResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::explain(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type NeighbourhoodStream = Chunks<pb::NeighbourhoodResponse>;

    async fn neighbourhood(&self, request: Request<pb::NeighbourhoodRequest>) -> Res<Self::NeighbourhoodStream> {
        let deadline = grpc_timeout(request.metadata());
        let chunks = ops::neighbourhood(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?;
        Ok(Response::new(stream(chunks)))
    }

    type TraverseStream = Chunks<pb::TraverseResponse>;

    async fn traverse(&self, request: Request<pb::TraverseRequest>) -> Res<Self::TraverseStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(
            ops::traverse(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?,
        )))
    }

    async fn shortest_path(&self, request: Request<pb::ShortestPathRequest>) -> Res<pb::ShortestPathResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::shortest_path(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type RandomWalksStream = Chunks<pb::RandomWalksResponse>;

    async fn random_walks(&self, request: Request<pb::RandomWalksRequest>) -> Res<Self::RandomWalksStream> {
        let deadline = grpc_timeout(request.metadata());
        let chunks = ops::random_walks(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?;
        Ok(Response::new(stream(chunks)))
    }

    type SubgraphStream = Chunks<pb::SubgraphResponse>;

    async fn subgraph(&self, request: Request<pb::SubgraphRequest>) -> Res<Self::SubgraphStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(
            ops::subgraph(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?,
        )))
    }

    type MatchPatternStream = Chunks<pb::MatchPatternResponse>;

    async fn match_pattern(&self, request: Request<pb::MatchPatternRequest>) -> Res<Self::MatchPatternStream> {
        let deadline = grpc_timeout(request.metadata());
        let chunks = ops::match_pattern(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?;
        Ok(Response::new(stream(chunks)))
    }

    async fn get_changes(&self, request: Request<pb::GetChangesRequest>) -> Res<pb::GetChangesResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::get_changes(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type WatchStream = Pin<Box<dyn Stream<Item = Result<pb::WatchResponse, Status>> + Send>>;

    /// The change stream, followed until the client cancels, an error, or
    /// shutdown (ADR 0031).
    async fn watch(&self, request: Request<pb::WatchRequest>) -> Res<Self::WatchStream> {
        let deadline = grpc_timeout(request.metadata());
        let db = Arc::new(self.db(&request)?);
        let batches = ops::follow(db, request.into_inner(), deadline, self.stopping.clone());
        let stream = ReceiverStream::new(batches).map(|batch| batch.map(watch_response).map_err(fail));
        Ok(Response::new(Box::pin(stream)))
    }

    type AnalyzeStream = Chunks<pb::AnalyzeResponse>;

    async fn analyze(&self, request: Request<pb::AnalyzeRequest>) -> Res<Self::AnalyzeStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(
            ops::analyze(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?,
        )))
    }

    async fn get_catalog(&self, request: Request<pb::GetCatalogRequest>) -> Res<pb::GetCatalogResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::get_catalog(&self.db(&request)?, request.into_inner(), deadline).await.map_err(fail)?))
    }

    async fn get_namespace_status(
        &self,
        request: Request<pb::GetNamespaceStatusRequest>,
    ) -> Res<pb::GetNamespaceStatusResponse> {
        Ok(Response::new(ops::get_namespace_status(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn list_namespaces(&self, request: Request<pb::ListNamespacesRequest>) -> Res<pb::ListNamespacesResponse> {
        Ok(Response::new(ops::list_namespaces(&self.db(&request)?).await.map_err(fail)?))
    }

    async fn create_namespace(&self, request: Request<pb::CreateNamespaceRequest>) -> Res<pb::CreateNamespaceResponse> {
        Ok(Response::new(ops::create_namespace(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn drop_namespace(&self, request: Request<pb::DropNamespaceRequest>) -> Res<pb::DropNamespaceResponse> {
        Ok(Response::new(ops::drop_namespace(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }
}

#[tonic::async_trait]
impl<D: Served> AuthService for Adapter<D> {
    async fn login(&self, request: Request<pb::LoginRequest>) -> Res<pb::LoginResponse> {
        let audit = audit_of(request.extensions().get::<Caller>(), &self.audit);
        let r = request.into_inner();
        if r.cookie {
            return Err(fail(Error::invalid("a session cookie is for REST only: leave `cookie` out")));
        }
        Ok(Response::new(ops::login(&*self.db, r, &audit).await.map_err(fail)?.0))
    }

    async fn logout(&self, request: Request<pb::LogoutRequest>) -> Res<pb::LogoutResponse> {
        let db = self.db(&request)?;
        let token = Self::caller(&request).and_then(|c| c.token);
        Ok(Response::new(ops::logout(&db, token.as_ref()).await.map_err(fail)?))
    }

    async fn who_am_i(&self, request: Request<pb::WhoAmIRequest>) -> Res<pb::WhoAmIResponse> {
        Ok(Response::new(ops::who_am_i(&self.db(&request)?, self.mode)))
    }

    async fn list_users(&self, request: Request<pb::ListUsersRequest>) -> Res<pb::ListUsersResponse> {
        Ok(Response::new(ops::list_users(&self.db(&request)?).await.map_err(fail)?))
    }

    async fn create_user(&self, request: Request<pb::CreateUserRequest>) -> Res<pb::CreateUserResponse> {
        Ok(Response::new(ops::create_user(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn set_password(&self, request: Request<pb::SetPasswordRequest>) -> Res<pb::SetPasswordResponse> {
        Ok(Response::new(ops::set_password(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn delete_user(&self, request: Request<pb::DeleteUserRequest>) -> Res<pb::DeleteUserResponse> {
        Ok(Response::new(ops::delete_user(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn set_admin(&self, request: Request<pb::SetAdminRequest>) -> Res<pb::SetAdminResponse> {
        Ok(Response::new(ops::set_admin(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn grant(&self, request: Request<pb::GrantRequest>) -> Res<pb::GrantResponse> {
        Ok(Response::new(ops::grant(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn revoke(&self, request: Request<pb::RevokeRequest>) -> Res<pb::RevokeResponse> {
        Ok(Response::new(ops::revoke(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn create_token(&self, request: Request<pb::CreateTokenRequest>) -> Res<pb::CreateTokenResponse> {
        Ok(Response::new(ops::create_token(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn revoke_token(&self, request: Request<pb::RevokeTokenRequest>) -> Res<pb::RevokeTokenResponse> {
        Ok(Response::new(ops::revoke_token(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn list_tokens(&self, request: Request<pb::ListTokensRequest>) -> Res<pb::ListTokensResponse> {
        Ok(Response::new(ops::list_tokens(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }
}

#[tonic::async_trait]
impl<D: Served> AdminService for Adapter<D> {
    async fn get_server_status(
        &self,
        request: Request<pb::GetServerStatusRequest>,
    ) -> Res<pb::GetServerStatusResponse> {
        Ok(Response::new(ops::get_server_status(&self.db(&request)?).await.map_err(fail)?))
    }

    async fn list_requests(&self, request: Request<pb::ListRequestsRequest>) -> Res<pb::ListRequestsResponse> {
        Ok(Response::new(ops::list_requests(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn cancel_request(&self, request: Request<pb::CancelRequestRequest>) -> Res<pb::CancelRequestResponse> {
        Ok(Response::new(ops::cancel_request(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }

    async fn list_consumers(&self, request: Request<pb::ListConsumersRequest>) -> Res<pb::ListConsumersResponse> {
        Ok(Response::new(ops::list_consumers(&self.db(&request)?).await.map_err(fail)?))
    }

    async fn get_metrics(&self, request: Request<pb::GetMetricsRequest>) -> Res<pb::GetMetricsResponse> {
        Ok(Response::new(ops::get_metrics(&self.db(&request)?).await.map_err(fail)?))
    }

    async fn get_log(&self, request: Request<pb::GetLogRequest>) -> Res<pb::GetLogResponse> {
        Ok(Response::new(ops::get_log(&self.db(&request)?, request.into_inner()).await.map_err(fail)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_timeout_headers_parse() {
        let parse = |text: &str| {
            let mut m = MetadataMap::new();
            m.insert("grpc-timeout", text.parse().expect("ascii"));
            grpc_timeout(&m)
        };
        assert_eq!(parse("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse("2M"), Some(Duration::from_secs(120)));
        assert_eq!(parse("3S"), Some(Duration::from_secs(3)));
        assert_eq!(parse("250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse("99999999u"), Some(Duration::from_micros(99_999_999)));
        assert_eq!(parse("0n"), Some(Duration::ZERO));
        for bad in ["", "S", "100", "123456789S", "1x", "-1S", "1.5S"] {
            assert_eq!(parse(bad), None, "{:?}", bad);
        }
        assert_eq!(grpc_timeout(&MetadataMap::new()), None);
    }
}
