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
//! - [`idempotency`]: idempotency keys and the table of recent keyed
//!   commits.
//! - [`mark`]: named high-water marks, moved by commits (projections).
//! - [`plain`]: plain graph files (the core's format without the
//!   database's keys), for import and export.
//! - [`CommitTime`]: when the WAL appended a commit.
//! - [`invariants`]: the invariants every namespace keeps, checked from
//!   scratch (for `verify`).
//! - [`testutil`]: canonical graph comparison for tests, and random
//!   workloads with the `testutil` feature.

pub mod catalog;
pub mod codec;
mod error;
#[cfg(feature = "failpoints")]
pub mod failpoint;
pub mod idempotency;
pub mod invariants;
pub mod mark;
pub mod mutation;
mod namespace;
pub mod plain;
mod record;
pub mod reserved;
mod resolve;
pub mod testutil;
mod time;

pub use error::{Entity, Error};
pub use idempotency::{IdempotencyKey, KeyTable, Keyed};
pub use mark::{Mark, MarkName, MarkTable, MarkUpdate};
pub use mutation::{CatalogChange, Change, CommitRecord, CommitResult, EdgeKey, Mutation, Target};
pub use namespace::{IndexBuild, Namespace, Prepare, Prepared};
pub use record::DbRecord;
pub use time::CommitTime;

/// A graph of the database: [`DbRecord`] payloads on nodes and edges.
pub type DbGraph = ironweaver_core::Graph<DbRecord, DbRecord>;
