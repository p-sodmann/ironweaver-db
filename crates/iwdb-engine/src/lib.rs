//! Ironweaver DB engine: the database payload, the catalog and the commit
//! pipeline, built on [`ironweaver_core`].
//!
//! - [`DbRecord`]: the node and edge payload, with a version per entity.
//! - [`reserved`]: keys owned by the database (`iwdb.*`).
//! - [`catalog`]: namespaces, index definitions and constraints.
//! - [`codec`]: saving and loading `Graph<DbRecord, DbRecord>` in the
//!   core's file format, with the catalog in the graph meta.
//! - [`mutation`]: the write vocabulary, commit records and results.
//! - [`Namespace`]: a graph changed only through the commit pipeline.
//! - [`invariants`]: the invariants every namespace keeps, checked from
//!   scratch (for `verify`).
//! - [`testutil`]: canonical graph comparison for tests, and random
//!   workloads with the `testutil` feature.

pub mod catalog;
pub mod codec;
mod error;
pub mod invariants;
pub mod mutation;
mod namespace;
mod record;
pub mod reserved;
mod resolve;
pub mod testutil;

pub use catalog::Catalog;
pub use error::{Entity, Error};
pub use mutation::{CatalogChange, Change, CommitRecord, CommitResult, EdgeKey, Mutation, Target};
pub use namespace::{Namespace, Prepared};
pub use record::DbRecord;

/// A graph of the database: [`DbRecord`] payloads on nodes and edges.
pub type DbGraph = ironweaver_core::Graph<DbRecord, DbRecord>;
