//! Every operation of `proto/ironweaver_db/v1`, once, for both APIs: decode
//! the request message, make one [`Database`] call, encode the answer
//! (design rule 8). gRPC ([`crate::service`]) and REST ([`crate::rest`])
//! only add their transport: headers, deadlines, streams, error statuses.
//!
//! A streamed answer is its chunks, all built before the first is sent
//! (ADR 0025): every chunk has the same item fields, each holds about
//! [`CHUNK_BYTES`] at most (one item may be bigger), and only the last has
//! `meta`.

use std::sync::Arc;
use std::time::Duration;

use iwdb_query::audit::Audit;
use iwdb_query::{Accounts, Authenticate, Authorized, ChangesRequest, Code, Database, Error, Secret, Session};
use tokio::sync::{mpsc, watch};

use crate::auth::AuthMode;
use crate::convert::*;
use crate::proto as pb;

/// A streamed answer's chunks stay below about this many bytes each, unless
/// one item is bigger (ADR 0025).
pub const CHUNK_BYTES: usize = 1 << 20;

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

/// `responses` with `meta` in the last one (an empty answer is one message
/// with only `meta`).
fn finish<R: Default>(
    mut responses: Vec<R>,
    meta: pb::AnswerMeta,
    slot: impl FnOnce(&mut R) -> &mut Option<pb::AnswerMeta>,
) -> Vec<R> {
    if responses.is_empty() {
        responses.push(R::default());
    }
    if let Some(last) = responses.last_mut() {
        *slot(last) = Some(meta);
    }
    responses
}

pub(crate) async fn commit<D: Database>(db: &D, r: pb::CommitRequest) -> Result<pb::CommitResponse, Error> {
    let mutations = mutations_from_pb(r.mutations)?;
    let options = commit_options_from_pb(r.options)?;
    let result = db.commit(&r.namespace, mutations, options).await?;
    Ok(pb::CommitResponse { result: Some(commit_result_to_pb(&result)) })
}

pub(crate) async fn commit_catalog<D: Database>(
    db: &D,
    r: pb::CommitCatalogRequest,
) -> Result<pb::CommitCatalogResponse, Error> {
    let change = catalog_change_from_pb(r.change)?;
    let options = commit_options_from_pb(r.options)?;
    let result = db.commit_catalog(&r.namespace, change, options).await?;
    Ok(pb::CommitCatalogResponse { result: Some(commit_result_to_pb(&result)) })
}

pub(crate) async fn wait_for_seq<D: Database>(
    db: &D,
    r: pb::WaitForSeqRequest,
    deadline: Option<Duration>,
) -> Result<pb::WaitForSeqResponse, Error> {
    let options = options_from_pb(r.options, deadline)?;
    let seq = db.wait_for_seq(&r.namespace, r.seq, options).await?;
    Ok(pb::WaitForSeqResponse { seq })
}

pub(crate) async fn get_nodes<D: Database>(
    db: &D,
    r: pb::GetNodesRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::GetNodesResponse>, Error> {
    let options = options_from_pb(r.options, deadline)?;
    let answer = db.get_nodes(&r.namespace, r.ids, options).await?;
    let items = maybe_nodes_to_pb(&answer.value)?;
    let responses = split(items, message_len).into_iter().map(|nodes| pb::GetNodesResponse { nodes, meta: None });
    Ok(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn get_edges<D: Database>(
    db: &D,
    r: pb::GetEdgesRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::GetEdgesResponse>, Error> {
    let options = options_from_pb(r.options, deadline)?;
    let ids = r.ids.into_iter().map(ironweaver_core::EdgeId).collect();
    let answer = db.get_edges(&r.namespace, ids, options).await?;
    let items = maybe_edges_to_pb(&answer.value)?;
    let responses = split(items, message_len).into_iter().map(|edges| pb::GetEdgesResponse { edges, meta: None });
    Ok(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn find<D: Database>(
    db: &D,
    r: pb::FindRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::FindResponse>, Error> {
    let options = options_from_pb(r.options, deadline)?;
    let find = find_from_pb(r.filter)?;
    let answer = db.find(&r.namespace, find, options).await?;
    let items = nodes_to_pb(&answer.value)?;
    let responses = split(items, message_len).into_iter().map(|nodes| pb::FindResponse { nodes, meta: None });
    Ok(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn explain<D: Database>(
    db: &D,
    r: pb::ExplainRequest,
    deadline: Option<Duration>,
) -> Result<pb::ExplainResponse, Error> {
    let options = options_from_pb(r.options, deadline)?;
    let explain = explain_from_pb(r.filter, r.analyze)?;
    let answer = db.explain(&r.namespace, explain, options).await?;
    Ok(pb::ExplainResponse { explain: Some(explain_to_pb(&answer.value)), meta: Some(meta_to_pb(&answer)) })
}

pub(crate) async fn neighbourhood<D: Database>(
    db: &D,
    mut r: pb::NeighbourhoodRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::NeighbourhoodResponse>, Error> {
    let options = options_from_pb(r.options.take(), deadline)?;
    let namespace = std::mem::take(&mut r.namespace);
    let neighbourhood = neighbourhood_from_pb(r)?;
    let answer = db.neighbourhood(&namespace, neighbourhood, options).await?;
    let items = nodes_to_pb(&answer.value)?;
    let responses = split(items, message_len).into_iter().map(|nodes| pb::NeighbourhoodResponse { nodes, meta: None });
    Ok(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn traverse<D: Database>(
    db: &D,
    mut r: pb::TraverseRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::TraverseResponse>, Error> {
    let options = options_from_pb(r.options.take(), deadline)?;
    let namespace = std::mem::take(&mut r.namespace);
    let traverse = traverse_from_pb(r)?;
    let answer = db.traverse(&namespace, traverse, options).await?;
    let meta = meta_to_pb(&answer);
    let responses = split(answer.value, String::len).into_iter().map(|ids| pb::TraverseResponse { ids, meta: None });
    Ok(finish(responses.collect(), meta, |r| &mut r.meta))
}

pub(crate) async fn shortest_path<D: Database>(
    db: &D,
    mut r: pb::ShortestPathRequest,
    deadline: Option<Duration>,
) -> Result<pb::ShortestPathResponse, Error> {
    let options = options_from_pb(r.options.take(), deadline)?;
    let namespace = std::mem::take(&mut r.namespace);
    let path = path_request_from_pb(r)?;
    let answer = db.shortest_path(&namespace, path, options).await?;
    Ok(pb::ShortestPathResponse { path: answer.value.as_ref().map(path_to_answer_pb), meta: Some(meta_to_pb(&answer)) })
}

pub(crate) async fn random_walks<D: Database>(
    db: &D,
    mut r: pb::RandomWalksRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::RandomWalksResponse>, Error> {
    let options = options_from_pb(r.options.take(), deadline)?;
    let namespace = std::mem::take(&mut r.namespace);
    let answer = db.random_walks(&namespace, walk_from_pb(r), options).await?;
    let items = walks_to_pb(&answer.value);
    let responses = split(items, message_len).into_iter().map(|walks| pb::RandomWalksResponse { walks, meta: None });
    Ok(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn subgraph<D: Database>(
    db: &D,
    mut r: pb::SubgraphRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::SubgraphResponse>, Error> {
    let options = options_from_pb(r.options.take(), deadline)?;
    let namespace = std::mem::take(&mut r.namespace);
    let subgraph = subgraph_from_pb(r)?;
    let answer = db.subgraph(&namespace, subgraph, options).await?;
    let nodes = nodes_to_pb(&answer.value.nodes)?;
    let edges = edges_to_pb(&answer.value.edges)?;
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
    Ok(finish(responses, meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn match_pattern<D: Database>(
    db: &D,
    mut r: pb::MatchPatternRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::MatchPatternResponse>, Error> {
    let options = options_from_pb(r.options.take(), deadline)?;
    let namespace = std::mem::take(&mut r.namespace);
    let matching = match_from_pb(r)?;
    let answer = db.match_pattern(&namespace, matching, options).await?;
    let items: Vec<pb::MatchRow> = answer.value.iter().map(match_row_to_pb).collect();
    let responses = split(items, message_len).into_iter().map(|rows| pb::MatchPatternResponse { rows, meta: None });
    Ok(finish(responses.collect(), meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn analyze<D: Database>(
    db: &D,
    mut r: pb::AnalyzeRequest,
    deadline: Option<Duration>,
) -> Result<Vec<pb::AnalyzeResponse>, Error> {
    let options = options_from_pb(r.options.take(), deadline)?;
    let namespace = std::mem::take(&mut r.namespace);
    let analytics = analyze_from_pb(r)?;
    let answer = db.analyze(&namespace, analytics, options).await?;
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
    Ok(finish(responses, meta_to_pb(&answer), |r| &mut r.meta))
}

pub(crate) async fn get_catalog<D: Database>(
    db: &D,
    r: pb::GetCatalogRequest,
    deadline: Option<Duration>,
) -> Result<pb::GetCatalogResponse, Error> {
    let options = options_from_pb(r.options, deadline)?;
    let answer = db.catalog(&r.namespace, options).await?;
    Ok(pb::GetCatalogResponse { catalog: Some(catalog_to_pb(&answer.value)), meta: Some(meta_to_pb(&answer)) })
}

pub(crate) async fn get_namespace_status<D: Database>(
    db: &D,
    r: pb::GetNamespaceStatusRequest,
) -> Result<pb::GetNamespaceStatusResponse, Error> {
    let status = db.namespace_status(&r.namespace).await?;
    Ok(pb::GetNamespaceStatusResponse { status: Some(status_to_pb(&status)) })
}

pub(crate) async fn list_namespaces<D: Database>(db: &D) -> Result<pb::ListNamespacesResponse, Error> {
    let namespaces = db.namespaces().await?;
    Ok(pb::ListNamespacesResponse { namespaces: namespaces.iter().map(namespace_info_to_pb).collect() })
}

pub(crate) async fn create_namespace<D: Database>(
    db: &D,
    r: pb::CreateNamespaceRequest,
) -> Result<pb::CreateNamespaceResponse, Error> {
    let key = idempotency_key_from_pb(r.idempotency_key)?;
    let result = db.create_namespace(&r.name, key).await?;
    Ok(pb::CreateNamespaceResponse { event: Some(event_to_pb(&result.event)), deduplicated: result.deduplicated })
}

pub(crate) async fn drop_namespace<D: Database>(
    db: &D,
    r: pb::DropNamespaceRequest,
) -> Result<pb::DropNamespaceResponse, Error> {
    let key = idempotency_key_from_pb(r.idempotency_key)?;
    let result = db.drop_namespace(&r.name, key).await?;
    Ok(pb::DropNamespaceResponse { event: Some(event_to_pb(&result.event)), deduplicated: result.deduplicated })
}

pub(crate) async fn get_changes<D: Database>(
    db: &D,
    r: pb::GetChangesRequest,
    deadline: Option<Duration>,
) -> Result<pb::GetChangesResponse, Error> {
    let options = options_from_pb(r.options, deadline)?;
    let request = ChangesRequest { from_seq: r.from_seq, wait: r.wait };
    changes_to_pb(&db.changes(&r.namespace, request, options).await?)
}

/// Batches of a followed change stream ([`follow`]): it ends after an
/// error.
pub(crate) type Follow = mpsc::Receiver<Result<pb::GetChangesResponse, Error>>;

/// Follow the change stream (ADR 0031), for `Watch` and the SSE route: a
/// first batch without waiting (so that an error, such as `not_retained`,
/// comes before any event), then `GetChanges` with `wait` in a loop, each
/// from the `next_seq` of the batch before, with `r.options` (whose
/// timeout is then how long a round waits; a round that finds nothing
/// yields an empty batch, a heartbeat). A waiting round that times out
/// (it had no time left to read) is repeated.
///
/// Runs in a task of its own until the receiver is dropped (the client
/// went away), an error (the last item), or `stopping` turns true (the
/// server is shutting down: `unavailable`).
pub(crate) fn follow<D: Database + 'static>(
    db: Arc<D>,
    r: pb::WatchRequest,
    deadline: Option<Duration>,
    mut stopping: watch::Receiver<bool>,
) -> Follow {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut from_seq = r.from_seq;
        let mut wait = false;
        loop {
            let request =
                pb::GetChangesRequest { namespace: r.namespace.clone(), from_seq, wait, options: r.options.clone() };
            let result = tokio::select! {
                result = get_changes(&*db, request, deadline) => result,
                _ = stopping.wait_for(|s| *s) => Err(Error::unavailable("the server is shutting down")),
                () = tx.closed() => return,
            };
            let batch = match result {
                Ok(batch) => batch,
                // A round that waited and had no time left to read: again,
                // after a pause (a tiny timeout mustn't spin)
                Err(e) if wait && e.code() == Code::Timeout => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            from_seq = batch.next_seq;
            wait = true;
            if tx.send(Ok(batch)).await.is_err() {
                return;
            }
        }
    });
    rx
}

// ---- authentication, users, grants, tokens (auth.proto, step 15a) ----

/// `Login`: on the database itself (no principal yet), audited.
pub(crate) async fn login<D: Authenticate>(
    db: &D,
    r: pb::LoginRequest,
    audit: &Audit,
) -> Result<(pb::LoginResponse, Session), Error> {
    if r.user.is_empty() {
        return Err(Error::invalid("a login needs a user"));
    }
    let session = iwdb_query::auth::login(db, audit, &r.user, Secret::new(r.password)).await?;
    let response = pb::LoginResponse {
        token: if r.cookie { String::new() } else { session.token.expose().to_owned() },
        user: Some(user_to_pb(&session.user)),
        expires_ms: session.expires_ms,
    };
    Ok((response, session))
}

/// `Logout`: end the session of the caller's token.
pub(crate) async fn logout<D: Authenticate>(
    db: &Authorized<D>,
    token: Option<&Secret>,
) -> Result<pb::LogoutResponse, Error> {
    db.logout(token).await?;
    Ok(pb::LogoutResponse {})
}

pub(crate) fn who_am_i<D>(db: &Authorized<D>, mode: AuthMode) -> pb::WhoAmIResponse {
    pb::WhoAmIResponse { user: Some(user_to_pb(&db.whoami())), auth_enabled: mode.enabled }
}

pub(crate) async fn list_users<D: Accounts>(db: &D) -> Result<pb::ListUsersResponse, Error> {
    Ok(pb::ListUsersResponse { users: db.users().await?.iter().map(user_to_pb).collect() })
}

pub(crate) async fn create_user<D: Accounts>(
    db: &D,
    r: pb::CreateUserRequest,
) -> Result<pb::CreateUserResponse, Error> {
    let user = db.create_user(&r.name, Secret::new(r.password), r.admin).await?;
    Ok(pb::CreateUserResponse { user: Some(user_to_pb(&user)) })
}

pub(crate) async fn set_password<D: Accounts>(
    db: &D,
    r: pb::SetPasswordRequest,
) -> Result<pb::SetPasswordResponse, Error> {
    db.set_password(&r.name, Secret::new(r.password), r.current_password.map(Secret::new)).await?;
    Ok(pb::SetPasswordResponse {})
}

pub(crate) async fn delete_user<D: Accounts>(
    db: &D,
    r: pb::DeleteUserRequest,
) -> Result<pb::DeleteUserResponse, Error> {
    db.delete_user(&r.name).await?;
    Ok(pb::DeleteUserResponse {})
}

pub(crate) async fn set_admin<D: Accounts>(db: &D, r: pb::SetAdminRequest) -> Result<pb::SetAdminResponse, Error> {
    let user = db.set_admin(&r.name, r.admin).await?;
    Ok(pb::SetAdminResponse { user: Some(user_to_pb(&user)) })
}

pub(crate) async fn grant<D: Accounts>(db: &D, r: pb::GrantRequest) -> Result<pb::GrantResponse, Error> {
    let user = db.grant(&r.name, &r.namespace, role_from_pb(r.role)?).await?;
    Ok(pb::GrantResponse { user: Some(user_to_pb(&user)) })
}

pub(crate) async fn revoke<D: Accounts>(db: &D, r: pb::RevokeRequest) -> Result<pb::RevokeResponse, Error> {
    let user = db.revoke(&r.name, &r.namespace).await?;
    Ok(pb::RevokeResponse { user: Some(user_to_pb(&user)) })
}

pub(crate) async fn create_token<D: Accounts>(
    db: &D,
    r: pb::CreateTokenRequest,
) -> Result<pb::CreateTokenResponse, Error> {
    let token = db.create_token(&r.user, &r.name, r.expires_in_secs.map(Duration::from_secs)).await?;
    Ok(new_token_to_pb(&token))
}

pub(crate) async fn revoke_token<D: Accounts>(
    db: &D,
    r: pb::RevokeTokenRequest,
) -> Result<pb::RevokeTokenResponse, Error> {
    db.revoke_token(&r.user, &r.name).await?;
    Ok(pb::RevokeTokenResponse {})
}

pub(crate) async fn list_tokens<D: Accounts>(
    db: &D,
    r: pb::ListTokensRequest,
) -> Result<pb::ListTokensResponse, Error> {
    Ok(pb::ListTokensResponse { tokens: db.tokens(&r.user).await?.iter().map(token_info_to_pb).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

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
