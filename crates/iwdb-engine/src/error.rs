//! The engine's error type.

use std::fmt;

use ironweaver_core::GraphError;

use crate::catalog::CatalogError;

/// Errors raised by the engine.
///
/// Corrupt or unexpected data on disk is always an error, never a panic.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An error from `ironweaver-core` (including file format errors).
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// User input used a meta key owned by the database (see
    /// [`reserved`](crate::reserved)).
    #[error("meta key '{key}' is reserved: keys starting with 'iwdb.' belong to the database")]
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
    #[error("{entity} has an unknown reserved meta key '{key}' (written by a newer version?)")]
    UnknownReservedKey { entity: Entity, key: String },
    /// The graph meta of a saved file has a key the database doesn't own.
    #[error("graph meta has key '{key}', but graph meta belongs to the database")]
    UnexpectedGraphMeta { key: String },
    /// An invalid catalog, or one that can't be read.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
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
