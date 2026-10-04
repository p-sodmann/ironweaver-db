//! [`Remote`]: the [`Database`] trait over gRPC (feature `client`, ADR
//! 0024).
//!
//! Every call runs as a task on a tokio runtime the client owns (or one it
//! was given), and the future a method returns waits for that task, so any
//! executor can drive it: `iwdb_query::exec::block_on`, tokio, Python.
//! Dropping the future aborts the task, which resets the call; the server
//! then cancels the read (ADR 0025). A commit that was sent may still be
//! applied: retry it with its idempotency key (ADR 0026).
//!
//! Errors carry the server's code (`iwdb-code`); a transport failure (no
//! connection, a message over the size limit) maps to the closest code, a
//! lost connection to `unavailable`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use ironweaver_core::EdgeId;
use iwdb_engine::catalog::NamespaceCatalog;
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, Mutation};
use iwdb_query::read::Explain;
use iwdb_query::{
    AnalyticsRequest, Answer, Changes, ChangesRequest, CommitOptions, Database, Edge, Error, ExplainRequest,
    FindRequest, JobResult, MatchRequest, MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Path, PathRequest,
    QueryOptions, Subgraph, SubgraphRequest, TraverseRequest, WalkRequest,
};
use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use tonic::Streaming;
use tonic::transport::{Channel, Endpoint};

use crate::DEFAULT_MAX_MESSAGE_BYTES;
use crate::convert::*;
use crate::proto as pb;
use crate::proto::database_service_client::DatabaseServiceClient;
use crate::status::from_status;

#[cfg(feature = "rest")]
mod rest;
#[cfg(feature = "rest")]
pub use rest::RestRemote;

type Client = DatabaseServiceClient<Channel>;

/// A database served by an `iwdb-server`, as a [`Database`].
pub struct Remote {
    client: Client,
    handle: Handle,
    /// The runtime this client started, if it started one.
    runtime: Option<Runtime>,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").field("own_runtime", &self.runtime.is_some()).finish_non_exhaustive()
    }
}

impl Remote {
    /// A client of the server at `endpoint` (`http://host:port`), on a
    /// runtime of its own (two threads). It connects on the first call (and
    /// again after a lost connection); a server that isn't there makes the
    /// calls fail with `unavailable`. Errors: `invalid_argument` for an
    /// invalid endpoint, `internal` if the runtime can't start.
    pub fn connect(endpoint: &str) -> Result<Remote, Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("iwdb-client")
            .enable_all()
            .build()
            .map_err(|e| Error::internal(format!("can't start the client's runtime: {}", e)))?;
        let mut remote = Remote::connect_on(runtime.handle().clone(), endpoint)?;
        remote.runtime = Some(runtime);
        Ok(remote)
    }

    /// The same, running its calls on `handle`'s runtime.
    pub fn connect_on(handle: Handle, endpoint: &str) -> Result<Remote, Error> {
        let endpoint = Endpoint::from_shared(endpoint.to_owned())
            .map_err(|e| Error::invalid(format!("invalid endpoint '{}': {}", endpoint, e)))?
            .tcp_nodelay(true);
        let channel = {
            let _runtime = handle.enter();
            endpoint.connect_lazy()
        };
        let client = DatabaseServiceClient::new(channel)
            .max_decoding_message_size(DEFAULT_MAX_MESSAGE_BYTES)
            .max_encoding_message_size(DEFAULT_MAX_MESSAGE_BYTES);
        Ok(Remote { client, handle, runtime: None })
    }

    /// Run `f` with a client on the runtime.
    fn call<T, F>(&self, f: impl FnOnce(Client) -> F) -> Call<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, Error>> + Send + 'static,
    {
        Call(self.handle.spawn(f(self.client.clone())))
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // Dropping a runtime blocks, which panics inside another runtime
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// A call running on the client's runtime; dropping it aborts the call.
struct Call<T>(JoinHandle<Result<T, Error>>);

impl<T> Future for Call<T> {
    type Output = Result<T, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(e)) if e.is_cancelled() => Poll::Ready(Err(Error::unavailable("the client shut down"))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::internal(format!("the client's call failed: {}", e)))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for Call<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn status(s: tonic::Status) -> Error {
    from_status(&s)
}

/// An answer the server shouldn't have sent.
fn bad_answer(e: Error) -> Error {
    Error::internal(format!("invalid answer from the server: {}", e.message()))
}

/// Read a streamed answer: every chunk's items through `take`, and the
/// meta of the last chunk. A stream without meta was cut off.
async fn collect<R>(
    mut stream: Streaming<R>,
    mut take: impl FnMut(R) -> Result<Option<pb::AnswerMeta>, Error>,
) -> Result<pb::AnswerMeta, Error> {
    let mut meta = None;
    while let Some(chunk) = stream.message().await.map_err(status)? {
        if meta.is_some() {
            return Err(bad_answer(Error::invalid("a chunk after the last one")));
        }
        meta = take(chunk)?;
    }
    meta.ok_or_else(|| Error::unavailable("the answer was cut off before its end"))
}

fn options(o: &QueryOptions) -> Option<pb::QueryOptions> {
    Some(options_to_pb(o))
}

impl Database for Remote {
    fn commit(
        &self,
        namespace: &str,
        mutations: Vec<Mutation>,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::CommitRequest {
                namespace,
                mutations: mutations_to_pb(&mutations)?,
                options: Some(commit_options_to_pb(&options)),
            };
            let response = client.commit(request).await.map_err(status)?.into_inner();
            commit_result_from_pb(response.result).map_err(bad_answer)
        })
    }

    fn commit_catalog(
        &self,
        namespace: &str,
        change: CatalogChange,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::CommitCatalogRequest {
                namespace,
                change: Some(catalog_change_to_pb(&change)),
                options: Some(commit_options_to_pb(&options)),
            };
            let response = client.commit_catalog(request).await.map_err(status)?.into_inner();
            commit_result_from_pb(response.result).map_err(bad_answer)
        })
    }

    fn wait_for_seq(
        &self,
        namespace: &str,
        seq: u64,
        o: QueryOptions,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::WaitForSeqRequest { namespace, seq, options: options(&o) };
            Ok(client.wait_for_seq(request).await.map_err(status)?.into_inner().seq)
        })
    }

    fn get_nodes(
        &self,
        namespace: &str,
        ids: Vec<String>,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Node>>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::GetNodesRequest { namespace, ids, options: options(&o) };
            let stream = client.get_nodes(request).await.map_err(status)?.into_inner();
            let mut nodes = Vec::new();
            let meta = collect(stream, |chunk| {
                nodes.extend(maybe_nodes_from_pb(chunk.nodes).map_err(bad_answer)?);
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(nodes, meta))
        })
    }

    fn get_edges(
        &self,
        namespace: &str,
        ids: Vec<EdgeId>,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Edge>>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request =
                pb::GetEdgesRequest { namespace, ids: ids.into_iter().map(|e| e.0).collect(), options: options(&o) };
            let stream = client.get_edges(request).await.map_err(status)?.into_inner();
            let mut edges = Vec::new();
            let meta = collect(stream, |chunk| {
                edges.extend(maybe_edges_from_pb(chunk.edges).map_err(bad_answer)?);
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(edges, meta))
        })
    }

    fn find(
        &self,
        namespace: &str,
        request: FindRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::FindRequest { namespace, filter: Some(find_to_pb(&request)?), options: options(&o) };
            let stream = client.find(request).await.map_err(status)?.into_inner();
            let mut nodes = Vec::new();
            let meta = collect(stream, |chunk| {
                nodes.extend(nodes_from_pb(chunk.nodes).map_err(bad_answer)?);
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(nodes, meta))
        })
    }

    fn changes(
        &self,
        namespace: &str,
        request: ChangesRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Changes>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::GetChangesRequest {
                namespace,
                from_seq: request.from_seq,
                wait: request.wait,
                options: options(&o),
            };
            let response = client.get_changes(request).await.map_err(status)?.into_inner();
            changes_from_pb(response).map_err(bad_answer)
        })
    }

    fn explain(
        &self,
        namespace: &str,
        request: ExplainRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Explain>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::ExplainRequest {
                namespace,
                filter: Some(expr_to_pb(&request.filter)?),
                analyze: request.analyze,
                options: options(&o),
            };
            let response = client.explain(request).await.map_err(status)?.into_inner();
            let explain = explain_answer_from_pb(response.explain).map_err(bad_answer)?;
            Ok(answer_from_pb(explain, response.meta.unwrap_or_default()))
        })
    }

    fn neighbourhood(
        &self,
        namespace: &str,
        request: NeighbourhoodRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = neighbourhood_to_pb(&namespace, &request, &o)?;
            let stream = client.neighbourhood(request).await.map_err(status)?.into_inner();
            let mut nodes = Vec::new();
            let meta = collect(stream, |chunk| {
                nodes.extend(nodes_from_pb(chunk.nodes).map_err(bad_answer)?);
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(nodes, meta))
        })
    }

    fn traverse(
        &self,
        namespace: &str,
        request: TraverseRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<String>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = traverse_to_pb(&namespace, &request, &o)?;
            let stream = client.traverse(request).await.map_err(status)?.into_inner();
            let mut ids = Vec::new();
            let meta = collect(stream, |chunk| {
                ids.extend(chunk.ids);
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(ids, meta))
        })
    }

    fn shortest_path(
        &self,
        namespace: &str,
        request: PathRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Option<Path>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = path_request_to_pb(&namespace, &request, &o);
            let response = client.shortest_path(request).await.map_err(status)?.into_inner();
            Ok(answer_from_pb(response.path.map(path_from_answer_pb), response.meta.unwrap_or_default()))
        })
    }

    fn random_walks(
        &self,
        namespace: &str,
        request: WalkRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Vec<String>>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = walk_to_pb(&namespace, &request, &o);
            let stream = client.random_walks(request).await.map_err(status)?.into_inner();
            let mut walks = Vec::new();
            let meta = collect(stream, |chunk| {
                walks.extend(chunk.walks.into_iter().map(|w| w.nodes));
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(walks, meta))
        })
    }

    fn subgraph(
        &self,
        namespace: &str,
        request: SubgraphRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Subgraph>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = subgraph_to_pb(&namespace, &request, &o)?;
            let stream = client.subgraph(request).await.map_err(status)?.into_inner();
            let (mut nodes, mut edges) = (Vec::new(), Vec::new());
            let meta = collect(stream, |chunk| {
                nodes.extend(chunk.nodes);
                edges.extend(chunk.edges);
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(subgraph_answer_from_pb(nodes, edges).map_err(bad_answer)?, meta))
        })
    }

    fn match_pattern(
        &self,
        namespace: &str,
        request: MatchRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<MatchRow>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = match_to_pb(&namespace, &request, &o)?;
            let stream = client.match_pattern(request).await.map_err(status)?.into_inner();
            let mut rows = Vec::new();
            let meta = collect(stream, |chunk| {
                rows.extend(chunk.rows.into_iter().map(match_row_from_pb));
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(rows, meta))
        })
    }

    fn analyze(
        &self,
        namespace: &str,
        request: AnalyticsRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<JobResult>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = analyze_to_pb(&namespace, &request, &o)?;
            let stream = client.analyze(request).await.map_err(status)?.into_inner();
            let mut rows =
                JobRows { kind: pb::JobResultKind::Unspecified, scores: vec![], groups: vec![], counts: vec![] };
            let meta = collect(stream, |chunk| {
                rows.kind = chunk.kind();
                rows.scores.extend(chunk.scores);
                rows.groups.extend(chunk.groups);
                rows.counts.extend(chunk.counts);
                Ok(chunk.meta)
            })
            .await?;
            Ok(answer_from_pb(job_result_from_pb(rows).map_err(bad_answer)?, meta))
        })
    }

    fn catalog(
        &self,
        namespace: &str,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<NamespaceCatalog>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::GetCatalogRequest { namespace, options: options(&o) };
            let response = client.get_catalog(request).await.map_err(status)?.into_inner();
            let catalog = catalog_from_pb(response.catalog).map_err(bad_answer)?;
            Ok(answer_from_pb(catalog, response.meta.unwrap_or_default()))
        })
    }

    fn namespace_status(&self, namespace: &str) -> impl Future<Output = Result<NamespaceStatus, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::GetNamespaceStatusRequest { namespace };
            let response = client.get_namespace_status(request).await.map_err(status)?.into_inner();
            status_from_pb(response.status).map_err(bad_answer)
        })
    }

    fn namespaces(&self) -> impl Future<Output = Result<Vec<NamespaceInfo>, Error>> + Send {
        self.call(move |mut client| async move {
            let response = client.list_namespaces(pb::ListNamespacesRequest {}).await.map_err(status)?.into_inner();
            response.namespaces.into_iter().map(namespace_info_from_pb).collect::<Result<_, _>>().map_err(bad_answer)
        })
    }

    fn create_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let name = name.to_owned();
        self.call(move |mut client| async move {
            let request = pb::CreateNamespaceRequest { name, idempotency_key: idempotency_key_to_pb(&key) };
            let response = client.create_namespace(request).await.map_err(status)?.into_inner();
            namespace_result_from_pb(response.event, response.deduplicated).map_err(bad_answer)
        })
    }

    fn drop_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let name = name.to_owned();
        self.call(move |mut client| async move {
            let request = pb::DropNamespaceRequest { name, idempotency_key: idempotency_key_to_pb(&key) };
            let response = client.drop_namespace(request).await.map_err(status)?.into_inner();
            namespace_result_from_pb(response.event, response.deduplicated).map_err(bad_answer)
        })
    }
}
