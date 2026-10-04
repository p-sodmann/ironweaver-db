//! [`Server`]: the `DatabaseService` of `proto/ironweaver_db/v1` over any
//! [`Database`]. Every RPC is its operation in [`crate::ops`] (one trait
//! call, design rule 8) with the `grpc-timeout` header as its deadline; a
//! failure is the error's status ([`crate::status`]).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use iwdb_query::{Database, Error};
use tokio::sync::watch;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use crate::convert::watch_response;
use crate::health::{Health, Phase};
use crate::ops;
use crate::proto as pb;
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
}

impl<D: Database + 'static> Server<D> {
    pub fn new(db: Arc<D>) -> Self {
        Server {
            db,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            stopping: watch::Sender::new(false),
            health: Health::new(Phase::Ready),
            unready_delay: Duration::ZERO,
            console: false,
        }
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
        DatabaseServiceServer::from_arc(Arc::new(Adapter { db: self.db.clone(), stopping: self.stopping.subscribe() }))
            .max_decoding_message_size(self.max_message_bytes)
            .max_encoding_message_size(self.max_message_bytes)
    }

    /// The REST routes (ADR 0030), with request bodies up to the message
    /// size limit.
    #[cfg(feature = "rest")]
    pub fn rest_router(&self) -> axum::Router {
        crate::rest::router(self.db.clone(), self.max_message_bytes, self.stopping.subscribe())
    }

    /// gRPC and REST as one service, as [`serve`](Self::serve) serves them.
    #[cfg(feature = "rest")]
    pub(crate) fn http_service(&self) -> crate::serve::Dispatch<D> {
        crate::serve::Dispatch::new(self.service(), self.rest_router())
    }

    /// gRPC alone, as [`serve`](Self::serve) serves it without `rest`.
    #[cfg(not(feature = "rest"))]
    pub(crate) fn http_service(&self) -> crate::serve::Dispatch<D> {
        crate::serve::Dispatch::new(self.service())
    }
}

/// The handlers of `DatabaseService` over a database ([`Server::service`]).
pub struct Adapter<D> {
    db: Arc<D>,
    stopping: watch::Receiver<bool>,
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
impl<D: Database + 'static> DatabaseService for Adapter<D> {
    async fn commit(&self, request: Request<pb::CommitRequest>) -> Res<pb::CommitResponse> {
        Ok(Response::new(ops::commit(&*self.db, request.into_inner()).await.map_err(fail)?))
    }

    async fn commit_catalog(&self, request: Request<pb::CommitCatalogRequest>) -> Res<pb::CommitCatalogResponse> {
        Ok(Response::new(ops::commit_catalog(&*self.db, request.into_inner()).await.map_err(fail)?))
    }

    async fn wait_for_seq(&self, request: Request<pb::WaitForSeqRequest>) -> Res<pb::WaitForSeqResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::wait_for_seq(&*self.db, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type GetNodesStream = Chunks<pb::GetNodesResponse>;

    async fn get_nodes(&self, request: Request<pb::GetNodesRequest>) -> Res<Self::GetNodesStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(ops::get_nodes(&*self.db, request.into_inner(), deadline).await.map_err(fail)?)))
    }

    type GetEdgesStream = Chunks<pb::GetEdgesResponse>;

    async fn get_edges(&self, request: Request<pb::GetEdgesRequest>) -> Res<Self::GetEdgesStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(ops::get_edges(&*self.db, request.into_inner(), deadline).await.map_err(fail)?)))
    }

    type FindStream = Chunks<pb::FindResponse>;

    async fn find(&self, request: Request<pb::FindRequest>) -> Res<Self::FindStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(ops::find(&*self.db, request.into_inner(), deadline).await.map_err(fail)?)))
    }

    async fn explain(&self, request: Request<pb::ExplainRequest>) -> Res<pb::ExplainResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::explain(&*self.db, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type NeighbourhoodStream = Chunks<pb::NeighbourhoodResponse>;

    async fn neighbourhood(&self, request: Request<pb::NeighbourhoodRequest>) -> Res<Self::NeighbourhoodStream> {
        let deadline = grpc_timeout(request.metadata());
        let chunks = ops::neighbourhood(&*self.db, request.into_inner(), deadline).await.map_err(fail)?;
        Ok(Response::new(stream(chunks)))
    }

    type TraverseStream = Chunks<pb::TraverseResponse>;

    async fn traverse(&self, request: Request<pb::TraverseRequest>) -> Res<Self::TraverseStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(ops::traverse(&*self.db, request.into_inner(), deadline).await.map_err(fail)?)))
    }

    async fn shortest_path(&self, request: Request<pb::ShortestPathRequest>) -> Res<pb::ShortestPathResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::shortest_path(&*self.db, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type RandomWalksStream = Chunks<pb::RandomWalksResponse>;

    async fn random_walks(&self, request: Request<pb::RandomWalksRequest>) -> Res<Self::RandomWalksStream> {
        let deadline = grpc_timeout(request.metadata());
        let chunks = ops::random_walks(&*self.db, request.into_inner(), deadline).await.map_err(fail)?;
        Ok(Response::new(stream(chunks)))
    }

    type SubgraphStream = Chunks<pb::SubgraphResponse>;

    async fn subgraph(&self, request: Request<pb::SubgraphRequest>) -> Res<Self::SubgraphStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(ops::subgraph(&*self.db, request.into_inner(), deadline).await.map_err(fail)?)))
    }

    type MatchPatternStream = Chunks<pb::MatchPatternResponse>;

    async fn match_pattern(&self, request: Request<pb::MatchPatternRequest>) -> Res<Self::MatchPatternStream> {
        let deadline = grpc_timeout(request.metadata());
        let chunks = ops::match_pattern(&*self.db, request.into_inner(), deadline).await.map_err(fail)?;
        Ok(Response::new(stream(chunks)))
    }

    async fn get_changes(&self, request: Request<pb::GetChangesRequest>) -> Res<pb::GetChangesResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::get_changes(&*self.db, request.into_inner(), deadline).await.map_err(fail)?))
    }

    type WatchStream = Pin<Box<dyn Stream<Item = Result<pb::WatchResponse, Status>> + Send>>;

    /// The change stream, followed until the client cancels, an error, or
    /// shutdown (ADR 0031).
    async fn watch(&self, request: Request<pb::WatchRequest>) -> Res<Self::WatchStream> {
        let deadline = grpc_timeout(request.metadata());
        let batches = ops::follow(self.db.clone(), request.into_inner(), deadline, self.stopping.clone());
        let stream = ReceiverStream::new(batches).map(|batch| batch.map(watch_response).map_err(fail));
        Ok(Response::new(Box::pin(stream)))
    }

    type AnalyzeStream = Chunks<pb::AnalyzeResponse>;

    async fn analyze(&self, request: Request<pb::AnalyzeRequest>) -> Res<Self::AnalyzeStream> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(stream(ops::analyze(&*self.db, request.into_inner(), deadline).await.map_err(fail)?)))
    }

    async fn get_catalog(&self, request: Request<pb::GetCatalogRequest>) -> Res<pb::GetCatalogResponse> {
        let deadline = grpc_timeout(request.metadata());
        Ok(Response::new(ops::get_catalog(&*self.db, request.into_inner(), deadline).await.map_err(fail)?))
    }

    async fn get_namespace_status(
        &self,
        request: Request<pb::GetNamespaceStatusRequest>,
    ) -> Res<pb::GetNamespaceStatusResponse> {
        Ok(Response::new(ops::get_namespace_status(&*self.db, request.into_inner()).await.map_err(fail)?))
    }

    async fn list_namespaces(&self, _request: Request<pb::ListNamespacesRequest>) -> Res<pb::ListNamespacesResponse> {
        Ok(Response::new(ops::list_namespaces(&*self.db).await.map_err(fail)?))
    }

    async fn create_namespace(&self, request: Request<pb::CreateNamespaceRequest>) -> Res<pb::CreateNamespaceResponse> {
        Ok(Response::new(ops::create_namespace(&*self.db, request.into_inner()).await.map_err(fail)?))
    }

    async fn drop_namespace(&self, request: Request<pb::DropNamespaceRequest>) -> Res<pb::DropNamespaceResponse> {
        Ok(Response::new(ops::drop_namespace(&*self.db, request.into_inner()).await.map_err(fail)?))
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
