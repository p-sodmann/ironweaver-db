//! Request validation, response encoding, and HTTP errors.

use super::{JSON, NDJSON};
use crate::proto as pb;
use crate::status::http_status;
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use iwdb_query::{Code, Error};
use serde::Deserialize;
use serde::de::DeserializeOwned;

// ---- errors ----

/// A failed request: the error, and the HTTP status if it isn't the code's
/// (HTTP-layer errors: 404, 405, 413, 415).
#[derive(Debug)]
pub(crate) struct Failure(Error, Option<StatusCode>);

impl Failure {
    pub(super) fn http(status: StatusCode, message: impl Into<String>) -> Failure {
        Failure(Error::new(Code::InvalidArgument, message), Some(status))
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure(e, None)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let Failure(e, status) = self;
        crate::status::log_server_error(&e);
        let body = pb::Error { code: e.code().as_str().to_owned(), message: e.message().to_owned() };
        // Two strings: writing them can't fail
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        (status.unwrap_or_else(|| http_status(e.code())), [(header::CONTENT_TYPE, JSON)], bytes).into_response()
    }
}

pub(super) type Answer = Result<Response, Failure>;

// ---- requests ----

/// The request message of a body (an empty body is the empty message).
pub(super) async fn read<T: DeserializeOwned + Default>(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<T, Failure> {
    let bytes = axum::body::to_bytes(body, limit).await.map_err(|e| {
        let too_large = std::error::Error::source(&e).is_some_and(|s| s.is::<http_body_util::LengthLimitError>());
        if too_large {
            Failure::http(StatusCode::PAYLOAD_TOO_LARGE, format!("the request body is larger than {} bytes", limit))
        } else {
            Failure::from(Error::invalid(format!("can't read the request body: {}", e)))
        }
    })?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case(JSON));
    if !json {
        return Err(Failure::http(StatusCode::UNSUPPORTED_MEDIA_TYPE, "a request body must be application/json"));
    }
    serde_json::from_slice(&bytes).map_err(|e| Failure::from(Error::invalid(format!("invalid request body: {}", e))))
}

/// The namespace of a request: the path's.
pub(super) fn namespace(field: &mut String, path: String) -> Result<(), Failure> {
    if !field.is_empty() && *field != path {
        return Err(Error::invalid(format!("the body names namespace '{}', the path '{}'", field, path)).into());
    }
    *field = path;
    Ok(())
}

pub(super) fn path<T>(p: Result<Path<T>, PathRejection>) -> Result<T, Failure> {
    p.map(|Path(t)| t).map_err(|e| Error::invalid(format!("invalid path: {}", e)).into())
}

/// `QueryOptions` as query parameters (GET reads).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OptionsQuery {
    min_seq: Option<u64>,
    history: Option<String>,
    timeout_ms: Option<u32>,
    max_results: Option<u64>,
    max_visited: Option<u64>,
    max_edges: Option<u64>,
    partial: Option<bool>,
    cursor: Option<String>,
}

/// The query parameters of [`Input::Options`] routes, for the OpenAPI
/// document: name, type, description.
pub(crate) const OPTION_PARAMETERS: &[(&str, &str, &str)] = &[
    ("min_seq", "integer", "Read-your-writes: wait until the namespace has applied this seq."),
    ("history", "string", "The history `min_seq` belongs to (32 hex digits)."),
    ("timeout_ms", "integer", "How long the read may take, in milliseconds."),
    ("max_results", "integer", "Most results in the answer."),
    ("max_visited", "integer", "Most nodes the read may visit."),
    ("max_edges", "integer", "Most edges the read may examine."),
    ("partial", "boolean", "Answer with what was found when a limit is reached."),
    ("cursor", "string", "Continue a paginated read: the `next` of the previous page."),
];

pub(super) fn options(q: Result<Query<OptionsQuery>, QueryRejection>) -> Result<Option<pb::QueryOptions>, Failure> {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    let limits = pb::Limits { max_results: q.max_results, max_visited: q.max_visited, max_edges: q.max_edges };
    Ok(Some(pb::QueryOptions {
        min_seq: q.min_seq,
        history: q.history.unwrap_or_default(),
        timeout_ms: q.timeout_ms,
        limits: Some(limits),
        partial: q.partial.unwrap_or_default(),
        cursor: q.cursor.unwrap_or_default(),
    }))
}

/// The query parameters of the change stream's routes.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChangesQuery {
    from_seq: Option<u64>,
    wait: Option<bool>,
    min_seq: Option<u64>,
    history: Option<String>,
    timeout_ms: Option<u32>,
    max_results: Option<u64>,
}

/// The query parameters of [`Input::Changes`] routes, for the OpenAPI
/// document: name, type, description (`wait` only without `stream`).
pub(crate) const CHANGES_PARAMETERS: &[(&str, &str, &str)] = &[
    ("from_seq", "integer", "The first seq to return; resume with the `nextSeq` of the last batch."),
    ("wait", "boolean", "If there is no commit yet, wait for one for about the timeout (a long poll)."),
    ("min_seq", "integer", "Wait until the namespace has applied this seq first."),
    ("history", "string", "The history `from_seq` belongs to (32 hex digits); another one is refused."),
    ("timeout_ms", "integer", "How long the request may take (for a stream: each round), in milliseconds."),
    ("max_results", "integer", "Most commits in a batch."),
];

/// `from_seq`, `wait` and the options of a change stream route.
pub(super) fn changes_query(
    q: Result<Query<ChangesQuery>, QueryRejection>,
    stream: bool,
) -> Result<(u64, bool, Option<pb::QueryOptions>), Failure> {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    if stream && q.wait.is_some() {
        return Err(Error::invalid("the change stream always waits: leave `wait` out").into());
    }
    let options = pb::QueryOptions {
        min_seq: q.min_seq,
        history: q.history.unwrap_or_default(),
        timeout_ms: q.timeout_ms,
        limits: Some(pb::Limits { max_results: q.max_results, ..pb::Limits::default() }),
        ..pb::QueryOptions::default()
    };
    Ok((q.from_seq.unwrap_or_default(), q.wait.unwrap_or_default(), Some(options)))
}

// ---- answers ----

pub(super) fn json_response(bytes: Vec<u8>, media: &'static str) -> Response {
    ([(header::CONTENT_TYPE, HeaderValue::from_static(media))], bytes).into_response()
}

pub(super) fn to_json<T: serde::Serialize>(message: &T) -> Result<Vec<u8>, Failure> {
    serde_json::to_vec(message).map_err(|e| Error::internal(format!("can't write the answer: {}", e)).into())
}

pub(super) fn unary<T: serde::Serialize>(message: Result<T, Error>) -> Answer {
    Ok(json_response(to_json(&message?)?, JSON))
}

pub(super) fn wants_ndjson(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.split(',').any(|t| t.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case(NDJSON))))
}

/// A streamed answer: NDJSON if asked for, otherwise its chunks merged into
/// one message (protobuf merge: repeated fields concatenate, `meta` is the
/// last chunk's).
pub(super) fn streamed<T>(headers: &HeaderMap, chunks: Result<Vec<T>, Error>) -> Answer
where
    T: prost::Message + serde::Serialize + Default,
{
    let mut chunks = chunks?;
    if wants_ndjson(headers) {
        let mut lines = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let mut line = to_json(chunk)?;
            line.push(b'\n');
            lines.push(Ok::<_, std::convert::Infallible>(Bytes::from(line)));
        }
        let body = Body::from_stream(tokio_stream::iter(lines));
        return Ok(([(header::CONTENT_TYPE, HeaderValue::from_static(NDJSON))], body).into_response());
    }
    let one = if chunks.len() == 1 {
        chunks.pop().unwrap_or_default()
    } else {
        let mut all = T::default();
        for chunk in chunks {
            all.merge(chunk.encode_to_vec().as_slice())
                .map_err(|e| Error::internal(format!("can't merge the answer: {}", e)))?;
        }
        all
    };
    unary(Ok(one))
}

/// The chunks of an answer of one item: there is one.
pub(super) fn merged<T: Default>(mut chunks: Vec<T>) -> Result<T, Failure> {
    match chunks.len() {
        1 => Ok(chunks.pop().unwrap_or_default()),
        n => Err(Error::internal(format!("an answer of one item came in {} chunks", n)).into()),
    }
}
