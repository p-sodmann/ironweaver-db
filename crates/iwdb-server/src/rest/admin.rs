//! Server administration and analytics job handlers.

use super::transport::{Answer, Failure, path, read, unary};
use super::{Caller_, St};
use crate::auth::Served;
use crate::{ops, proto as pb};
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use http::HeaderMap;
use iwdb_query::Error;
use serde::Deserialize;

pub(super) async fn get_server_status<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::get_server_status(&s.db(&caller)?).await)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestsQuery {
    user: Option<String>,
    limit: Option<u32>,
}

pub(super) async fn list_requests<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    q: Result<Query<RequestsQuery>, QueryRejection>,
) -> Answer {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    unary(ops::list_requests(&s.db(&caller)?, pb::ListRequestsRequest { user: q.user, limit: q.limit }).await)
}

pub(super) async fn cancel_request<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<u64>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let db = s.db(&caller)?;
    let mut r: pb::CancelRequestRequest = read(&headers, body, s.max_body).await?;
    let id = path(p)?;
    if r.id != 0 && r.id != id {
        return Err(Error::invalid(format!("the body names request {}, the path {}", r.id, id)).into());
    }
    r.id = id;
    unary(ops::cancel_request(&db, r).await)
}

// ---- managed jobs (step 16f) ----

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobQuery {
    user: Option<String>,
    offset: Option<u64>,
    limit: Option<u32>,
}

fn job_query(q: Result<Query<JobQuery>, QueryRejection>) -> Result<JobQuery, Failure> {
    q.map(|Query(q)| q).map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))
}

pub(super) async fn list_jobs<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    q: Result<Query<RequestsQuery>, QueryRejection>,
) -> Answer {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    unary(ops::list_jobs(&s.db(&caller)?, pb::ListJobsRequest { user: q.user, limit: q.limit }).await)
}

pub(super) async fn get_job<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<u64>, PathRejection>,
    q: Result<Query<JobQuery>, QueryRejection>,
) -> Answer {
    let q = job_query(q)?;
    if q.offset.is_some() || q.limit.is_some() {
        return Err(Error::invalid("a job takes only the parameter 'user'").into());
    }
    unary(ops::get_job(&s.db(&caller)?, pb::GetJobRequest { id: path(p)?, user: q.user }).await)
}

pub(super) async fn get_job_result<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<u64>, PathRejection>,
    q: Result<Query<JobQuery>, QueryRejection>,
) -> Answer {
    let q = job_query(q)?;
    let r = pb::GetJobResultRequest { id: path(p)?, user: q.user, offset: q.offset.unwrap_or(0), limit: q.limit };
    unary(ops::get_job_result(&s.db(&caller)?, r).await)
}

pub(super) async fn cancel_job<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<u64>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Answer {
    let db = s.db(&caller)?;
    let mut r: pb::CancelJobRequest = read(&headers, body, s.max_body).await?;
    let id = path(p)?;
    if r.id != 0 && r.id != id {
        return Err(Error::invalid(format!("the body names job {}, the path {}", r.id, id)).into());
    }
    r.id = id;
    unary(ops::cancel_job(&db, r).await)
}

pub(super) async fn list_consumers<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::list_consumers(&s.db(&caller)?).await)
}

pub(super) async fn get_metrics<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::get_metrics(&s.db(&caller)?).await)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LogQuery {
    after: Option<u64>,
    limit: Option<u32>,
}

pub(super) async fn get_log<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    q: Result<Query<LogQuery>, QueryRejection>,
) -> Answer {
    let Query(q) = q.map_err(|e| Failure::from(Error::invalid(format!("invalid query: {}", e))))?;
    unary(ops::get_log(&s.db(&caller)?, pb::GetLogRequest { after: q.after.unwrap_or(0), limit: q.limit }).await)
}

/// A route whose body is its RPC's request message, and that has no path
/// parameters: the admin writes.
macro_rules! body_handler {
    ($name:ident, $request:ty) => {
        pub(super) async fn $name<D: Served>(
            State(s): St<D>,
            caller: Caller_,
            headers: HeaderMap,
            body: Body,
        ) -> Answer {
            let db = s.db(&caller)?;
            let r: $request = read(&headers, body, s.max_body).await?;
            unary(ops::$name(&db, r).await)
        }
    };
}

body_handler!(checkpoint, pb::CheckpointRequest);
body_handler!(backup, pb::BackupRequest);
body_handler!(verify, pb::VerifyRequest);
body_handler!(prune_archive, pb::PruneArchiveRequest);
