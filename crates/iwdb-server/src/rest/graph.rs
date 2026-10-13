//! Namespace and graph-operation handlers.

use super::transport::{Answer, OptionsQuery, merged, namespace, options, path, read, streamed, unary};
use super::{Caller_, St};
use crate::auth::Served;
use crate::{ops, proto as pb};
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use http::HeaderMap;
use iwdb_query::{Code, Error};

// ---- handlers ----

pub(super) async fn list_namespaces<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::list_namespaces(&s.db(&caller)?).await)
}

pub(super) async fn create_namespace<D: Served>(
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

pub(super) async fn drop_namespace<D: Served>(
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

pub(super) async fn namespace_status<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
) -> Answer {
    let r = pb::GetNamespaceStatusRequest { namespace: path(p)? };
    unary(ops::get_namespace_status(&s.db(&caller)?, r).await)
}

pub(super) async fn catalog<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<OptionsQuery>, QueryRejection>,
) -> Answer {
    let r = pb::GetCatalogRequest { namespace: path(p)?, options: options(q)? };
    unary(ops::get_catalog(&s.db(&caller)?, r, None).await)
}

pub(super) async fn schema<D: Served>(
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
        pub(super) async fn $name<D: Served>(
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
body_handler!(start_job, pb::StartJobRequest, |db, r, _h| unary(ops::start_job(&db, r).await));
body_handler!(set_namespace_limits, pb::SetNamespaceLimitsRequest, |db, r, _h| unary(
    ops::set_namespace_limits(&db, r).await
));

/// One node by id: its `GetNodesResponse`, or 404 if it doesn't exist.
pub(super) async fn get_node<D: Served>(
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
pub(super) async fn get_edge<D: Served>(
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
