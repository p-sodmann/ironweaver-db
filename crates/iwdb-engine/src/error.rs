//! The engine's error type.

use std::fmt;

use ironweaver_core::GraphError;

use crate::catalog::{AttrPath, CatalogError, Constraint};
use crate::mutation::Target;

/// Errors raised by the engine.
///
/// Corrupt or unexpected data on disk is always an error, never a panic.
/// So is every invalid or conflicting commit: a commit that fails leaves
/// the namespace unchanged, except for [`Error::ApplyFailed`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An error from `ironweaver-core` (including file format errors).
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// User input used a meta key, or a top-level attribute key, owned by
    /// the database (see [`reserved`](crate::reserved)).
    #[error("key '{key}' is reserved: keys starting with 'iwdb.' belong to the database")]
    ReservedName { key: String },
    /// A saved node or edge has no `iwdb.version` meta entry.
    #[error("{entity} has no '{}' meta entry", crate::reserved::VERSION_KEY)]
    MissingVersion { entity: Entity },
    /// A saved node or edge has an `iwdb.version` that is not a
    /// non-negative integer.
    #[error("{entity} has an invalid '{}' meta entry: {found}", crate::reserved::VERSION_KEY)]
    InvalidVersion { entity: Entity, found: String },
    /// A saved node or edge, or the graph meta, has a reserved key this
    /// version of the database doesn't know (written by a newer version?).
    #[error("{entity} has an unknown reserved key '{key}' (written by a newer version?)")]
    UnknownReservedKey { entity: Entity, key: String },
    /// The graph meta of a saved file has a key the database doesn't own.
    #[error("graph meta has key '{key}', but graph meta belongs to the database")]
    UnexpectedGraphMeta { key: String },
    /// The graph meta of a saved file has no `iwdb.seq`.
    #[error("graph meta has no '{}' entry", crate::reserved::SEQ_KEY)]
    MissingSeq,
    /// The graph meta's `iwdb.seq` is not a non-negative integer.
    #[error("graph meta has an invalid '{}' entry: {found}", crate::reserved::SEQ_KEY)]
    InvalidSeq { found: String },
    /// An invalid catalog, or one that can't be read.
    #[error(transparent)]
    Catalog(#[from] CatalogError),

    // Commit errors (step 3). The commit is rejected, nothing changes.
    /// A mutation's `expected_version` differs from the entity's version
    /// before the transaction (0: the entity didn't exist).
    #[error("version conflict on {target}: expected version {expected}, found {actual}")]
    Conflict { target: Target, expected: u64, actual: u64 },
    /// A mutation addresses a node or edge that doesn't exist (at that
    /// point of the transaction).
    #[error("{target} not found")]
    NotFound { target: Target },
    /// An edge upsert by endpoints and type matches more than one edge.
    #[error("{count} edges from '{from}' to '{to}' with type {ty:?} match; an upsert by endpoints needs at most one")]
    AmbiguousEdge { from: String, to: String, ty: Option<String>, count: usize },
    /// An edge upsert by endpoints and type found no edge, but expected
    /// one (an `expected_version` other than 0): a version conflict without
    /// an edge id to name.
    #[error("no edge from '{from}' to '{to}' with type {ty:?} exists, but version {expected} was expected")]
    NoMatchingEdge { from: String, to: String, ty: Option<String>, expected: u64 },
    /// An append to an attribute that holds something other than a list.
    #[error("attribute '{key}' of {target} is not a list")]
    NotAList { target: Target, key: String },
    /// An attribute value nested deeper than the core can encode and decode
    /// again (see [`MAX_VALUE_DEPTH`](crate::mutation::MAX_VALUE_DEPTH)).
    #[error("attribute '{key}' of {target} is nested more than {} levels deep", ironweaver_core::format::MAX_DEPTH)]
    ValueTooDeep { target: Target, key: String },
    /// The entity's version would exceed `i64::MAX`, the largest version a
    /// file can hold. Versions never wrap.
    #[error("the version of {target} would exceed {}", i64::MAX)]
    VersionOverflow { target: Target },
    /// The state after the transaction violates a constraint: `node` has
    /// no value at a required path, or has the same value at a unique path
    /// as `other`.
    #[error("{constraint} is violated by node '{node}'{}", other.as_ref().map(|o| format!(" and node '{}'", o)).unwrap_or_default())]
    ConstraintViolation { constraint: Constraint, node: String, other: Option<String> },
    /// A transaction without mutations.
    #[error("the transaction has no mutations")]
    EmptyTransaction,
    /// The catalog already has this index.
    #[error("an index on '{path}' exists already")]
    IndexExists { path: AttrPath },
    /// The catalog has no such index.
    #[error("there is no index on '{path}'")]
    NoSuchIndex { path: AttrPath },
    /// The core can't index this path (`labels` is the node's labels).
    #[error("attribute path '{path}' can't be indexed")]
    UnindexablePath { path: AttrPath },
    /// The catalog already has this constraint.
    #[error("{constraint} exists already")]
    ConstraintExists { constraint: Constraint },
    /// The catalog has no such constraint.
    #[error("there is no {constraint}")]
    NoSuchConstraint { constraint: Constraint },
    /// The transaction would need an edge id above `u64::MAX`.
    #[error("edge ids are exhausted")]
    EdgeIdsExhausted,
    /// No commit sequence number is left (`u64::MAX` commits).
    #[error("commit sequence numbers are exhausted")]
    SeqExhausted,
    /// A commit record was applied out of order: its `seq` is not the one
    /// after the namespace's (a prepared commit that another commit
    /// overtook, or a gap in replayed records).
    #[error("commit record has seq {found}, but the next seq is {expected}")]
    OutOfOrder { expected: u64, found: u64 },
    /// Applying a validated commit failed. The namespace is now poisoned:
    /// its graph may be inconsistent (`GraphError::Internal`) or no longer
    /// match what the log will hold, so it rejects further commits until it
    /// is reopened from checkpoint and log.
    #[error("applying commit {seq} failed ({error}); the namespace must be reopened")]
    ApplyFailed { seq: u64, error: GraphError },
    /// The namespace was poisoned by an earlier [`Error::ApplyFailed`].
    #[error("the namespace is poisoned by a failed commit and must be reopened")]
    Poisoned,

    // Idempotency keys (step 8)
    /// An idempotency key that is empty or too long.
    #[error("invalid idempotency key: {reason}")]
    InvalidIdempotencyKey { reason: String },
    /// A commit reused the idempotency key of an earlier commit (at `seq`)
    /// with a different request. A key names one request; nothing changed.
    #[error("idempotency key {key} was used for a different request (commit {seq})")]
    IdempotencyKeyReused { key: crate::IdempotencyKey, seq: u64 },
    /// A request that can't be encoded for its fingerprint (values nested
    /// too deep; such a request fails validation too).
    #[error("the request can't be encoded: {message}")]
    Unencodable { message: String },
    /// A saved idempotency key table (`iwdb.keys`) that is invalid.
    #[error("invalid idempotency key table: {reason}")]
    InvalidKeyTable { reason: String },
}

/// Which part of a saved graph an error is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entity {
    Node(String),
    /// An edge, by its id as written in the file.
    Edge(String),
    /// The graph-level meta.
    Graph,
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Entity::Node(id) => write!(f, "node '{}'", id),
            Entity::Edge(id) => write!(f, "edge {}", id),
            Entity::Graph => f.write_str("graph meta"),
        }
    }
}
