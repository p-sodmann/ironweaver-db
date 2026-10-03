//! What the [`Database`](crate::Database) trait returns: entities, answers
//! with their seq and cursor, and namespace status.

use ironweaver_core::{Attrs, EdgeId, EdgeIx, NodeIx};
use iwdb_engine::catalog::AttrPath;
use iwdb_engine::{Change, CommitTime, DbGraph, IdempotencyKey};
use iwdb_storage::RecoveryReport;

use crate::Cursor;

/// A node, as read from the store.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    /// Sorted by name.
    pub labels: Vec<String>,
    pub attr: Attrs,
    /// User meta (without the database's `iwdb.*` keys).
    pub meta: Attrs,
    pub version: u64,
}

impl Node {
    /// The node at `ix`, if it is live.
    pub fn read(g: &DbGraph, ix: NodeIx) -> Option<Node> {
        let node = g.node(ix)?;
        let mut labels: Vec<String> = g.label_names(ix)?.into_iter().map(str::to_owned).collect();
        labels.sort_unstable();
        let data = &node.data;
        Some(Node {
            id: node.id().to_owned(),
            labels,
            attr: data.attr.clone(),
            meta: data.meta.clone(),
            version: data.version,
        })
    }
}

/// An edge, as read from the store.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub id: EdgeId,
    pub from: String,
    pub to: String,
    pub ty: Option<String>,
    pub attr: Attrs,
    pub meta: Attrs,
    pub version: u64,
}

impl Edge {
    /// The edge at `ix`, if it is live.
    pub fn read(g: &DbGraph, ix: EdgeIx) -> Option<Edge> {
        let edge = g.edge(ix)?;
        let data = &edge.data;
        Some(Edge {
            id: edge.id(),
            from: g.node(edge.source())?.id().to_owned(),
            to: g.node(edge.target())?.id().to_owned(),
            ty: g.edge_type_name(ix).map(str::to_owned),
            attr: data.attr.clone(),
            meta: data.meta.clone(),
            version: data.version,
        })
    }
}

/// One commit in the change stream (ADR 0031): the WAL record as logged.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeEvent {
    pub seq: u64,
    /// The commit time (`None` for commits logged in WAL format 1).
    pub time: Option<CommitTime>,
    /// The idempotency key the commit was made with, if any.
    pub key: Option<IdempotencyKey>,
    /// The resolved data ops (with explicit edge ids and the version ops of
    /// ADR 0004), or the catalog change.
    pub change: Change,
}

/// A batch of the change stream ([`Database::changes`](crate::Database::changes)).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changes {
    /// Commits in seq order, without gaps, from the seq asked for.
    pub events: Vec<ChangeEvent>,
    /// Where the next batch starts: the seq after the last event (the seq
    /// asked for if there is none).
    pub next_seq: u64,
    /// The oldest seq still retained: asking for an older one fails with
    /// `not_retained`.
    pub first_seq: u64,
}

/// How much work a read did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Work {
    /// Nodes visited (see `Limits::max_visited`).
    pub visited: usize,
    /// Edges examined (see `Limits::max_edges`). Some operations can't
    /// count them yet and report 0 (see each operation).
    pub edges: usize,
}

/// The answer of a read.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer<T> {
    pub value: T,
    /// The seq of the namespace state the read saw: every commit up to it,
    /// none after it.
    pub seq: u64,
    /// The next page, for paginated reads with more results: pass it in
    /// `QueryOptions::cursor` with the same request.
    pub next: Option<Cursor>,
    /// A limit stopped the read and `QueryOptions::partial` asked for what
    /// was found: `value` is incomplete.
    pub truncated: bool,
    pub work: Work,
}

impl<T> Answer<T> {
    /// A complete answer at `seq`.
    pub fn at(seq: u64, value: T) -> Self {
        Answer { value, seq, next: None, truncated: false, work: Work::default() }
    }

    /// The answer with its value changed by `f`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Answer<U> {
        Answer { value: f(self.value), seq: self.seq, next: self.next, truncated: self.truncated, work: self.work }
    }
}

/// Options of a commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// Commit at most once under this key (ADR 0015): if the namespace has
    /// a commit with this key and the same request, it returns that
    /// commit's result (with `CommitResult::deduplicated` set) and commits
    /// nothing; with another request, it fails with `conflict`. The store
    /// remembers the last
    /// [`KEY_TABLE_CAPACITY`](iwdb_engine::idempotency::KEY_TABLE_CAPACITY)
    /// keyed commits of each namespace, across restarts, checkpoints,
    /// backups and restores.
    pub idempotency_key: Option<IdempotencyKey>,
}

/// The state of an index ([`IndexStatus`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexState {
    /// Built and kept up to date by every commit.
    Ready,
    /// An online build is reading the nodes (ADR 0019); the index isn't in
    /// the catalog yet, and reads don't use it.
    Building { scanned: usize, total: usize },
}

/// One index of a namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexStatus {
    pub path: AttrPath,
    pub state: IndexState,
    /// Declared with `CreateIndex`.
    pub declared: bool,
    /// Needed by a unique constraint.
    pub unique: bool,
    /// The index's size; `None` while it is being built.
    pub size: Option<IndexSize>,
}

/// How big an index is ([`IndexStatus::size`]), from the core's
/// `Graph::index_stats` (O(1)). The namespace's indexes are flushed after
/// every commit, so the counts are exact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexSize {
    /// Nodes with an indexable (scalar) value at the path.
    pub entries: usize,
    /// Distinct values.
    pub distinct_keys: usize,
    /// Approximate bytes the index uses (its share of
    /// [`NamespaceStatus::memory_bytes`]).
    pub memory_bytes: usize,
}

/// What an open namespace reports about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct NamespaceStatus {
    pub id: u64,
    pub name: String,
    /// When the namespace was created (0 for one that predates layout 4).
    pub created: CommitTime,
    /// The seq of the last applied commit.
    pub seq: u64,
    /// The highest seq known to be durable; `None` under the `off` fsync
    /// policy until an explicit sync.
    pub synced_seq: Option<u64>,
    /// The newest checkpoint's seq.
    pub checkpoint: Option<u64>,
    /// Why the namespace is read-only, if it is.
    pub read_only: Option<String>,
    /// The last checkpoint error, if the last checkpoint failed.
    pub checkpoint_failure: Option<String>,
    pub nodes: usize,
    pub edges: usize,
    /// Approximate bytes the graph uses, indexes included (the core's
    /// `Graph::memory_usage`: O(1)); each index's share is in its
    /// [`IndexSize`]. Payloads (attribute maps) are not counted.
    pub memory_bytes: usize,
    /// Declared indexes and those unique constraints need, and builds in
    /// progress, sorted by path.
    pub indexes: Vec<IndexStatus>,
    pub constraints: usize,
    /// The namespace's marks (ADR 0032), by name: how far each projection
    /// has committed its source's events.
    pub marks: Vec<MarkStatus>,
    /// What recovery did to this namespace when the store opened (for a
    /// namespace created since: nothing).
    pub recovery: RecoveryReport,
}

/// A mark of a namespace (ADR 0032): the position in an external log up to
/// which a projection's events are committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkStatus {
    pub name: String,
    pub position: u64,
    /// The seq of the commit that set it.
    pub seq: u64,
}
