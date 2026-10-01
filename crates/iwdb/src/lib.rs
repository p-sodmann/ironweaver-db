//! Ironweaver DB, embedded: a durable graph store in a directory.
//!
//! ```no_run
//! use iwdb::{Mutation, Store, StoreOptions};
//!
//! # fn main() -> Result<(), iwdb::Error> {
//! let store = Store::open("data".as_ref(), StoreOptions::default())?;
//! store.commit(&[Mutation::UpsertNode {
//!     id: "alice".into(),
//!     labels: vec!["Person".into()],
//!     attr: [("name".to_owned(), "Alice".into())].into(),
//!     meta: Default::default(),
//!     expected_version: None,
//! }])?;
//! assert_eq!(store.node("alice").map(|n| n.version), Some(1));
//! store.close()?;
//! # Ok(())
//! # }
//! ```
//!
//! [`Store`] translates only: commits go through the engine's commit
//! pipeline ([`iwdb_engine::Namespace`]) and the WAL
//! ([`iwdb_storage::LoggedNamespace`]); opening runs recovery
//! ([`iwdb_storage::recover`]); checkpoints are written by
//! [`iwdb_storage::Checkpointer`]. The guarantees are in
//! `documentation/guarantees.md`, the directory layout in
//! `documentation/formats/data-dir.md`.

mod ops;
mod options;
mod store;

pub use ironweaver_core::{Attrs, EdgeId, Value};
pub use iwdb_engine::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label, NamespaceCatalog};
pub use iwdb_engine::{CatalogChange, CommitResult, EdgeKey, Mutation, Namespace, Target};
pub use iwdb_storage::io::{LogFs, StdFs};
pub use iwdb_storage::{
    BackupReport, CheckpointOutcome, CommitTime, CutTail, DirStatus, Error, Finding, FsyncPolicy, HistoryId, Kind,
    RecoveryReport, RestoreReport, RestoreSources, RestoreTarget, SkippedCheckpoint, VerifyReport, WalOptions,
};
pub use ops::{restore, restore_with, status, verify, Status};
pub use options::{CheckpointOptions, StoreOptions};
pub use store::{Edge, Node, Store, StoreStatus, NAMESPACE};
