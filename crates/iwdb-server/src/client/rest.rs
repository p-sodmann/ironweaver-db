//! [`RestRemote`]: the [`Database`] trait over the REST API (feature
//! `client`, ADR 0030). It exists so that the conformance suite runs over
//! REST, and as a reference for clients in other languages; Rust programs
//! should prefer [`Remote`](super::Remote) (gRPC: smaller messages, HTTP/2).
//!
//! It sends the same request messages as `Remote`, as JSON, over HTTP/1.1
//! (one connection per call in flight, kept alive), and reads streamed
//! answers either as one JSON message or, with [`RestRemote::ndjson`], as
//! NDJSON. Calls run on a tokio runtime as `Remote`'s do; dropping a call's
//! future closes its connection. Credentials work as `Remote`'s (a token
//! sent as `authorization: Bearer`, [`RestRemote::login`]), and so does TLS
//! (`https://`, [`RestRemote::connect_tls`]). Every request carries the
//! CSRF header, so a client certificate alone can authenticate writes.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
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
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::runtime::{Handle, Runtime};

use super::https::Connector;
use super::{Call, ClientTls, TokenSlot, bad_answer, read_token, write_token};
use crate::auth::CSRF_HEADER;
use crate::convert::*;
use crate::proto as pb;
use crate::rest::{JSON, NDJSON};
use crate::status::from_http;

type Http = Client<Connector, Full<Bytes>>;

/// A database served by an `iwdb-server`, as a [`Database`], over REST.
pub struct RestRemote {
    http: Http,
    /// `http://host:port` or `https://host:port`, without a trailing slash.
    base: String,
    ndjson: bool,
    token: TokenSlot,
    handle: Handle,
    runtime: Option<Runtime>,
}

impl std::fmt::Debug for RestRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestRemote").field("base", &self.base).field("ndjson", &self.ndjson).finish_non_exhaustive()
    }
}

impl RestRemote {
    /// A client of the server at `endpoint` (`http://host:port`), on a
    /// runtime of its own (two threads). Errors: `invalid_argument` for an
    /// endpoint that isn't `http://...`, `internal` if the runtime can't
    /// start.
    pub fn connect(endpoint: &str) -> Result<RestRemote, Error> {
        RestRemote::connect_tls(endpoint, &ClientTls::default())
    }

    /// A client of the server at `endpoint` (`https://host:port` with TLS,
    /// `http://host:port` without), trusting and presenting what `tls` says
    /// ([`Remote::connect_tls`](super::Remote::connect_tls)).
    pub fn connect_tls(endpoint: &str, tls: &ClientTls) -> Result<RestRemote, Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("iwdb-rest-client")
            .enable_all()
            .build()
            .map_err(|e| Error::internal(format!("can't start the client's runtime: {}", e)))?;
        let mut remote = RestRemote::connect_on_tls(runtime.handle().clone(), endpoint, tls)?;
        remote.runtime = Some(runtime);
        Ok(remote)
    }

    /// The same, running its calls on `handle`'s runtime.
    pub fn connect_on(handle: Handle, endpoint: &str) -> Result<RestRemote, Error> {
        RestRemote::connect_on_tls(handle, endpoint, &ClientTls::default())
    }

    /// [`connect_tls`](Self::connect_tls), running its calls on `handle`'s
    /// runtime.
    pub fn connect_on_tls(handle: Handle, endpoint: &str, tls: &ClientTls) -> Result<RestRemote, Error> {
        let base = endpoint.trim_end_matches('/');
        let scheme = base.starts_with("http://") || base.starts_with("https://");
        if !scheme || base.parse::<http::Uri>().is_err() {
            return Err(Error::invalid(format!(
                "invalid endpoint '{}': expected https://host:port or http://host:port",
                endpoint
            )));
        }
        let config = if super::https(base, tls)? {
            let config = tls.rustls_config(&[b"http/1.1"]).map_err(|e| Error::invalid(e.to_string()))?;
            Some(std::sync::Arc::new(config))
        } else {
            None
        };
        let http = Client::builder(TokioExecutor::new()).build(Connector::new(config));
        Ok(RestRemote {
            http,
            base: base.to_owned(),
            ndjson: false,
            token: TokenSlot::default(),
            handle,
            runtime: None,
        })
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

    /// Log in (`POST /v1/auth/login`): on success the session's token is
    /// what calls send from now on.
    pub fn login(&self, user: &str, password: Secret) -> impl Future<Output = Result<Session, Error>> + Send {
        let (user, slot) = (user.to_owned(), self.token.clone());
        self.call(move |c| async move {
            let request = pb::LoginRequest { user, password: password.expose().to_owned(), cookie: false };
            let response: pb::LoginResponse = c.post(format!("{}/v1/auth/login", c.base), &request).await?;
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
        self.call(move |c| async move {
            let _: pb::LogoutResponse = c.post(format!("{}/v1/auth/logout", c.base), &pb::LogoutRequest {}).await?;
            write_token(&slot, None);
            Ok(())
        })
    }

    /// Who the server takes this client for, and whether it checks
    /// credentials at all.
    pub fn whoami(&self) -> impl Future<Output = Result<(UserInfo, bool), Error>> + Send {
        self.call(move |c| async move {
            let response: pb::WhoAmIResponse = c.get(format!("{}/v1/auth/whoami", c.base)).await?;
            Ok((user_from_pb(response.user).map_err(bad_answer)?, response.auth_enabled))
        })
    }

    /// Read streamed answers as NDJSON (one chunk per line) instead of one
    /// JSON message.
    pub fn ndjson(mut self, ndjson: bool) -> Self {
        self.ndjson = ndjson;
        self
    }

    fn call<T, F>(&self, f: impl FnOnce(Calls) -> F) -> Call<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, Error>> + Send + 'static,
    {
        let calls =
            Calls { http: self.http.clone(), base: self.base.clone(), ndjson: self.ndjson, token: self.token() };
        Call(self.handle.spawn(f(calls)))
    }
}

impl Drop for RestRemote {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Percent-encode a path segment (everything but RFC 3986's unreserved
/// characters).
fn segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// What a call needs, moved onto the runtime.
struct Calls {
    http: Http,
    base: String,
    ndjson: bool,
    token: Option<Secret>,
}

impl Calls {
    fn url(&self, namespace: &str, rest: &str) -> String {
        format!("{}/v1/namespaces/{}{}", self.base, segment(namespace), rest)
    }

    /// Send a request; the answer's body if it succeeded, the error its
    /// status and body stand for otherwise.
    async fn send(&self, method: Method, url: String, body: Option<Vec<u8>>, accept: &str) -> Result<Bytes, Error> {
        let mut builder =
            Request::builder().method(method).uri(&url).header(header::ACCEPT, accept).header(CSRF_HEADER, "1");
        if let Some(token) = &self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {}", token.expose()));
        }
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, JSON);
        }
        let request = builder
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|e| Error::invalid(format!("invalid request to {}: {}", url, e)))?;
        let response =
            self.http.request(request).await.map_err(|e| Error::unavailable(format!("{}: {}", url, transport(&e))))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::unavailable(format!("the answer was cut off: {}", e)))?
            .to_bytes();
        if status == StatusCode::OK {
            return Ok(bytes);
        }
        let error: Option<pb::Error> = serde_json::from_slice(&bytes).ok();
        Err(match error {
            Some(e) => from_http(status, Some(&e.code), &e.message),
            None => from_http(status, None, &format!("{} answered {}", url, status)),
        })
    }

    /// A unary call: POST `request`, read the answer message.
    async fn post<Q: Serialize, R: DeserializeOwned>(&self, url: String, request: &Q) -> Result<R, Error> {
        let body =
            serde_json::to_vec(request).map_err(|e| Error::invalid(format!("can't write the request: {}", e)))?;
        let bytes = self.send(Method::POST, url, Some(body), JSON).await?;
        parse(&bytes)
    }

    /// A unary call with any method: `request` as the body.
    async fn json<Q: Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        url: String,
        request: &Q,
    ) -> Result<R, Error> {
        let body =
            serde_json::to_vec(request).map_err(|e| Error::invalid(format!("can't write the request: {}", e)))?;
        parse(&self.send(method, url, Some(body), JSON).await?)
    }

    fn user_url(&self, user: &str, rest: &str) -> String {
        format!("{}/v1/users/{}{}", self.base, segment(user), rest)
    }

    async fn get<R: DeserializeOwned>(&self, url: String) -> Result<R, Error> {
        parse(&self.send(Method::GET, url, None, JSON).await?)
    }

    /// A streamed call: the answer's chunks (one if not asked for NDJSON),
    /// and the meta of the last one.
    async fn stream<Q: Serialize, R: DeserializeOwned>(
        &self,
        url: String,
        request: &Q,
        mut meta: impl FnMut(&mut R) -> Option<pb::AnswerMeta>,
    ) -> Result<(Vec<R>, pb::AnswerMeta), Error> {
        let body =
            serde_json::to_vec(request).map_err(|e| Error::invalid(format!("can't write the request: {}", e)))?;
        let accept = if self.ndjson { NDJSON } else { JSON };
        let bytes = self.send(Method::POST, url, Some(body), accept).await?;
        let mut chunks: Vec<R> = if self.ndjson {
            bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(parse).collect::<Result<_, _>>()?
        } else {
            vec![parse(&bytes)?]
        };
        let mut last = None;
        for chunk in &mut chunks {
            if last.is_some() {
                return Err(bad_answer(Error::invalid("a chunk after the last one")));
            }
            last = meta(chunk);
        }
        let meta = last.ok_or_else(|| Error::unavailable("the answer was cut off before its end"))?;
        Ok((chunks, meta))
    }
}

/// The underlying error of a failed request: hyper-util's own says only
/// "client error (Connect)".
fn transport(e: &hyper_util::client::legacy::Error) -> String {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        message = format!("{}: {}", message, s);
        source = s.source();
    }
    message
}

fn parse<R: DeserializeOwned>(bytes: &[u8]) -> Result<R, Error> {
    serde_json::from_slice(bytes).map_err(|e| bad_answer(Error::invalid(format!("invalid JSON: {}", e))))
}

/// `QueryOptions` as the query string of a GET read.
fn query(o: &QueryOptions) -> String {
    let o = options_to_pb(o);
    let l = o.limits.unwrap_or_default();
    let mut pairs: Vec<String> = Vec::new();
    let mut add = |name: &str, value: Option<String>| {
        if let Some(v) = value {
            pairs.push(format!("{}={}", name, segment(&v)));
        }
    };
    add("min_seq", o.min_seq.map(|v| v.to_string()));
    add("history", (!o.history.is_empty()).then_some(o.history));
    add("timeout_ms", o.timeout_ms.map(|v| v.to_string()));
    add("max_results", l.max_results.map(|v| v.to_string()));
    add("max_visited", l.max_visited.map(|v| v.to_string()));
    add("max_edges", l.max_edges.map(|v| v.to_string()));
    add("partial", o.partial.then(|| "true".to_owned()));
    add("cursor", (!o.cursor.is_empty()).then_some(o.cursor));
    if pairs.is_empty() { String::new() } else { format!("?{}", pairs.join("&")) }
}

/// The query of the change stream's route: the options it takes.
fn changes_query(request: &ChangesRequest, o: &QueryOptions) -> String {
    let o = options_to_pb(o);
    let mut pairs = vec![format!("from_seq={}", request.from_seq), format!("wait={}", request.wait)];
    pairs.extend(o.min_seq.map(|v| format!("min_seq={}", v)));
    pairs.extend((!o.history.is_empty()).then(|| format!("history={}", segment(&o.history))));
    pairs.extend(o.timeout_ms.map(|v| format!("timeout_ms={}", v)));
    pairs.extend(o.limits.and_then(|l| l.max_results).map(|v| format!("max_results={}", v)));
    format!("?{}", pairs.join("&"))
}

fn options(o: &QueryOptions) -> Option<pb::QueryOptions> {
    Some(options_to_pb(o))
}

impl Database for RestRemote {
    fn commit(
        &self,
        namespace: &str,
        mutations: Vec<Mutation>,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let request = pb::CommitRequest {
                namespace: String::new(),
                mutations: mutations_to_pb(&mutations)?,
                options: Some(commit_options_to_pb(&options)),
            };
            let response: pb::CommitResponse = c.post(c.url(&namespace, "/commit"), &request).await?;
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
        self.call(move |c| async move {
            let request = pb::CommitCatalogRequest {
                namespace: String::new(),
                change: Some(catalog_change_to_pb(&change)),
                options: Some(commit_options_to_pb(&options)),
            };
            let response: pb::CommitCatalogResponse = c.post(c.url(&namespace, "/catalog"), &request).await?;
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
        self.call(move |c| async move {
            let request = pb::WaitForSeqRequest { namespace: String::new(), seq, options: options(&o) };
            let response: pb::WaitForSeqResponse = c.post(c.url(&namespace, "/wait"), &request).await?;
            Ok(response.seq)
        })
    }

    fn get_nodes(
        &self,
        namespace: &str,
        ids: Vec<String>,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Node>>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let request = pb::GetNodesRequest { namespace: String::new(), ids, options: options(&o) };
            let url = c.url(&namespace, "/get-nodes");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::GetNodesResponse| r.meta.take()).await?;
            let nodes = chunks.into_iter().flat_map(|r| r.nodes).collect();
            Ok(answer_from_pb(maybe_nodes_from_pb(nodes).map_err(bad_answer)?, meta))
        })
    }

    fn get_edges(
        &self,
        namespace: &str,
        ids: Vec<EdgeId>,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Edge>>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let ids = ids.into_iter().map(|e| e.0).collect();
            let request = pb::GetEdgesRequest { namespace: String::new(), ids, options: options(&o) };
            let url = c.url(&namespace, "/get-edges");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::GetEdgesResponse| r.meta.take()).await?;
            let edges = chunks.into_iter().flat_map(|r| r.edges).collect();
            Ok(answer_from_pb(maybe_edges_from_pb(edges).map_err(bad_answer)?, meta))
        })
    }

    fn find(
        &self,
        namespace: &str,
        request: FindRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let request =
                pb::FindRequest { namespace: String::new(), filter: Some(find_to_pb(&request)?), options: options(&o) };
            let url = c.url(&namespace, "/find");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::FindResponse| r.meta.take()).await?;
            let nodes = chunks.into_iter().flat_map(|r| r.nodes).collect();
            Ok(answer_from_pb(nodes_from_pb(nodes).map_err(bad_answer)?, meta))
        })
    }

    fn changes(
        &self,
        namespace: &str,
        request: ChangesRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Changes>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let url = c.url(&namespace, &format!("/changes{}", changes_query(&request, &o)));
            let response: pb::GetChangesResponse = c.get(url).await?;
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
        self.call(move |c| async move {
            let request = pb::ExplainRequest {
                namespace: String::new(),
                filter: Some(expr_to_pb(&request.filter)?),
                analyze: request.analyze,
                options: options(&o),
            };
            let response: pb::ExplainResponse = c.post(c.url(&namespace, "/explain"), &request).await?;
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
        self.call(move |c| async move {
            let request = neighbourhood_to_pb("", &request, &o)?;
            let url = c.url(&namespace, "/neighbourhood");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::NeighbourhoodResponse| r.meta.take()).await?;
            let nodes = chunks.into_iter().flat_map(|r| r.nodes).collect();
            Ok(answer_from_pb(nodes_from_pb(nodes).map_err(bad_answer)?, meta))
        })
    }

    fn traverse(
        &self,
        namespace: &str,
        request: TraverseRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<String>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let request = traverse_to_pb("", &request, &o)?;
            let url = c.url(&namespace, "/traverse");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::TraverseResponse| r.meta.take()).await?;
            Ok(answer_from_pb(chunks.into_iter().flat_map(|r| r.ids).collect(), meta))
        })
    }

    fn shortest_path(
        &self,
        namespace: &str,
        request: PathRequest,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Option<Path>>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let request = path_request_to_pb("", &request, &o);
            let response: pb::ShortestPathResponse = c.post(c.url(&namespace, "/shortest-path"), &request).await?;
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
        self.call(move |c| async move {
            let request = walk_to_pb("", &request, &o);
            let url = c.url(&namespace, "/random-walks");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::RandomWalksResponse| r.meta.take()).await?;
            let walks = chunks.into_iter().flat_map(|r| r.walks).map(|w| w.nodes).collect();
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
        self.call(move |c| async move {
            let request = subgraph_to_pb("", &request, &o)?;
            let url = c.url(&namespace, "/subgraph");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::SubgraphResponse| r.meta.take()).await?;
            let (mut nodes, mut edges) = (Vec::new(), Vec::new());
            for chunk in chunks {
                nodes.extend(chunk.nodes);
                edges.extend(chunk.edges);
            }
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
        self.call(move |c| async move {
            let request = match_to_pb("", &request, &o)?;
            let url = c.url(&namespace, "/match");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::MatchPatternResponse| r.meta.take()).await?;
            let rows = chunks.into_iter().flat_map(|r| r.rows).map(match_row_from_pb).collect();
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
        self.call(move |c| async move {
            let request = analyze_to_pb("", &request, &o)?;
            let url = c.url(&namespace, "/analyze");
            let (chunks, meta) = c.stream(url, &request, |r: &mut pb::AnalyzeResponse| r.meta.take()).await?;
            let mut rows =
                JobRows { kind: pb::JobResultKind::Unspecified, scores: vec![], groups: vec![], counts: vec![] };
            for chunk in chunks {
                rows.kind = chunk.kind();
                rows.scores.extend(chunk.scores);
                rows.groups.extend(chunk.groups);
                rows.counts.extend(chunk.counts);
            }
            Ok(answer_from_pb(job_result_from_pb(rows).map_err(bad_answer)?, meta))
        })
    }

    fn catalog(
        &self,
        namespace: &str,
        o: QueryOptions,
    ) -> impl Future<Output = Result<Answer<NamespaceCatalog>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let response: pb::GetCatalogResponse = c.get(c.url(&namespace, &format!("/catalog{}", query(&o)))).await?;
            let catalog = catalog_from_pb(response.catalog).map_err(bad_answer)?;
            Ok(answer_from_pb(catalog, response.meta.unwrap_or_default()))
        })
    }

    fn schema(&self, namespace: &str, o: QueryOptions) -> impl Future<Output = Result<Answer<Schema>, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let response: pb::GetSchemaResponse = c.get(c.url(&namespace, &format!("/schema{}", query(&o)))).await?;
            let schema = schema_from_pb(response.schema).map_err(bad_answer)?;
            Ok(answer_from_pb(schema, response.meta.unwrap_or_default()))
        })
    }

    fn namespace_status(&self, namespace: &str) -> impl Future<Output = Result<NamespaceStatus, Error>> + Send {
        let namespace = namespace.to_owned();
        self.call(move |c| async move {
            let response: pb::GetNamespaceStatusResponse = c.get(c.url(&namespace, "")).await?;
            status_from_pb(response.status).map_err(bad_answer)
        })
    }

    fn namespaces(&self) -> impl Future<Output = Result<Vec<NamespaceInfo>, Error>> + Send {
        self.call(move |c| async move {
            let response: pb::ListNamespacesResponse = c.get(format!("{}/v1/namespaces", c.base)).await?;
            response.namespaces.into_iter().map(namespace_info_from_pb).collect::<Result<_, _>>().map_err(bad_answer)
        })
    }

    fn create_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let name = name.to_owned();
        self.call(move |c| async move {
            let request =
                pb::CreateNamespaceRequest { name: String::new(), idempotency_key: idempotency_key_to_pb(&key) };
            let body = serde_json::to_vec(&request).map_err(|e| Error::internal(e.to_string()))?;
            let bytes = c.send(Method::PUT, c.url(&name, ""), Some(body), JSON).await?;
            let response: pb::CreateNamespaceResponse = parse(&bytes)?;
            namespace_result_from_pb(response.event, response.deduplicated).map_err(bad_answer)
        })
    }

    fn drop_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let name = name.to_owned();
        self.call(move |c| async move {
            // Without a key, no body: the route takes an empty one
            let body = match idempotency_key_to_pb(&key) {
                None => None,
                Some(k) => {
                    let request = pb::DropNamespaceRequest { name: String::new(), idempotency_key: Some(k) };
                    Some(serde_json::to_vec(&request).map_err(|e| Error::internal(e.to_string()))?)
                }
            };
            let bytes = c.send(Method::DELETE, c.url(&name, ""), body, JSON).await?;
            let response: pb::DropNamespaceResponse = parse(&bytes)?;
            namespace_result_from_pb(response.event, response.deduplicated).map_err(bad_answer)
        })
    }
}

/// `?name=value&...` of the parameters that are set (empty without any).
fn parameters(parameters: &[(&str, Option<String>)]) -> String {
    let set: Vec<String> =
        parameters.iter().filter_map(|(n, v)| v.as_ref().map(|v| format!("{}={}", n, segment(v)))).collect();
    if set.is_empty() { String::new() } else { format!("?{}", set.join("&")) }
}

impl Admin for RestRemote {
    fn server_status(&self) -> impl Future<Output = Result<ServerStatus, Error>> + Send {
        self.call(move |c| async move {
            let response: pb::GetServerStatusResponse = c.get(format!("{}/v1/status", c.base)).await?;
            server_status_from_pb(response.status).map_err(bad_answer)
        })
    }

    fn active_requests(
        &self,
        user: Option<String>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Listed<RequestInfo>, Error>> + Send {
        self.call(move |c| async move {
            let q = parameters(&[("user", user), ("limit", limit.map(|n| n.min(u32::MAX as usize).to_string()))]);
            let response: pb::ListRequestsResponse = c.get(format!("{}/v1/requests{}", c.base, q)).await?;
            requests_from_pb(response).map_err(bad_answer)
        })
    }

    fn cancel_request(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<RequestInfo, Error>> + Send {
        self.call(move |c| async move {
            let url = format!("{}/v1/requests/{}/cancel", c.base, id);
            let response: pb::CancelRequestResponse = c.post(url, &pb::CancelRequestRequest { id, user }).await?;
            request_from_pb(response.request).map_err(bad_answer)
        })
    }

    fn consumers(&self) -> impl Future<Output = Result<Vec<ConsumerInfo>, Error>> + Send {
        self.call(move |c| async move {
            let response: pb::ListConsumersResponse = c.get(format!("{}/v1/consumers", c.base)).await?;
            response.consumers.into_iter().map(consumer_from_pb).collect::<Result<_, _>>().map_err(bad_answer)
        })
    }

    fn metrics(&self) -> impl Future<Output = Result<Metrics, Error>> + Send {
        self.call(move |c| async move {
            let response: pb::GetMetricsResponse = c.get(format!("{}/v1/metrics", c.base)).await?;
            metrics_from_pb(response).map_err(bad_answer)
        })
    }

    fn log(&self, after: u64, limit: Option<usize>) -> impl Future<Output = Result<LogTail, Error>> + Send {
        self.call(move |c| async move {
            let q = parameters(&[
                ("after", Some(after.to_string())),
                ("limit", limit.map(|n| n.min(u32::MAX as usize).to_string())),
            ]);
            let response: pb::GetLogResponse = c.get(format!("{}/v1/log{}", c.base, q)).await?;
            log_from_pb(response).map_err(bad_answer)
        })
    }

    fn checkpoint(&self, namespace: Option<String>) -> impl Future<Output = Result<Vec<Checkpointed>, Error>> + Send {
        self.call(move |c| async move {
            let url = format!("{}/v1/checkpoint", c.base);
            let response: pb::CheckpointResponse = c.post(url, &pb::CheckpointRequest { namespace }).await?;
            Ok(checkpoints_from_pb(response))
        })
    }

    fn backup(&self, request: BackupRequest) -> impl Future<Output = Result<BackupDone, Error>> + Send {
        self.call(move |c| async move {
            let request = pb::BackupRequest {
                name: request.name,
                max_bytes_per_second: request.max_bytes_per_second,
                no_verify: !request.verify,
            };
            let response: pb::BackupResponse = c.post(format!("{}/v1/backups", c.base), &request).await?;
            backup_from_pb(response).map_err(bad_answer)
        })
    }

    fn verify(&self, target: VerifyTarget) -> impl Future<Output = Result<VerifyReport, Error>> + Send {
        self.call(move |c| async move {
            let url = format!("{}/v1/verify", c.base);
            let response: pb::VerifyResponse = c.post(url, &verify_target_to_pb(&target)).await?;
            verify_from_pb(response.report.ok_or_else(|| bad_answer(Error::invalid("the report is missing")))?)
                .map_err(bad_answer)
        })
    }

    fn prune_archive(&self, before: String, dry_run: bool) -> impl Future<Output = Result<PruneReport, Error>> + Send {
        self.call(move |c| async move {
            let url = format!("{}/v1/archive/prune", c.base);
            let response: pb::PruneArchiveResponse = c.post(url, &pb::PruneArchiveRequest { before, dry_run }).await?;
            prune_from_pb(response).map_err(bad_answer)
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
        self.call(move |c| async move {
            let url = c.url(&namespace, "/jobs");
            let response: pb::StartJobResponse = c.post(url, &start?).await?;
            job_from_pb(response.job).map_err(bad_answer)
        })
    }

    fn jobs(
        &self,
        user: Option<String>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Listed<JobInfo>, Error>> + Send {
        self.call(move |c| async move {
            let q = parameters(&[("user", user), ("limit", limit.map(|n| n.min(u32::MAX as usize).to_string()))]);
            let response: pb::ListJobsResponse = c.get(format!("{}/v1/jobs{}", c.base, q)).await?;
            jobs_from_pb(response).map_err(bad_answer)
        })
    }

    fn job(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        self.call(move |c| async move {
            let q = parameters(&[("user", user)]);
            let response: pb::GetJobResponse = c.get(format!("{}/v1/jobs/{}{}", c.base, id, q)).await?;
            job_from_pb(response.job).map_err(bad_answer)
        })
    }

    fn cancel_job(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        self.call(move |c| async move {
            let url = format!("{}/v1/jobs/{}/cancel", c.base, id);
            let response: pb::CancelJobResponse = c.post(url, &pb::CancelJobRequest { id, user }).await?;
            job_from_pb(response.job).map_err(bad_answer)
        })
    }

    fn job_result(
        &self,
        id: u64,
        user: Option<String>,
        offset: u64,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<JobPage, Error>> + Send {
        self.call(move |c| async move {
            let q = parameters(&[
                ("user", user),
                ("offset", Some(offset.to_string())),
                ("limit", limit.map(|n| n.min(u32::MAX as usize).to_string())),
            ]);
            let response: pb::GetJobResultResponse = c.get(format!("{}/v1/jobs/{}/result{}", c.base, id, q)).await?;
            job_page_from_pb(response).map_err(bad_answer)
        })
    }
}

impl Accounts for RestRemote {
    fn users(&self) -> impl Future<Output = Result<Vec<UserInfo>, Error>> + Send {
        self.call(move |c| async move {
            let response: pb::ListUsersResponse = c.get(format!("{}/v1/users", c.base)).await?;
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
        self.call(move |c| async move {
            let request = pb::CreateUserRequest { name, password: password.expose().to_owned(), admin };
            let response: pb::CreateUserResponse = c.post(format!("{}/v1/users", c.base), &request).await?;
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
        self.call(move |c| async move {
            let request = pb::SetPasswordRequest {
                name: String::new(),
                password: password.expose().to_owned(),
                current_password: current.map(|c| c.expose().to_owned()),
            };
            let _: pb::SetPasswordResponse = c.json(Method::PUT, c.user_url(&name, "/password"), &request).await?;
            Ok(())
        })
    }

    fn delete_user(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let name = name.to_owned();
        self.call(move |c| async move {
            let _: pb::DeleteUserResponse =
                c.json(Method::DELETE, c.user_url(&name, ""), &pb::DeleteUserRequest::default()).await?;
            Ok(())
        })
    }

    fn set_admin(&self, name: &str, admin: bool) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let name = name.to_owned();
        self.call(move |c| async move {
            let request = pb::SetAdminRequest { name: String::new(), admin };
            let response: pb::SetAdminResponse = c.json(Method::PUT, c.user_url(&name, "/admin"), &request).await?;
            user_from_pb(response.user).map_err(bad_answer)
        })
    }

    fn grant(&self, name: &str, namespace: &str, role: Role) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let (name, namespace) = (name.to_owned(), namespace.to_owned());
        self.call(move |c| async move {
            let request = pb::GrantRequest { role: role_to_pb(role) as i32, ..pb::GrantRequest::default() };
            let url = c.user_url(&name, &format!("/grants/{}", segment(&namespace)));
            let response: pb::GrantResponse = c.json(Method::PUT, url, &request).await?;
            user_from_pb(response.user).map_err(bad_answer)
        })
    }

    fn revoke(&self, name: &str, namespace: &str) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let (name, namespace) = (name.to_owned(), namespace.to_owned());
        self.call(move |c| async move {
            let url = c.user_url(&name, &format!("/grants/{}", segment(&namespace)));
            let response: pb::RevokeResponse = c.json(Method::DELETE, url, &pb::RevokeRequest::default()).await?;
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
        self.call(move |c| async move {
            let request =
                pb::CreateTokenRequest { user: String::new(), name, expires_in_secs: expires_in.map(|d| d.as_secs()) };
            let response: pb::CreateTokenResponse = c.post(c.user_url(&user, "/tokens"), &request).await?;
            new_token_from_pb(response).map_err(bad_answer)
        })
    }

    fn revoke_token(&self, user: &str, name: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let (user, name) = (user.to_owned(), name.to_owned());
        self.call(move |c| async move {
            let url = c.user_url(&user, &format!("/tokens/{}", segment(&name)));
            let _: pb::RevokeTokenResponse = c.json(Method::DELETE, url, &pb::RevokeTokenRequest::default()).await?;
            Ok(())
        })
    }

    fn tokens(&self, user: &str) -> impl Future<Output = Result<Vec<TokenInfo>, Error>> + Send {
        let user = user.to_owned();
        self.call(move |c| async move {
            let response: pb::ListTokensResponse = c.get(c.user_url(&user, "/tokens")).await?;
            Ok(response.tokens.into_iter().map(token_info_from_pb).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_and_queries_are_percent_encoded() {
        assert_eq!(segment("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(segment("a/b ü?"), "a%2Fb%20%C3%BC%3F");
        assert_eq!(query(&QueryOptions::default()), "");
        let o = QueryOptions {
            min_seq: Some(3),
            partial: true,
            cursor: Some(iwdb_query::Cursor::new("a b")),
            ..Default::default()
        };
        assert_eq!(query(&o), "?min_seq=3&partial=true&cursor=a%20b");
    }
}
