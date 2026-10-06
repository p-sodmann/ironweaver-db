//! Ironweaver DB's query layer: the [`Database`] service trait, the
//! bounded read operations behind it, `explain`, and the error model
//! shared by every access method.
//!
//! - [`Database`]: the trait. `iwdb::Embedded` implements it on the
//!   embedded store; the servers of steps 11 and 12 serve it.
//! - [`read`]: the read operations, as functions over a namespace that a
//!   caller runs under the namespace's read lock and a cancel token.
//! - [`QueryOptions`], [`LimitConfig`]: limits per request, server defaults
//!   and hard caps (ADR 0021); [`Cursor`]: pagination.
//! - [`Error`], [`Code`]: stable error codes (`documentation/api/errors.md`).
//! - [`exec`]: a worker pool and `block_on`, to run the synchronous engine
//!   behind async methods without an async runtime (ADR 0020).
//! - [`auth`]: principals, roles, the operation table and [`Authorized`],
//!   the one place that authorises (step 15a, ADR 0045).
//! - [`Admin`]: the operator's reads and cancel (step 16c): the server's
//!   status, the [`requests`] registry, the [`metrics`] and the [`log`]
//!   tail.
//! - `conformance` (feature): the conformance suite for implementations.
//!
//! Pure Rust (design rule 1): no Python, no protocol types.

pub mod admin;
pub mod audit;
pub mod auth;
mod cursor;
mod error;
pub mod exec;
pub mod log;
pub mod metrics;
mod model;
mod options;
pub mod read;
mod request;
pub mod requests;
mod service;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use admin::{
    Admin, BackupDone, BackupRequest, Checkpointed, DiskStatus, LimitSource, Listed, MemorySnapshot, MemoryState,
    MemoryStatus, RequestCounts, ServerStatus, VerifyTarget,
};
pub use auth::{
    Accounts, Audited, Authenticate, Authorized, NewToken, Operation, Principal, Requirement, Role, Secret, Session,
    TokenInfo, UserInfo, Via,
};
pub use cursor::Cursor;
pub use error::{Code, Error};
pub use model::{
    Answer, ChangeEvent, Changes, CommitOptions, Edge, IndexSize, IndexState, IndexStatus, MarkStatus, NamespaceStatus,
    Node, Work,
};
pub use options::{Bounds, LimitConfig, Limits, QueryOptions};
pub use read::{Explain, KeyInfo, LabelInfo, Lookup, Plan, ReadContext, Schema, TypeInfo};
pub use request::{
    AnalyticsRequest, CHANGES_BATCH_BYTES, ChangesRequest, ExplainRequest, FindRequest, Job, JobResult, MatchRequest,
    MatchRow, NeighbourhoodRequest, Order, Path, PathMethod, PathRequest, ProjectionSpec, Subgraph, SubgraphRequest,
    TraverseRequest, WalkRequest,
};
pub use service::Database;
