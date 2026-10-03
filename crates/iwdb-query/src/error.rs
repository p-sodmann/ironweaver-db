//! The error model of every access method: an [`Error`] has a stable
//! [`Code`] and a message (`documentation/api/errors.md`).
//!
//! The engine's and the storage layer's errors are mapped here, once, so
//! that the embedded store, Python, gRPC and REST report the same code for
//! the same failure (design rule 8).

use std::fmt;

use ironweaver_core::GraphError;

/// What kind of failure an [`Error`] is. The codes and their strings
/// ([`Code::as_str`]) are a contract: adapters map them to their own
/// status codes (gRPC in step 11, HTTP in step 12), and clients branch on
/// them. A new code is a minor version; renaming or removing one breaks
/// clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Code {
    /// The request is invalid: a bad argument, a limit of 0, a filter or
    /// pattern the core rejects, an invalid transaction. Retrying the same
    /// request fails the same way.
    InvalidArgument,
    /// A namespace (or, in a mutation, a node or edge, an index or a
    /// constraint) doesn't exist.
    NotFound,
    /// The request conflicts with the state: a version conflict, a
    /// namespace or index that exists already, an idempotency key reused
    /// for another request. Nothing changed.
    Conflict,
    /// A commit would violate a unique or required constraint. Nothing
    /// changed.
    ConstraintViolation,
    /// A read reached one of its limits (results, nodes visited, edges
    /// examined) and the request didn't ask for a partial answer.
    BudgetExceeded,
    /// The request didn't finish within its timeout. Nothing changed.
    Timeout,
    /// The caller cancelled the request (dropped its future).
    Cancelled,
    /// A cursor whose namespace has changed since it was made: a paginated
    /// read is served only at the seq of its first page. Start again
    /// without the cursor.
    CursorExpired,
    /// The namespace is read-only after a failed WAL write or fsync, or a
    /// failed apply, until the store is reopened.
    ReadOnly,
    /// The store can't take the request now: it is closed or shutting
    /// down, or too many requests are queued. Retry later.
    Unavailable,
    /// A file operation failed. A commit's outcome is unknown: retry it
    /// with the same idempotency key.
    Io,
    /// Damaged data on disk: the WAL, a checkpoint, the namespace log.
    Corrupt,
    /// A bug in the database (a panic, a broken invariant).
    Internal,
}

impl Code {
    /// Every code, in the order of `documentation/api/errors.md`.
    pub const ALL: [Code; 13] = [
        Code::InvalidArgument,
        Code::NotFound,
        Code::Conflict,
        Code::ConstraintViolation,
        Code::BudgetExceeded,
        Code::Timeout,
        Code::Cancelled,
        Code::CursorExpired,
        Code::ReadOnly,
        Code::Unavailable,
        Code::Io,
        Code::Corrupt,
        Code::Internal,
    ];

    /// The code's stable name, in snake case (`"budget_exceeded"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Code::InvalidArgument => "invalid_argument",
            Code::NotFound => "not_found",
            Code::Conflict => "conflict",
            Code::ConstraintViolation => "constraint_violation",
            Code::BudgetExceeded => "budget_exceeded",
            Code::Timeout => "timeout",
            Code::Cancelled => "cancelled",
            Code::CursorExpired => "cursor_expired",
            Code::ReadOnly => "read_only",
            Code::Unavailable => "unavailable",
            Code::Io => "io",
            Code::Corrupt => "corrupt",
            Code::Internal => "internal",
        }
    }

    /// The code named `name` ([`as_str`](Self::as_str)), for adapters that
    /// receive codes over the wire.
    pub fn parse(name: &str) -> Option<Code> {
        Code::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error of the [`Database`](crate::Database) trait: a stable [`Code`]
/// and a message for people. Branch on the code, never on the message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    code: Code,
    message: String,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Error { code, message: message.into() }
    }

    pub fn code(&self) -> Code {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Error::new(Code::InvalidArgument, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Error::new(Code::NotFound, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Error::new(Code::Unavailable, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Error::new(Code::Internal, message)
    }

    /// A limit stopped the read: `what` was reached.
    pub fn budget(what: impl fmt::Display) -> Self {
        Error::new(
            Code::BudgetExceeded,
            format!("the read reached its limit of {}; narrow it, raise the limit, or ask for a partial answer", what),
        )
    }
}

impl From<GraphError> for Error {
    fn from(e: GraphError) -> Self {
        let code = match &e {
            GraphError::NodeNotFound(_) | GraphError::EdgeNotFound(_) => Code::NotFound,
            GraphError::DuplicateNode(_) | GraphError::DuplicateEdge(_) => Code::Conflict,
            GraphError::InvalidArgument(_)
            | GraphError::InvalidType(_)
            | GraphError::Format(_)
            | GraphError::Capacity(_) => Code::InvalidArgument,
            GraphError::Interrupted => Code::Cancelled,
            GraphError::BudgetExceeded { .. } => Code::BudgetExceeded,
            _ => Code::Internal,
        };
        Error::new(code, e.to_string())
    }
}

impl From<iwdb_engine::Error> for Error {
    fn from(e: iwdb_engine::Error) -> Self {
        use iwdb_engine::Error as E;
        let code = match &e {
            E::Graph(g) => return g.clone().into(),
            E::Conflict { .. }
            | E::NoMatchingEdge { .. }
            | E::IdempotencyKeyReused { .. }
            | E::IndexExists { .. }
            | E::ConstraintExists { .. } => Code::Conflict,
            E::NotFound { .. } | E::NoSuchIndex { .. } | E::NoSuchConstraint { .. } => Code::NotFound,
            E::ConstraintViolation { .. } => Code::ConstraintViolation,
            E::ReservedName { .. }
            | E::Catalog(_)
            | E::AmbiguousEdge { .. }
            | E::NotAList { .. }
            | E::ValueTooDeep { .. }
            | E::VersionOverflow { .. }
            | E::EmptyTransaction
            | E::UnindexablePath { .. }
            | E::EdgeIdsExhausted
            | E::SeqExhausted
            | E::InvalidIdempotencyKey { .. }
            | E::Unencodable { .. } => Code::InvalidArgument,
            E::MissingVersion { .. }
            | E::InvalidVersion { .. }
            | E::UnknownReservedKey { .. }
            | E::UnexpectedGraphMeta { .. }
            | E::MissingSeq
            | E::InvalidSeq { .. }
            | E::InvalidKeyTable { .. } => Code::Corrupt,
            E::ApplyFailed { .. } | E::Poisoned => Code::ReadOnly,
            _ => Code::Internal,
        };
        Error::new(code, e.to_string())
    }
}

impl From<iwdb_storage::Error> for Error {
    fn from(e: iwdb_storage::Error) -> Self {
        use iwdb_storage::Error as E;
        let code = match e {
            E::Engine(engine) => return engine.into(),
            E::Io { .. } | E::CheckpointsDisabled { .. } => Code::Io,
            E::ReadOnly { .. } => Code::ReadOnly,
            E::Locked { .. } => Code::Unavailable,
            E::Timeout { .. } => Code::Timeout,
            E::Cancelled => Code::Cancelled,
            E::NoSuchNamespace { .. } | E::NamespaceDropped { .. } => Code::NotFound,
            E::NamespaceExists { .. } => Code::Conflict,
            E::Corrupt { .. }
            | E::InvalidRecord { .. }
            | E::SeqMismatch { .. }
            | E::HeaderMismatch { .. }
            | E::UnsupportedVersion { .. }
            | E::SegmentTooLarge { .. }
            | E::TornTail { .. }
            | E::InvalidCheckpoint { .. }
            | E::NoUsableCheckpoint { .. }
            | E::ReplayFailed { .. }
            | E::InvalidDataDir { .. }
            | E::InvalidManifest { .. }
            | E::InvalidNamespaceLog { .. }
            | E::NamespaceDamaged { .. }
            | E::ArchiveConflict { .. }
            | E::LogEndsBefore { .. }
            | E::MissingRecords { .. } => Code::Corrupt,
            E::OutOfOrder { .. } | E::LogAhead { .. } => Code::Internal,
            // Requests the state doesn't allow: a record too large, a seq
            // of another history, a restore that names no namespace, a
            // directory that isn't a store, ...
            _ => Code::InvalidArgument,
        };
        Error::new(code, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_unique_stable_names() {
        let names: Vec<&str> = Code::ALL.iter().map(|c| c.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        for code in Code::ALL {
            assert_eq!(Code::parse(code.as_str()), Some(code));
        }
        assert_eq!(Code::parse("nope"), None);
    }

    #[test]
    fn errors_of_the_lower_layers_keep_their_message() {
        let e = Error::from(iwdb_storage::Error::NoSuchNamespace { name: "x".into() });
        assert_eq!((e.code(), e.message()), (Code::NotFound, "no namespace 'x'"));
        let e = Error::from(iwdb_storage::Error::Engine(iwdb_engine::Error::EmptyTransaction));
        assert_eq!(e.code(), Code::InvalidArgument);
        let e = Error::from(GraphError::BudgetExceeded { visited: 1, edges: 2, results: 3 });
        assert_eq!(e.code(), Code::BudgetExceeded);
        assert!(e.message().contains("examining 2 edges"));
    }
}
