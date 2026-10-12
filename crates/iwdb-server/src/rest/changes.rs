//! Polling and SSE change-stream handlers.

use super::transport::{Answer, ChangesQuery, changes_query, path, unary};
use super::{Caller_, EVENT_STREAM, St};
use crate::auth::Served;
use crate::{ops, proto as pb};
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, header};
use iwdb_query::Error;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

pub(super) async fn get_changes<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<ChangesQuery>, QueryRejection>,
) -> Answer {
    let (from_seq, wait, options) = changes_query(q, false)?;
    let r = pb::GetChangesRequest { namespace: path(p)?, from_seq, wait, options };
    unary(ops::get_changes(&s.db(&caller)?, r, None).await)
}

/// The change stream as Server-Sent Events (ADR 0031): a `change` event per
/// commit, with the seq as its `id` and the `ChangeEvent` as its data; a
/// comment line as heartbeat after a round without commits; an `error`
/// event with an `Error` before the stream ends on an error (`unavailable`
/// at shutdown). A `Last-Event-ID` header (what `EventSource` sends when it
/// reconnects) resumes after that seq, instead of `from_seq`. An error in
/// the first batch (no namespace, `not_retained`, ...) is the answer's
/// status instead.
pub(super) async fn watch_changes<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
    q: Result<Query<ChangesQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Answer {
    let (mut from_seq, _, options) = changes_query(q, true)?;
    if let Some(id) = headers.get("last-event-id") {
        let seq = id.to_str().ok().and_then(|id| id.trim().parse::<u64>().ok());
        let seq = seq.ok_or_else(|| Error::invalid("Last-Event-ID must be the seq of an event"))?;
        from_seq = seq.saturating_add(1);
    }
    let r = pb::WatchRequest { namespace: path(p)?, from_seq, options };
    let db = s.db(&caller)?;
    let rounds = Arc::new(db.following());
    let mut batches = ops::follow(Arc::new(db), rounds, r, None, s.stopping.clone());
    let first = match batches.recv().await {
        Some(first) => first?,
        None => return Err(Error::unavailable("the server is shutting down").into()),
    };
    let events = tokio_stream::once(Ok(first)).chain(ReceiverStream::new(batches)).map(|batch| {
        Ok::<_, std::convert::Infallible>(Bytes::from(match batch {
            Ok(batch) => sse_events(&batch),
            Err(e) => sse_error(&e),
        }))
    });
    let headers = [
        (header::CONTENT_TYPE, HeaderValue::from_static(EVENT_STREAM)),
        (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
    ];
    Ok((headers, Body::from_stream(events)).into_response())
}

/// A batch as SSE events, or a heartbeat comment if it has none.
fn sse_events(batch: &pb::GetChangesResponse) -> Vec<u8> {
    if batch.events.is_empty() {
        return format!(": next seq {}\n\n", batch.next_seq).into_bytes();
    }
    let mut out = Vec::new();
    for event in &batch.events {
        match serde_json::to_vec(event) {
            Ok(json) => {
                out.extend_from_slice(format!("id: {}\nevent: change\ndata: ", event.seq).as_bytes());
                // JSON has no raw newlines: one data line
                out.extend_from_slice(&json);
                out.extend_from_slice(b"\n\n");
            }
            Err(e) => {
                out.extend(sse_error(&Error::internal(format!("can't write change {}: {}", event.seq, e))));
                break;
            }
        }
    }
    out
}

fn sse_error(e: &Error) -> Vec<u8> {
    let body = pb::Error { code: e.code().as_str().to_owned(), message: e.message().to_owned() };
    let json = serde_json::to_vec(&body).unwrap_or_default();
    let mut out = b"event: error\ndata: ".to_vec();
    out.extend_from_slice(&json);
    out.extend_from_slice(b"\n\n");
    out
}
