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
//! **The `Database` trait** (step 10): [`Embedded`] serves a store
//! through [`iwdb_query::Database`], the service interface every access
//! method uses: bounded reads, pattern matching, analytics, the catalog
//! and namespaces, with stable error codes.
//!
//! Operations: [`Store::backup`] (an online backup), continuous WAL
//! archiving ([`StoreOptions::archive`]), [`restore`] (point-in-time
//! recovery from a backup and/or an archive), [`verify`] (every file and
//! invariant, without writing) and [`status`].
//!
//! [`Store`] translates only: commits go through the engine's commit
//! pipeline ([`iwdb_engine::Namespace`]) and the WAL
//! ([`iwdb_storage::LoggedNamespace`]); opening runs recovery
//! ([`iwdb_storage::recover`]); checkpoints are written by
//! [`iwdb_storage::Checkpointer`]. The guarantees are in
//! `documentation/guarantees.md`, the directory layout in
//! `documentation/formats/data-dir.md`.

mod embedded;
mod ops;
mod options;
mod request;
mod store;

pub use embedded::{Embedded, QueryConfig};
pub use ironweaver_core::cancel::Token as CancelToken;
pub use ironweaver_core::pathfinding::EdgeCost;
pub use ironweaver_core::{Attrs, EdgeId, Value};
pub use ironweaver_core::{Direction, Projection};
pub use iwdb_engine::catalog::{
    AttrPath, CatalogError, Constraint, ConstraintKind, IndexDef, Label, NamespaceCatalog, NamespaceName,
};
pub use iwdb_engine::{
    CatalogChange, CommitResult, CommitTime, EdgeKey, IdempotencyKey, KeyTable, Mutation, Namespace, Target,
};
pub use iwdb_query::{CommitOptions, Edge, IndexSize, IndexState, IndexStatus, NamespaceStatus, Node, ProjectionSpec};
pub use iwdb_storage::io::{LogFs, StdFs};
pub use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};
pub use iwdb_storage::{
    BackupReport, CheckpointOutcome, CutTail, DirStatus, Error, Finding, FsyncPolicy, HistoryId, Kind, LockStats,
    NamespaceBackup, NamespaceFiles, NamespaceRestore, NamespaceVerify, RecoveryReport, RestoreReport, RestoreSources,
    RestoreTarget, SkippedCheckpoint, StoreRecovery, VerifyReport, WalOptions,
};
pub use ops::{restore, restore_namespaces, restore_with, restore_with_only, status, verify, Status};
pub use options::{CheckpointOptions, StoreOptions};
pub use request::{ReadOptions, DEFAULT_TIMEOUT};
pub use store::{Analysis, Ns, Store, StoreStatus, NAMESPACE};
