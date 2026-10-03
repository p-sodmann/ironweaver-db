//! [`Server`]: the `DatabaseService` of `proto/ironweaver_db/v1` over any
//! [`Database`]. Every RPC decodes its request, makes one trait call, and
//! encodes the answer (design rule 8); a failure at any step is the error's
//! status ([`crate::status`]).

use std::sync::Arc;
use std::time::Duration;

use iwdb_query::{Database, Error};
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use crate::convert::*;
use crate::proto as pb;
use crate::proto::database_service_server::{DatabaseService, DatabaseServiceServer};
use crate::status::to_status;

/// A streamed answer's chunks stay below about this many bytes each, unless
/// one item is bigger (ADR 0025).
pub const CHUNK_BYTES: usize = 1 << 20;

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
}

impl<D: Database + 'static> Server<D> {
    pub fn new(db: Arc<D>) -> Self {
        Server { db, max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES }
    }

    /// Requests and answer messages above this size fail (default
    /// [`DEFAULT_MAX_MESSAGE_BYTES`]). Streamed answers are sent in chunks of
    /// about [`CHUNK_BYTES`].
    pub fn max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes;
        self
    }

    pub fn database(&self) -> &Arc<D> {
        &self.db
    }

    /// The tonic service (a tower `Service` of HTTP requests), to serve
    /// together with other services (step 12 adds REST next to it).
    pub fn service(&self) -> DatabaseServiceServer<Adapter<D>> {
        DatabaseServiceServer::from_arc(Arc::new(Adapter { db: self.db.clone() }))
            .max_decoding_message_size(self.max_message_bytes)
            .max_encoding_message_size(self.max_message_bytes)
    }
}

/// The handlers of `DatabaseService` over a database ([`Server::service`]).
pub struct Adapter<D> {
    db: Arc<D>,
}

type Res<T> = Result<Response<T>, Status>;

/// A streamed answer: its chunks, all built before the first is sent.
pub(crate) type Chunks<T> = tokio_stream::Iter<std::vec::IntoIter<Result<T, Status>>>;

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

/// The request's options, with the header's deadline.
fn read_options<T>(
    request: &Request<T>,
    options: Option<pb::QueryOptions>,
) -> Result<iwdb_query::QueryOptions, Status> {
    options_from_pb(options, grpc_timeout(request.metadata())).map_err(fail)
}

/// Split `items` into chunks of about [`CHUNK_BYTES`] by `len` (at least one
/// item per chunk, and one empty chunk for no items).
fn split<T>(items: Vec<T>, len: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut bytes = 0;
    for item in items {
        // The item's own bytes, plus its field tag and length prefix
        let n = len(&item) + 8;
        if !current.is_empty() && bytes + n > CHUNK_BYTES {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += n;
        current.push(item);
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn message_len<T: prost::Message>(item: &T) -> usize {
    item.encoded_len()
}

/// The stream of `responses`, with `meta` in the last one (an empty answer
/// is one message with only `meta`).
fn finish<R: Default>(
    mut responses: Vec<R>,
    meta: pb::AnswerMeta,
    slot: impl FnOnce(&mut R) -> &mut Option<pb::AnswerMeta>,
) -> Chunks<R> {
    if responses.is_empty() {
        responses.push(R::default());
    }
    if let Some(last) = responses.last_mut() {
        *slot(last) = Some(meta);
    }
    tokio_stream::iter(responses.into_iter().map(Ok).collect::<Vec<_>>())
}

#[tonic::async_trait]
impl<D: Database + 'static> DatabaseService for Adapter<D> {
    async fn commit(&self, request: Request<pb::CommitRequest>) -> Res<pb::CommitResponse> {
        let r = request.into_inner();
        let mutations = mutations_from_pb(r.mutations).map_err(fail)?;
        let options = commit_options_from_pb(r.options).map_err(fail)?;
        let result = self.db.commit(&r.namespace, mutations, options).await.map_err(fail)?;
        Ok(Response::new(pb::CommitResponse { result: Some(commit_result_to_pb(&result)) }))
    }

    async fn commit_catalog(&self, request: Request<pb::CommitCatalogRequest>) -> Res<pb::CommitCatalogResponse> {
        let r = request.into_inner();
        let change = catalog_change_from_pb(r.change).map_err(fail)?;
        let options = commit_options_from_pb(r.options).map_err(fail)?;
        let result = self.db.commit_catalog(&r.namespace, change, options).await.map_err(fail)?;
        Ok(Response::new(pb::CommitCatalogResponse { result: Some(commit_result_to_pb(&result)) }))
    }

    async fn wait_for_seq(&self, request: Request<pb::WaitForSeqRequest>) -> Res<pb::WaitForSeqResponse> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let seq = self.db.wait_for_seq(&r.namespace, r.seq, options).await.map_err(fail)?;
        Ok(Response::new(pb::WaitForSeqResponse { seq }))
    }

    type GetNodesStream = Chunks<pb::GetNodesResponse>;

    async fn get_nodes(&self, request: Request<pb::GetNodesRequest>) -> Res<Self::GetNodesStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let answer = self.db.get_nodes(&r.namespace, r.ids, options).await.map_err(fail)?;
        let items = maybe_nodes_to_pb(&answer.value).map_err(fail)?;
        let responses = split(items, message_len).into_iter().map(|nodes| pb::GetNodesResponse { nodes, meta: None });
        Ok(Response::new(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta)))
    }

    type GetEdgesStream = Chunks<pb::GetEdgesResponse>;

    async fn get_edges(&self, request: Request<pb::GetEdgesRequest>) -> Res<Self::GetEdgesStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let ids = r.ids.into_iter().map(ironweaver_core::EdgeId).collect();
        let answer = self.db.get_edges(&r.namespace, ids, options).await.map_err(fail)?;
        let items = maybe_edges_to_pb(&answer.value).map_err(fail)?;
        let responses = split(items, message_len).into_iter().map(|edges| pb::GetEdgesResponse { edges, meta: None });
        Ok(Response::new(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta)))
    }

    type FindStream = Chunks<pb::FindResponse>;

    async fn find(&self, request: Request<pb::FindRequest>) -> Res<Self::FindStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let find = find_from_pb(r.filter).map_err(fail)?;
        let answer = self.db.find(&r.namespace, find, options).await.map_err(fail)?;
        let items = nodes_to_pb(&answer.value).map_err(fail)?;
        let responses = split(items, message_len).into_iter().map(|nodes| pb::FindResponse { nodes, meta: None });
        Ok(Response::new(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta)))
    }

    async fn explain(&self, request: Request<pb::ExplainRequest>) -> Res<pb::ExplainResponse> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let explain = explain_from_pb(r.filter, r.analyze).map_err(fail)?;
        let answer = self.db.explain(&r.namespace, explain, options).await.map_err(fail)?;
        Ok(Response::new(pb::ExplainResponse {
            explain: Some(explain_to_pb(&answer.value)),
            meta: Some(meta_to_pb(&answer)),
        }))
    }

    type NeighbourhoodStream = Chunks<pb::NeighbourhoodResponse>;

    async fn neighbourhood(&self, request: Request<pb::NeighbourhoodRequest>) -> Res<Self::NeighbourhoodStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let namespace = r.namespace.clone();
        let neighbourhood = neighbourhood_from_pb(r).map_err(fail)?;
        let answer = self.db.neighbourhood(&namespace, neighbourhood, options).await.map_err(fail)?;
        let items = nodes_to_pb(&answer.value).map_err(fail)?;
        let responses =
            split(items, message_len).into_iter().map(|nodes| pb::NeighbourhoodResponse { nodes, meta: None });
        Ok(Response::new(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta)))
    }

    type TraverseStream = Chunks<pb::TraverseResponse>;

    async fn traverse(&self, request: Request<pb::TraverseRequest>) -> Res<Self::TraverseStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let namespace = r.namespace.clone();
        let traverse = traverse_from_pb(r).map_err(fail)?;
        let answer = self.db.traverse(&namespace, traverse, options).await.map_err(fail)?;
        let meta = meta_to_pb(&answer);
        let responses =
            split(answer.value, String::len).into_iter().map(|ids| pb::TraverseResponse { ids, meta: None });
        Ok(Response::new(finish(responses.collect(), meta, |r| &mut r.meta)))
    }

    async fn shortest_path(&self, request: Request<pb::ShortestPathRequest>) -> Res<pb::ShortestPathResponse> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let namespace = r.namespace.clone();
        let path = path_request_from_pb(r).map_err(fail)?;
        let answer = self.db.shortest_path(&namespace, path, options).await.map_err(fail)?;
        Ok(Response::new(pb::ShortestPathResponse {
            path: answer.value.as_ref().map(path_to_answer_pb),
            meta: Some(meta_to_pb(&answer)),
        }))
    }

    type RandomWalksStream = Chunks<pb::RandomWalksResponse>;

    async fn random_walks(&self, request: Request<pb::RandomWalksRequest>) -> Res<Self::RandomWalksStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let namespace = r.namespace.clone();
        let answer = self.db.random_walks(&namespace, walk_from_pb(r), options).await.map_err(fail)?;
        let items = walks_to_pb(&answer.value);
        let responses =
            split(items, message_len).into_iter().map(|walks| pb::RandomWalksResponse { walks, meta: None });
        Ok(Response::new(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta)))
    }

    type SubgraphStream = Chunks<pb::SubgraphResponse>;

    async fn subgraph(&self, request: Request<pb::SubgraphRequest>) -> Res<Self::SubgraphStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let namespace = r.namespace.clone();
        let subgraph = subgraph_from_pb(r).map_err(fail)?;
        let answer = self.db.subgraph(&namespace, subgraph, options).await.map_err(fail)?;
        let nodes = nodes_to_pb(&answer.value.nodes).map_err(fail)?;
        let edges = edges_to_pb(&answer.value.edges).map_err(fail)?;
        let mut responses: Vec<pb::SubgraphResponse> = split(nodes, message_len)
            .into_iter()
            .filter(|chunk| !chunk.is_empty())
            .map(|nodes| pb::SubgraphResponse { nodes, ..Default::default() })
            .collect();
        responses.extend(
            split(edges, message_len)
                .into_iter()
                .filter(|chunk| !chunk.is_empty())
                .map(|edges| pb::SubgraphResponse { edges, ..Default::default() }),
        );
        Ok(Response::new(finish(responses, meta_to_pb(&answer), |r| &mut r.meta)))
    }

    type MatchPatternStream = Chunks<pb::MatchPatternResponse>;

    async fn match_pattern(&self, request: Request<pb::MatchPatternRequest>) -> Res<Self::MatchPatternStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let namespace = r.namespace.clone();
        let matching = match_from_pb(r).map_err(fail)?;
        let answer = self.db.match_pattern(&namespace, matching, options).await.map_err(fail)?;
        let items: Vec<pb::MatchRow> = answer.value.iter().map(match_row_to_pb).collect();
        let responses = split(items, message_len).into_iter().map(|rows| pb::MatchPatternResponse { rows, meta: None });
        Ok(Response::new(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta)))
    }

    type AnalyzeStream = Chunks<pb::AnalyzeResponse>;

    async fn analyze(&self, request: Request<pb::AnalyzeRequest>) -> Res<Self::AnalyzeStream> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let namespace = r.namespace.clone();
        let analytics = analyze_from_pb(r).map_err(fail)?;
        let answer = self.db.analyze(&namespace, analytics, options).await.map_err(fail)?;
        let rows = job_result_to_pb(&answer.value);
        let kind = i32::from(rows.kind);
        let chunk = || pb::AnalyzeResponse { kind, ..Default::default() };
        let mut responses: Vec<pb::AnalyzeResponse> = Vec::new();
        for scores in split(rows.scores, message_len).into_iter().filter(|c| !c.is_empty()) {
            responses.push(pb::AnalyzeResponse { scores, ..chunk() });
        }
        for groups in split(rows.groups, message_len).into_iter().filter(|c| !c.is_empty()) {
            responses.push(pb::AnalyzeResponse { groups, ..chunk() });
        }
        for counts in split(rows.counts, message_len).into_iter().filter(|c| !c.is_empty()) {
            responses.push(pb::AnalyzeResponse { counts, ..chunk() });
        }
        if responses.is_empty() {
            responses.push(chunk());
        }
        Ok(Response::new(finish(responses, meta_to_pb(&answer), |r| &mut r.meta)))
    }

    async fn get_catalog(&self, request: Request<pb::GetCatalogRequest>) -> Res<pb::GetCatalogResponse> {
        let options = read_options(&request, request.get_ref().options.clone())?;
        let r = request.into_inner();
        let answer = self.db.catalog(&r.namespace, options).await.map_err(fail)?;
        Ok(Response::new(pb::GetCatalogResponse {
            catalog: Some(catalog_to_pb(&answer.value)),
            meta: Some(meta_to_pb(&answer)),
        }))
    }

    async fn get_namespace_status(
        &self,
        request: Request<pb::GetNamespaceStatusRequest>,
    ) -> Res<pb::GetNamespaceStatusResponse> {
        let r = request.into_inner();
        let status = self.db.namespace_status(&r.namespace).await.map_err(fail)?;
        Ok(Response::new(pb::GetNamespaceStatusResponse { status: Some(status_to_pb(&status)) }))
    }

    async fn list_namespaces(&self, _request: Request<pb::ListNamespacesRequest>) -> Res<pb::ListNamespacesResponse> {
        let namespaces = self.db.namespaces().await.map_err(fail)?;
        Ok(Response::new(pb::ListNamespacesResponse {
            namespaces: namespaces.iter().map(namespace_info_to_pb).collect(),
        }))
    }

    async fn create_namespace(&self, request: Request<pb::CreateNamespaceRequest>) -> Res<pb::CreateNamespaceResponse> {
        let r = request.into_inner();
        let key = idempotency_key_from_pb(r.idempotency_key).map_err(fail)?;
        let result = self.db.create_namespace(&r.name, key).await.map_err(fail)?;
        Ok(Response::new(pb::CreateNamespaceResponse {
            event: Some(event_to_pb(&result.event)),
            deduplicated: result.deduplicated,
        }))
    }

    async fn drop_namespace(&self, request: Request<pb::DropNamespaceRequest>) -> Res<pb::DropNamespaceResponse> {
        let r = request.into_inner();
        let key = idempotency_key_from_pb(r.idempotency_key).map_err(fail)?;
        let result = self.db.drop_namespace(&r.name, key).await.map_err(fail)?;
        Ok(Response::new(pb::DropNamespaceResponse {
            event: Some(event_to_pb(&result.event)),
            deduplicated: result.deduplicated,
        }))
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

    #[test]
    fn chunks_stay_below_the_size_and_keep_every_item() {
        let items: Vec<String> = (0..10_000).map(|i| format!("{:0>300}", i)).collect();
        let chunks = split(items.clone(), String::len);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.iter().map(|s| s.len() + 8).sum::<usize>() <= CHUNK_BYTES));
        assert_eq!(chunks.concat(), items);
        // An item bigger than a chunk gets one of its own
        let big = vec!["x".repeat(CHUNK_BYTES + 1), "y".into()];
        assert_eq!(split(big, String::len).len(), 2);
        assert_eq!(split(Vec::<String>::new(), String::len), vec![Vec::<String>::new()]);
    }
}
