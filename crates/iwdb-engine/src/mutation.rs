//! The write vocabulary ([`Mutation`]) and what a commit produces: the
//! [`CommitRecord`] the log stores and the [`CommitResult`] the caller gets.
//!
//! A transaction is a list of mutations, committed all or nothing by
//! [`Namespace::commit`](crate::Namespace::commit). The commit pipeline
//! resolves the mutations against the current graph into plain core ops
//! with explicit edge ids and versions: that list is the commit record,
//! and replaying the records in `seq` order onto an empty namespace
//! reproduces its state exactly.
//!
//! # Versions
//!
//! Every node and edge has a version (see [`DbRecord`]):
//!
//! - a commit that writes an entity sets its version to its version before
//!   the commit plus 1, once per commit however many of the commit's
//!   mutations address it; an entity that didn't exist before the commit
//!   counts as version 0, so a new entity gets version 1;
//! - every successful mutation that addresses an entity writes it, even if
//!   it changes nothing (like an SQL `UPDATE` that sets a column to its
//!   value). Deleted entities have no version;
//! - `expected_version` is compared with the version before the commit (the
//!   state the client read), not with changes earlier in the same
//!   transaction. `Some(0)` means "must not exist before the commit";
//! - a node deleted in one commit and created again in a later one starts
//!   again at 1: the database keeps no tombstones. A client that must not
//!   miss such a recreation guards on something else (for example a unique
//!   attribute). A node deleted and created again within one commit gets
//!   the old version plus 1, like any other write;
//! - versions never wrap: a write that would go above `i64::MAX` (the
//!   largest version a file can hold) fails with
//!   [`Error::VersionOverflow`](crate::Error::VersionOverflow).

use std::fmt;

use ironweaver_core::{Attrs, EdgeId, Op, Value};
use serde::{Deserialize, Serialize};

use crate::catalog::{Constraint, IndexDef};
use crate::{CommitTime, DbRecord, Keyed, Mark};

/// Deepest nesting of an attribute or meta value a mutation may carry, the
/// core's [`MAX_DEPTH`](ironweaver_core::format::MAX_DEPTH): a value is
/// depth 1 and a container's items are one deeper, so an empty list at
/// depth 100 is fine. The file format and `Value`'s serde (which the log
/// uses) count the same way since `3b15149` (upstream #31).
pub const MAX_VALUE_DEPTH: usize = ironweaver_core::format::MAX_DEPTH;

/// A node (by id) or an edge (by id).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Target {
    Node(String),
    Edge(EdgeId),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Node(id) => write!(f, "node '{}'", id),
            Target::Edge(id) => write!(f, "edge {}", id.0),
        }
    }
}

/// How an edge upsert finds its edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKey {
    /// The edge with this id, which must exist.
    Id(EdgeId),
    /// The edge from `from` to `to` with exactly this type (`None`: an
    /// untyped edge). No such edge: one is added. More than one:
    /// [`Error::AmbiguousEdge`](crate::Error::AmbiguousEdge).
    Endpoints { from: String, to: String, ty: Option<String> },
}

/// One change in a transaction.
///
/// `expected_version` (optimistic concurrency, see the [module
/// comment](self)): `None` skips the check, `Some(v)` fails the commit with
/// [`Error::Conflict`](crate::Error::Conflict) unless the entity's version
/// before the commit is `v` (0: it didn't exist).
///
/// Top-level attribute keys and meta keys must not start with `iwdb.`
/// ([`reserved`](crate::reserved)); values may be nested at most
/// [`MAX_VALUE_DEPTH`] levels.
///
/// Serde writes `attr` and `meta` sorted by key, so equal mutations encode
/// to equal bytes (the fingerprint of an idempotent request depends on it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Mutation {
    /// Create the node, or replace its attributes and meta. The labels are
    /// added; labels the node has already are kept (remove them with
    /// [`Mutation::RemoveLabel`]).
    UpsertNode {
        id: String,
        labels: Vec<String>,
        #[serde(serialize_with = "ironweaver_core::value::serialize_sorted")]
        attr: Attrs,
        #[serde(serialize_with = "ironweaver_core::value::serialize_sorted")]
        meta: Attrs,
        expected_version: Option<u64>,
    },
    /// Delete the node and its edges.
    DeleteNode {
        id: String,
        expected_version: Option<u64>,
    },
    /// Add an edge with a new id (in [`CommitResult::edge_ids`]). Both
    /// endpoints must exist.
    AddEdge {
        from: String,
        to: String,
        ty: Option<String>,
        #[serde(serialize_with = "ironweaver_core::value::serialize_sorted")]
        attr: Attrs,
        #[serde(serialize_with = "ironweaver_core::value::serialize_sorted")]
        meta: Attrs,
    },
    /// Replace an edge's attributes and meta, or add the edge (for
    /// [`EdgeKey::Endpoints`]). Its id is in [`CommitResult::edge_ids`].
    UpsertEdge {
        key: EdgeKey,
        #[serde(serialize_with = "ironweaver_core::value::serialize_sorted")]
        attr: Attrs,
        #[serde(serialize_with = "ironweaver_core::value::serialize_sorted")]
        meta: Attrs,
        expected_version: Option<u64>,
    },
    DeleteEdge {
        id: EdgeId,
        expected_version: Option<u64>,
    },
    /// Set one top-level attribute.
    SetAttr {
        target: Target,
        key: String,
        value: Value,
        expected_version: Option<u64>,
    },
    /// Remove one top-level attribute (nothing to remove is fine).
    RemoveAttr {
        target: Target,
        key: String,
        expected_version: Option<u64>,
    },
    /// Append `value` to the list in attribute `key`; a missing or none
    /// attribute becomes `[value]`, any other value is
    /// [`Error::NotAList`](crate::Error::NotAList).
    AppendAttr {
        target: Target,
        key: String,
        value: Value,
        expected_version: Option<u64>,
    },
    AddLabel {
        id: String,
        label: String,
        expected_version: Option<u64>,
    },
    RemoveLabel {
        id: String,
        label: String,
        expected_version: Option<u64>,
    },
    /// Set (`Some`) or clear (`None`) an edge's type.
    SetEdgeType {
        id: EdgeId,
        ty: Option<String>,
        expected_version: Option<u64>,
    },
}

/// A change to a namespace's catalog, committed on its own (never mixed
/// with data mutations). Each is validated against the catalog and the
/// existing data; the graph's property indexes follow the catalog
/// ([`NamespaceCatalog::index_paths`](crate::catalog::NamespaceCatalog::index_paths)).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatalogChange {
    /// Declare an index (fails if declared already) and build it.
    CreateIndex(IndexDef),
    /// Remove an index declaration; the graph keeps the index while a
    /// unique constraint needs it.
    DropIndex(IndexDef),
    /// Add a constraint; fails if the existing data violates it.
    AddConstraint(Constraint),
    DropConstraint(Constraint),
}

/// What a commit changes: the kinds of log record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Change {
    /// Resolved data changes: core ops with explicit edge ids and the new
    /// versions, applied with `Graph::apply_all`. Versions are set by
    /// `SetNodeAttr` / `SetEdgeAttr` on the reserved key
    /// [`VERSION_KEY`](crate::reserved::VERSION_KEY) (ADR 0004), or carried
    /// in the records of `AddNode`, `AddEdge`, `SetNode` and `SetEdge`.
    Data(Vec<Op<DbRecord, DbRecord>>),
    Catalog(CatalogChange),
}

/// One committed transaction, as the write-ahead log stores it:
/// its sequence number and its resolved change. Applying the records of a
/// namespace in `seq` order to an empty namespace reproduces its state
/// ([`Namespace::replay`](crate::Namespace::replay)). Serde (postcard in
/// the log) writes maps sorted, so equal records encode to equal bytes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitRecord {
    /// Position in the namespace's history: 1 for the first commit, one
    /// more for each successful commit. Failed commits don't use one.
    pub seq: u64,
    pub change: Change,
    /// Set for a commit made with an idempotency key (WAL format 3, step
    /// 8): the key and the result, which replay puts into the key table.
    pub keyed: Option<Keyed>,
    /// Set for a commit that moved a mark (WAL format 4, step 13): replay
    /// sets the mark to its position.
    pub mark: Option<Mark>,
}

impl CommitRecord {
    /// A record without an idempotency key or mark.
    pub fn new(seq: u64, change: Change) -> Self {
        CommitRecord { seq, change, keyed: None, mark: None }
    }
}

/// The outcome of a successful commit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResult {
    pub seq: u64,
    /// One id per [`Mutation::AddEdge`] and [`Mutation::UpsertEdge`], in
    /// mutation order: the edge added or updated.
    pub edge_ids: Vec<EdgeId>,
    /// The new version of every node and edge the commit wrote that exists
    /// after it, sorted (nodes by id, then edges by id). Empty for catalog
    /// changes.
    pub versions: Vec<(Target, u64)>,
    /// When the WAL appended the commit (ADR 0010). `None` for a
    /// commit applied without a log, and for records of WAL format 1.
    pub time: Option<CommitTime>,
    /// True if this commit was not applied now: its idempotency key was
    /// found, and this is the original commit's result (ADR 0015).
    pub deduplicated: bool,
}
