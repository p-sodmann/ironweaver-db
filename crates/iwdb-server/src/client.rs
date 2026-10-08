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
//!
//! **Credentials** (step 15a): a client holds one token (a session's or an
//! API token) and sends it with every call as `authorization: Bearer`.
//! Give it with [`Remote::with_token`], or log in with [`Remote::login`],
//! which keeps the session's token. It implements
//! [`Accounts`](iwdb_query::Accounts) too (users, grants, tokens).
//!
//! **TLS** (step 15b, ADR 0048): an `https://` endpoint speaks TLS, and
//! verifies the server against the CA of a [`ClientTls`] (default: the
//! operating system's trust store); a client certificate and key in it
//! authenticate the client (mTLS) when the server verifies them. Use
//! [`Remote::connect_tls`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use ironweaver_core::EdgeId;
use iwdb_engine::catalog::NamespaceCatalog;
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, Mutation};
use iwdb_query::admin::{PruneReport, VerifyReport};
use iwdb_query::log::LogTail;
use iwdb_query::metrics::Metrics;
use iwdb_query::read::Explain;
use iwdb_query::requests::{ConsumerInfo, RequestInfo};
use iwdb_query::{Accounts, Admin, Listed, NewToken, Role, Secret, ServerStatus, Session, TokenInfo, UserInfo};
use iwdb_query::{
    AnalyticsRequest, Answer, Changes, ChangesRequest, CommitOptions, Database, Edge, Error, ExplainRequest,
    FindRequest, JobResult, MatchRequest, MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Path, PathRequest,
    QueryOptions, Schema, Subgraph, SubgraphRequest, TraverseRequest, WalkRequest,
};
use iwdb_query::{BackupDone, BackupRequest, Checkpointed, JobInfo, JobOwner, JobPage, VerifyTarget};
use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use tonic::Streaming;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::DEFAULT_MAX_MESSAGE_BYTES;
use crate::convert::*;
use crate::proto as pb;
use crate::proto::admin_service_client::AdminServiceClient;
use crate::proto::auth_service_client::AuthServiceClient;
use crate::proto::database_service_client::DatabaseServiceClient;
use crate::status::from_status;
pub use crate::tls::ClientTls;

#[cfg(any(feature = "rest", feature = "otel"))]
pub(crate) mod https;
#[cfg(feature = "rest")]
mod rest;
#[cfg(feature = "rest")]
pub use rest::RestRemote;

type Client = DatabaseServiceClient<InterceptedService<Channel, Bearer>>;
type AuthClient = AuthServiceClient<InterceptedService<Channel, Bearer>>;
type AdminClient = AdminServiceClient<InterceptedService<Channel, Bearer>>;

/// The token a client sends, shared by its calls.
pub(crate) type TokenSlot = Arc<RwLock<Option<Secret>>>;

pub(crate) fn read_token(slot: &TokenSlot) -> Option<Secret> {
    slot.read().unwrap_or_else(PoisonError::into_inner).clone()
}

pub(crate) fn write_token(slot: &TokenSlot, token: Option<Secret>) {
    *slot.write().unwrap_or_else(PoisonError::into_inner) = token;
}

/// Adds `authorization: Bearer <token>` to every call.
#[derive(Clone)]
pub struct Bearer(TokenSlot);

impl Interceptor for Bearer {
    fn call(&mut self, mut request: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = read_token(&self.0) {
            let value = MetadataValue::try_from(format!("Bearer {}", token.expose()))
                .map_err(|_| tonic::Status::invalid_argument("the token isn't valid in a header"))?;
            request.metadata_mut().insert("authorization", value);
        }
        Ok(request)
    }
}

/// Whether `endpoint` speaks TLS (`https://`); TLS settings need it.
pub(crate) fn https(endpoint: &str, tls: &ClientTls) -> Result<bool, Error> {
    let https = endpoint.starts_with("https://");
    if !https && tls.is_set() {
        return Err(Error::invalid(format!(
            "a CA or client certificate was given, but '{}' isn't an https:// endpoint",
            endpoint
        )));
    }
    Ok(https)
}

fn tls_file(what: &str, path: &std::path::Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::invalid(format!("can't read the {} {}: {}", what, path.display(), e)))
}

/// tonic's TLS settings from `tls`. The files are checked by our own PEM
/// reader first, so no error quotes a key.
pub(crate) fn tonic_tls(tls: &ClientTls) -> Result<ClientTlsConfig, Error> {
    let invalid = |e: crate::tls::TlsError| Error::invalid(e.to_string());
    let mut config = ClientTlsConfig::new();
    config = match &tls.ca {
        Some(ca) => {
            crate::tls::certificates("CA certificate", ca).map_err(invalid)?;
            config.ca_certificate(Certificate::from_pem(tls_file("CA certificate", ca)?))
        }
        None => config.with_native_roots(),
    };
    if let Some((cert, key)) = tls.identity().map_err(invalid)? {
        crate::tls::certificates("client certificate", cert).map_err(invalid)?;
        crate::tls::private_key(key).map_err(invalid)?;
        config =
            config.identity(Identity::from_pem(tls_file("client certificate", cert)?, tls_file("private key", key)?));
    }
    Ok(config)
}

/// A database served by an `iwdb-server`, as a [`Database`].
pub struct Remote {
    client: Client,
    auth: AuthClient,
    admin: AdminClient,
    token: TokenSlot,
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
        Remote::connect_tls(endpoint, &ClientTls::default())
    }

    /// A client of the server at `endpoint` (`https://host:port` with TLS,
    /// `http://host:port` without), trusting and presenting what `tls`
    /// says. Errors: also `invalid_argument` for TLS files that can't be
    /// read or used, and for TLS settings with an `http://` endpoint.
    pub fn connect_tls(endpoint: &str, tls: &ClientTls) -> Result<Remote, Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("iwdb-client")
            .enable_all()
            .build()
            .map_err(|e| Error::internal(format!("can't start the client's runtime: {}", e)))?;
        let mut remote = Remote::connect_on_tls(runtime.handle().clone(), endpoint, tls)?;
        remote.runtime = Some(runtime);
        Ok(remote)
    }

    /// The same, running its calls on `handle`'s runtime.
    pub fn connect_on(handle: Handle, endpoint: &str) -> Result<Remote, Error> {
        Remote::connect_on_tls(handle, endpoint, &ClientTls::default())
    }

    /// [`connect_tls`](Self::connect_tls), running its calls on `handle`'s
    /// runtime.
    pub fn connect_on_tls(handle: Handle, endpoint: &str, tls: &ClientTls) -> Result<Remote, Error> {
        let text = endpoint;
        let mut endpoint = Endpoint::from_shared(endpoint.to_owned())
            .map_err(|e| Error::invalid(format!("invalid endpoint '{}': {}", endpoint, e)))?
            .tcp_nodelay(true);
        if https(text, tls)? {
            endpoint = endpoint
                .tls_config(tonic_tls(tls)?)
                .map_err(|e| Error::invalid(format!("the TLS settings can't be used: {}", e)))?;
        }
        let channel = {
            let _runtime = handle.enter();
            endpoint.connect_lazy()
        };
        let token = TokenSlot::default();
        let client = DatabaseServiceClient::with_interceptor(channel.clone(), Bearer(token.clone()))
            .max_decoding_message_size(DEFAULT_MAX_MESSAGE_BYTES)
            .max_encoding_message_size(DEFAULT_MAX_MESSAGE_BYTES);
        let auth = AuthServiceClient::with_interceptor(channel.clone(), Bearer(token.clone()));
        let admin = AdminServiceClient::with_interceptor(channel, Bearer(token.clone()))
            .max_decoding_message_size(DEFAULT_MAX_MESSAGE_BYTES);
        Ok(Remote { client, auth, admin, token, handle, runtime: None })
    }

    /// Send `token` (a session's or an API token) with every call.
    pub fn with_token(self, token: Secret) -> Self {
        self.set_token(Some(token));
        self
    }

    /// Replace the token every call sends (`None`: none).
    pub fn set_token(&self, token: Option<Secret>) {
        write_token(&self.token, token);
    }

    /// The token calls send now.
    pub fn token(&self) -> Option<Secret> {
        read_token(&self.token)
    }

    /// Log in: on success the session's token is what calls send from now
    /// on. Errors: `unauthenticated` (a wrong user or password, too many
    /// failures).
    pub fn login(&self, user: &str, password: Secret) -> impl Future<Output = Result<Session, Error>> + Send {
        let (user, slot) = (user.to_owned(), self.token.clone());
        self.call_auth(move |mut client| async move {
            let request = pb::LoginRequest { user, password: password.expose().to_owned(), cookie: false };
            let response = client.login(request).await.map_err(status)?.into_inner();
            let token = Secret::new(response.token);
            write_token(&slot, Some(token.clone()));
            Ok(Session {
                token,
                user: user_from_pb(response.user).map_err(bad_answer)?,
                expires_ms: response.expires_ms,
            })
        })
    }

    /// End the session; calls send no token afterwards.
    pub fn logout(&self) -> impl Future<Output = Result<(), Error>> + Send {
        let slot = self.token.clone();
        self.call_auth(move |mut client| async move {
            client.logout(pb::LogoutRequest {}).await.map_err(status)?;
            write_token(&slot, None);
            Ok(())
        })
    }

    /// Who the server takes this client for, and whether it checks
    /// credentials at all.
    pub fn whoami(&self) -> impl Future<Output = Result<(UserInfo, bool), Error>> + Send {
        self.call_auth(move |mut client| async move {
            let response = client.who_am_i(pb::WhoAmIRequest {}).await.map_err(status)?.into_inner();
            Ok((user_from_pb(response.user).map_err(bad_answer)?, response.auth_enabled))
        })
    }

    fn call_auth<T, F>(&self, f: impl FnOnce(AuthClient) -> F) -> Call<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, Error>> + Send + 'static,
    {
        Call(self.handle.spawn(f(self.auth.clone())))
    }

    fn call_admin<T, F>(&self, f: impl FnOnce(AdminClient) -> F) -> Call<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, Error>> + Send + 'static,
    {
        Call(self.handle.spawn(f(self.admin.clone())))
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

/// Whether `s` is a call cancelled here, without the server's code, by
/// anything but its deadline: the connection closed under it (hyper's
/// "operation was canceled", an HTTP/2 stream reset). With TLS 1.3 the
/// server checks a client certificate after the client's side of the
/// handshake is done, so a refused certificate closes the connection
/// during the first call. A deadline is tonic's `TimeoutExpired`.
fn connection_closed(s: &tonic::Status) -> bool {
    s.code() == tonic::Code::Cancelled
        && s.metadata().get(crate::status::CODE_KEY).is_none()
        && s.message() != tonic::TimeoutExpired(()).to_string()
}

/// A call's status as an error. A status with a source error and without
/// the server's code was made here, by a connection that failed (refused,
/// a TLS handshake the server or the client refused, a connection that
/// broke): `unavailable`, with the source's reasons; so is a connection
/// that closed under the call ([`connection_closed`]).
fn status(s: tonic::Status) -> Error {
    if connection_closed(&s) {
        return Error::unavailable(format!("the connection closed: {}", s.message()));
    }
    let local = s.metadata().get(crate::status::CODE_KEY).is_none()
        && matches!(s.code(), tonic::Code::Internal | tonic::Code::Unknown | tonic::Code::Unavailable);
    match std::error::Error::source(&s) {
        Some(source) if local => {
            let mut message = s.message().to_owned();
            let mut next = Some(source);
            while let Some(e) = next {
                let text = e.to_string();
                if !message.contains(&text) {
                    message = format!("{}: {}", message, text);
                }
                next = e.source();
            }
            Error::unavailable(message)
        }
        _ => from_status(&s),
    }
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

    fn schema(&self, namespace: &str, o: QueryOptions) -> impl Future<Output = Result<Answer<Schema>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |mut client| async move {
            let request = pb::GetSchemaRequest { namespace, options: options(&o) };
            let response = client.get_schema(request).await.map_err(status)?.into_inner();
            let schema = schema_from_pb(response.schema).map_err(bad_answer)?;
            Ok(answer_from_pb(schema, response.meta.unwrap_or_default()))
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

fn limit_to_pb(limit: Option<usize>) -> Option<u32> {
    limit.map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

impl Admin for Remote {
    fn server_status(&self) -> impl Future<Output = Result<ServerStatus, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.get_server_status(pb::GetServerStatusRequest {}).await.map_err(status)?;
            server_status_from_pb(response.into_inner().status).map_err(bad_answer)
        })
    }

    fn active_requests(
        &self,
        user: Option<String>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Listed<RequestInfo>, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let request = pb::ListRequestsRequest { user, limit: limit_to_pb(limit) };
            let response = client.list_requests(request).await.map_err(status)?;
            requests_from_pb(response.into_inner()).map_err(bad_answer)
        })
    }

    fn cancel_request(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<RequestInfo, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.cancel_request(pb::CancelRequestRequest { id, user }).await.map_err(status)?;
            request_from_pb(response.into_inner().request).map_err(bad_answer)
        })
    }

    fn consumers(&self) -> impl Future<Output = Result<Vec<ConsumerInfo>, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.list_consumers(pb::ListConsumersRequest {}).await.map_err(status)?;
            response
                .into_inner()
                .consumers
                .into_iter()
                .map(consumer_from_pb)
                .collect::<Result<_, _>>()
                .map_err(bad_answer)
        })
    }

    fn metrics(&self) -> impl Future<Output = Result<Metrics, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.get_metrics(pb::GetMetricsRequest {}).await.map_err(status)?;
            metrics_from_pb(response.into_inner()).map_err(bad_answer)
        })
    }

    fn log(&self, after: u64, limit: Option<usize>) -> impl Future<Output = Result<LogTail, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response =
                client.get_log(pb::GetLogRequest { after, limit: limit_to_pb(limit) }).await.map_err(status)?;
            log_from_pb(response.into_inner()).map_err(bad_answer)
        })
    }

    fn checkpoint(&self, namespace: Option<String>) -> impl Future<Output = Result<Vec<Checkpointed>, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.checkpoint(pb::CheckpointRequest { namespace }).await.map_err(status)?;
            Ok(checkpoints_from_pb(response.into_inner()))
        })
    }

    fn backup(&self, request: BackupRequest) -> impl Future<Output = Result<BackupDone, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let request = pb::BackupRequest {
                name: request.name,
                max_bytes_per_second: request.max_bytes_per_second,
                no_verify: !request.verify,
            };
            let response = client.backup(request).await.map_err(status)?;
            backup_from_pb(response.into_inner()).map_err(bad_answer)
        })
    }

    fn verify(&self, target: VerifyTarget) -> impl Future<Output = Result<VerifyReport, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.verify(verify_target_to_pb(&target)).await.map_err(status)?.into_inner();
            verify_from_pb(response.report.ok_or_else(|| bad_answer(Error::invalid("the report is missing")))?)
                .map_err(bad_answer)
        })
    }

    fn prune_archive(&self, before: String, dry_run: bool) -> impl Future<Output = Result<PruneReport, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.prune_archive(pb::PruneArchiveRequest { before, dry_run }).await.map_err(status)?;
            prune_from_pb(response.into_inner()).map_err(bad_answer)
        })
    }

    /// `owner` is ignored: the server's authorisation point sets the caller.
    fn start_job(
        &self,
        namespace: String,
        request: AnalyticsRequest,
        options: QueryOptions,
        _owner: Option<JobOwner>,
    ) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        let start = start_job_to_pb(&namespace, &request, &options);
        self.call_admin(move |mut client| async move {
            let response = client.start_job(start?).await.map_err(status)?;
            job_from_pb(response.into_inner().job).map_err(bad_answer)
        })
    }

    fn jobs(
        &self,
        user: Option<String>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Listed<JobInfo>, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let limit = limit.map(|n| u32::try_from(n).unwrap_or(u32::MAX));
            let response = client.list_jobs(pb::ListJobsRequest { user, limit }).await.map_err(status)?;
            jobs_from_pb(response.into_inner()).map_err(bad_answer)
        })
    }

    fn job(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.get_job(pb::GetJobRequest { id, user }).await.map_err(status)?;
            job_from_pb(response.into_inner().job).map_err(bad_answer)
        })
    }

    fn cancel_job(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let response = client.cancel_job(pb::CancelJobRequest { id, user }).await.map_err(status)?;
            job_from_pb(response.into_inner().job).map_err(bad_answer)
        })
    }

    fn job_result(
        &self,
        id: u64,
        user: Option<String>,
        offset: u64,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<JobPage, Error>> + Send {
        self.call_admin(move |mut client| async move {
            let limit = limit.map(|n| u32::try_from(n).unwrap_or(u32::MAX));
            let request = pb::GetJobResultRequest { id, user, offset, limit };
            let response = client.get_job_result(request).await.map_err(status)?;
            job_page_from_pb(response.into_inner()).map_err(bad_answer)
        })
    }
}

impl Accounts for Remote {
    fn users(&self) -> impl Future<Output = Result<Vec<UserInfo>, Error>> + Send {
        self.call_auth(move |mut client| async move {
            let response = client.list_users(pb::ListUsersRequest {}).await.map_err(status)?.into_inner();
            response.users.into_iter().map(|u| user_from_pb(Some(u))).collect::<Result<_, _>>().map_err(bad_answer)
        })
    }

    fn create_user(
        &self,
        name: &str,
        password: Secret,
        admin: bool,
    ) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let name = name.to_owned();
        self.call_auth(move |mut client| async move {
            let request = pb::CreateUserRequest { name, password: password.expose().to_owned(), admin };
            let response = client.create_user(request).await.map_err(status)?.into_inner();
            user_from_pb(response.user).map_err(bad_answer)
        })
    }

    fn set_password(
        &self,
        name: &str,
        password: Secret,
        current: Option<Secret>,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        let name = name.to_owned();
        self.call_auth(move |mut client| async move {
            let request = pb::SetPasswordRequest {
                name,
                password: password.expose().to_owned(),
                current_password: current.map(|c| c.expose().to_owned()),
            };
            client.set_password(request).await.map_err(status)?;
            Ok(())
        })
    }

    fn delete_user(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let name = name.to_owned();
        self.call_auth(move |mut client| async move {
            client.delete_user(pb::DeleteUserRequest { name }).await.map_err(status)?;
            Ok(())
        })
    }

    fn set_admin(&self, name: &str, admin: bool) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let name = name.to_owned();
        self.call_auth(move |mut client| async move {
            let response = client.set_admin(pb::SetAdminRequest { name, admin }).await.map_err(status)?.into_inner();
            user_from_pb(response.user).map_err(bad_answer)
        })
    }

    fn grant(&self, name: &str, namespace: &str, role: Role) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let (name, namespace) = (name.to_owned(), namespace.to_owned());
        self.call_auth(move |mut client| async move {
            let request = pb::GrantRequest { name, namespace, role: role_to_pb(role) as i32 };
            let response = client.grant(request).await.map_err(status)?.into_inner();
            user_from_pb(response.user).map_err(bad_answer)
        })
    }

    fn revoke(&self, name: &str, namespace: &str) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let (name, namespace) = (name.to_owned(), namespace.to_owned());
        self.call_auth(move |mut client| async move {
            let response = client.revoke(pb::RevokeRequest { name, namespace }).await.map_err(status)?.into_inner();
            user_from_pb(response.user).map_err(bad_answer)
        })
    }

    fn create_token(
        &self,
        user: &str,
        name: &str,
        expires_in: Option<Duration>,
    ) -> impl Future<Output = Result<NewToken, Error>> + Send {
        let (user, name) = (user.to_owned(), name.to_owned());
        self.call_auth(move |mut client| async move {
            let request = pb::CreateTokenRequest { user, name, expires_in_secs: expires_in.map(|d| d.as_secs()) };
            let response = client.create_token(request).await.map_err(status)?.into_inner();
            new_token_from_pb(response).map_err(bad_answer)
        })
    }

    fn revoke_token(&self, user: &str, name: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let (user, name) = (user.to_owned(), name.to_owned());
        self.call_auth(move |mut client| async move {
            client.revoke_token(pb::RevokeTokenRequest { user, name }).await.map_err(status)?;
            Ok(())
        })
    }

    fn tokens(&self, user: &str) -> impl Future<Output = Result<Vec<TokenInfo>, Error>> + Send {
        let user = user.to_owned();
        self.call_auth(move |mut client| async move {
            let response = client.list_tokens(pb::ListTokensRequest { user }).await.map_err(status)?.into_inner();
            Ok(response.tokens.into_iter().map(token_info_from_pb).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwdb_query::Code;

    #[test]
    fn a_connection_closed_under_a_call_is_unavailable() {
        for closed in ["operation was canceled: connection closed", "h2 protocol error: stream reset"] {
            assert_eq!(status(tonic::Status::cancelled(closed)).code(), Code::Unavailable, "{}", closed);
        }
        // A deadline, and the server's own cancel, stay `cancelled`
        assert_eq!(status(tonic::Status::cancelled("Timeout expired")).code(), Code::Cancelled);
        let mut server = tonic::Status::cancelled("operation was canceled");
        server.metadata_mut().insert(crate::status::CODE_KEY, "cancelled".parse().expect("value"));
        assert_eq!(status(server).code(), Code::Cancelled);
    }
}
