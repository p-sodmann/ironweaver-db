//! Ironweaver DB engine: the database payload, the catalog and the commit
//! pipeline, built on [`ironweaver_core`].
//!
//! - [`DbRecord`]: the node and edge payload, with a version per entity.
//! - [`reserved`]: meta keys owned by the database (`iwdb.*`).
//! - [`catalog`]: namespaces, index definitions and constraints.
//! - [`codec`]: saving and loading `Graph<DbRecord, DbRecord>` in the
//!   core's file format, with the catalog in the graph meta.
//! - [`testutil`]: canonical graph comparison for tests.
//!
//! The commit pipeline arrives in step 3.

pub mod catalog;
pub mod codec;
mod error;
mod record;
pub mod reserved;
pub mod testutil;

pub use catalog::Catalog;
pub use error::{Entity, Error};
pub use record::DbRecord;

/// A graph of the database: [`DbRecord`] payloads on nodes and edges.
pub type DbGraph = ironweaver_core::Graph<DbRecord, DbRecord>;
