//! The [`Admin`] trait (step 16c, ADR 0051): what an operator reads about a
//! running database (its status, the running requests, the change-stream
//! readers, the metrics and the log tail) and cancelling a request. A
//! sibling of [`Database`](crate::Database), which stays the data API.
//!
//! Implemented once, by the embedded database (`iwdb::Embedded`), and
//! again by the clients over the wire and by
//! [`Authorized`](crate::Authorized), which checks the caller and narrows
//! what it sees (design rule 8).
//!
//! **Every read is bounded** (design rule 5): the status and the metrics
//! are O(namespaces), the request list and the log at most
//! [`MAX_LIST`] entries, the reader list at most
//! [`MAX_CONSUMERS`](crate::requests::MAX_CONSUMERS).

use std::future::Future;
use std::sync::Arc;

use iwdb_engine::CommitTime;
pub use iwdb_storage::memory::{LimitSource, MemorySnapshot, MemoryState};

use crate::log::LogTail;
use crate::metrics::Metrics;
use crate::requests::{ConsumerInfo, RequestInfo, Requests};
use crate::{Error, NamespaceStatus};

/// The most entries a list of [`Admin`] returns (and its default).
pub const MAX_LIST: usize = 1000;

/// The database's state ([`Admin::server_status`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ServerStatus {
    /// The server's version (its crates').
    pub version: String,
    /// When the database started serving.
    pub started: CommitTime,
    /// Whether it serves requests: `false` once it drains (shutdown).
    pub ready: bool,
    /// The WAL's fsync policy: `always`, `group` or `off`.
    pub fsync: String,
    pub memory: MemoryStatus,
    pub disk: DiskStatus,
    pub requests: RequestCounts,
    /// The namespaces' status, by name.
    pub namespaces: Vec<NamespaceStatus>,
}

/// Memory, as the database counts it, and its limit (ADR 0054). Server
/// wide: every namespace's, the system namespace's included.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryStatus {
    /// The live graphs, indexes included: the core's estimate (the sum of
    /// the namespaces' `memory_bytes`).
    pub graph_bytes: u64,
    /// The live graphs' payloads (attribute maps), estimated.
    pub payload_bytes: u64,
    /// The checkpointers' copies of the namespaces.
    pub checkpoint_bytes: u64,
    /// Analytics projections and index builds while they run.
    pub working_bytes: u64,
    /// The sum of the parts: what the limit counts.
    pub used_bytes: u64,
    /// The memory limit; `None`: none.
    pub limit_bytes: Option<u64>,
    /// Where the warning starts, and where writes are refused; `None`
    /// without a limit.
    pub warn_bytes: Option<u64>,
    pub refuse_writes_bytes: Option<u64>,
    pub state: MemoryState,
    /// Where the limit came from; `None` without a limit.
    pub limit_source: Option<LimitSource>,
}

impl MemoryStatus {
    /// The status of `memory`'s snapshot.
    pub fn of(m: &MemorySnapshot) -> Self {
        MemoryStatus {
            graph_bytes: m.graph,
            payload_bytes: m.payload,
            checkpoint_bytes: m.checkpoint,
            working_bytes: m.working,
            used_bytes: m.used(),
            limit_bytes: m.limit.map(|l| l.bytes),
            warn_bytes: m.limit.map(|l| l.warn),
            refuse_writes_bytes: m.limit.map(|l| l.refuse_writes),
            state: m.state,
            limit_source: m.limit.map(|l| l.source),
        }
    }
}

/// Disk use of the data directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskStatus {
    /// The namespaces' WAL segments.
    pub wal_bytes: u64,
    /// The namespaces' checkpoints.
    pub checkpoint_bytes: u64,
    /// Free for the server on the data directory's file system; `None`
    /// where that can't be read.
    pub free_bytes: Option<u64>,
}

/// Requests since the database started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestCounts {
    /// Running now.
    pub active: u64,
    /// Ended (refused ones included).
    pub total: u64,
    /// Ended with `timeout`.
    pub timed_out: u64,
    /// Ended with `cancelled` (by a cancel, or the client went away).
    pub cancelled: u64,
    /// Refused with `unavailable` (the server's queue was full, or it was
    /// draining).
    pub rejected: u64,
    /// Refused with `unauthenticated` or `permission_denied`.
    pub denied: u64,
}

/// A list and whether a limit cut it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}

/// The operator's reads of a database, and cancelling a request. Errors
/// have the stable codes of [`Code`](crate::Code).
pub trait Admin: Send + Sync {
    /// Version, uptime, readiness, fsync policy, memory, disk, request
    /// counts and every namespace's status. O(namespaces).
    fn server_status(&self) -> impl Future<Output = Result<ServerStatus, Error>> + Send;

    /// The running requests, oldest first: only `user`'s if given; at most
    /// `limit` (default and maximum [`MAX_LIST`]). O(running requests).
    fn active_requests(
        &self,
        user: Option<String>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Listed<RequestInfo>, Error>> + Send;

    /// Cancel running request `id` (only if it is `user`'s, if given): its
    /// caller gets `cancelled`, unless its answer was ready first. Returns
    /// the request. Errors: `not_found` (no such request running, or it is
    /// someone else's); `invalid_argument` (a commit or another change,
    /// which can't be cancelled).
    fn cancel_request(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<RequestInfo, Error>> + Send;

    /// The change-stream readers that polled in the last minute, by
    /// namespace, user and client, with their lag. At most
    /// [`MAX_CONSUMERS`](crate::requests::MAX_CONSUMERS).
    fn consumers(&self) -> impl Future<Output = Result<Vec<ConsumerInfo>, Error>> + Send;

    /// Every metric of [`METRICS`](crate::metrics::METRICS), now.
    /// O(operations + namespaces).
    fn metrics(&self) -> impl Future<Output = Result<Metrics, Error>> + Send;

    /// The logged events after `after`, at most `limit` (default and
    /// maximum [`MAX_READ`](crate::log::MAX_READ)).
    fn log(&self, after: u64, limit: Option<usize>) -> impl Future<Output = Result<LogTail, Error>> + Send;

    /// Whether the database's server serves requests: the server says
    /// `false` when it starts draining (shutdown). What
    /// [`ServerStatus::ready`] and the `iwdb_ready` metric report. A client
    /// ignores it.
    fn set_ready(&self, _ready: bool) {}

    /// The registry the authorisation point registers calls in (the
    /// database's own); `None` for a client, whose server registers them.
    fn registry(&self) -> Option<Arc<Requests>> {
        None
    }
}

/// `limit` as a list's limit: [`MAX_LIST`] at most, and by default.
pub fn list_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(MAX_LIST).min(MAX_LIST)
}
