//! [`Namespace`]: a graph with its catalog and commit position, changed
//! only through the commit pipeline.

use ironweaver_core::{GraphError, Key, NodeIx};

use crate::catalog::{AttrPath, ConstraintKind, NamespaceCatalog, NamespaceName};
use crate::idempotency::{fingerprint_catalog, fingerprint_data, IdempotencyKey, KeyEntry, KeyTable, Keyed};
use crate::mutation::{CatalogChange, Change, CommitRecord, CommitResult, Mutation};
use crate::{codec, resolve, CommitTime, DbGraph, Error};

/// One namespace in memory: its graph, its catalog, the `seq` of its last
/// commit and the table of its recent idempotency keys.
///
/// Every change goes through the commit pipeline, and the graph is only
/// ever lent out immutably ([`graph`](Self::graph)). A commit is
///
/// 1. **prepared** ([`prepare`](Self::prepare) /
///    [`prepare_catalog`](Self::prepare_catalog)): resolved into a
///    [`CommitRecord`] (core ops with explicit edge ids and versions, or a
///    catalog change) and validated (versions, existence, reserved names,
///    value depth, constraints), without touching the graph;
/// 2. **applied** ([`apply`](Self::apply)): the record is applied to the
///    graph, the indexes are flushed and the record's `seq` becomes the
///    namespace's.
///
/// [`commit`](Self::commit) does both. With an idempotency key
/// ([`prepare_keyed`](Self::prepare_keyed), step 8), preparing first looks
/// the key up in the [`KeyTable`]: a known key with the same request gives
/// the original result ([`Prepare::Duplicate`]) and nothing is applied; a
/// known key with another request is [`Error::IdempotencyKeyReused`].
/// Applying a keyed record adds it to the table, so replay rebuilds it.
///
/// The write-ahead log (step 4) goes
/// between the two: the record is logged after it is validated and before
/// it is applied. A commit that fails to prepare changes nothing and uses
/// no `seq`.
///
/// Guarantees: a commit is all or nothing; it sees and checks the state
/// after all of its mutations; replaying the records of every successful
/// commit in order onto an empty namespace ([`replay`](Self::replay))
/// gives the same graph (as compared by
/// [`canonical`](crate::testutil::canonical)), catalog and `seq`. Not
/// thread-safe by itself (`&mut self`); `iwdb_storage::LoggedNamespace` adds
/// the locks (step 8).
///
/// If applying a validated record fails (a bug, or `GraphError::Internal`
/// from the core, after which the graph may be inconsistent), the
/// namespace is **poisoned**: it rejects all further commits with
/// [`Error::Poisoned`] and must be reopened from checkpoint and log.
///
/// Only an immutable borrow of the graph is available:
///
/// ```compile_fail
/// use iwdb_engine::{catalog::NamespaceName, DbRecord, Namespace};
/// let mut ns = Namespace::new(NamespaceName::new("n").unwrap());
/// ns.graph().add_node("a", DbRecord::default()); // needs &mut DbGraph
/// ```
#[derive(Debug)]
pub struct Namespace {
    name: NamespaceName,
    catalog: NamespaceCatalog,
    graph: DbGraph,
    seq: u64,
    keys: KeyTable,
    poisoned: bool,
}

/// The keys of an index being built off the write lock (step 9, ADR 0019):
/// for each node the scan saw, its index key and its version at the time.
/// [`Namespace::apply_built`] installs it, checking each node's version
/// again; nodes that changed or appeared meanwhile are re-read then.
#[derive(Debug)]
pub struct IndexBuild {
    path: AttrPath,
    keys: Vec<(NodeIx, Option<Key>, u64)>,
}

impl IndexBuild {
    pub fn new(path: AttrPath) -> Self {
        IndexBuild { path, keys: Vec::new() }
    }

    pub fn path(&self) -> &AttrPath {
        &self.path
    }

    /// Nodes scanned so far.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// What preparing a keyed commit gives.
#[derive(Clone, Debug, PartialEq)]
pub enum Prepare {
    /// A new commit, to log and apply.
    New(Prepared),
    /// The key's commit is in the table with the same request: its
    /// original result, with `deduplicated` set. Nothing to log or apply.
    Duplicate(CommitResult),
}

/// A validated commit, ready to be logged and applied to the namespace
/// that prepared it (before any other commit there: otherwise
/// [`Error::OutOfOrder`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    record: CommitRecord,
    result: CommitResult,
}

impl Prepared {
    /// The record to log.
    pub fn record(&self) -> &CommitRecord {
        &self.record
    }

    /// What [`Namespace::apply`] will return (without the commit time).
    pub fn result(&self) -> &CommitResult {
        &self.result
    }

    /// Carry an idempotency key: the record logs it with the result.
    fn with_key(mut self, key: IdempotencyKey, fingerprint: u32) -> Self {
        let result = &self.result;
        self.record.keyed =
            Some(Keyed { key, fingerprint, edge_ids: result.edge_ids.clone(), versions: result.versions.clone() });
        self
    }
}

impl Namespace {
    /// An empty namespace with an empty catalog, at `seq` 0.
    pub fn new(name: NamespaceName) -> Self {
        Namespace {
            name,
            catalog: NamespaceCatalog::new(),
            graph: DbGraph::new(),
            seq: 0,
            keys: KeyTable::new(),
            poisoned: false,
        }
    }

    /// A namespace from a loaded database file (a checkpoint): its graph,
    /// catalog and seq. The loader has already made the graph's indexes
    /// match the catalog and flushed them ([`codec::Loaded`]), which is the
    /// invariant every namespace keeps. Commits and replay continue at
    /// `loaded.meta.seq + 1`.
    ///
    /// Recovery (step 5) and the checkpointer build their namespace this
    /// way; [`new`](Self::new) only makes an empty one.
    pub fn from_loaded(loaded: codec::Loaded) -> Self {
        let codec::Loaded { graph, meta, index_changes: _ } = loaded;
        Namespace {
            name: meta.namespace,
            catalog: meta.catalog,
            graph,
            seq: meta.seq,
            keys: meta.keys,
            poisoned: false,
        }
    }

    /// The graph meta a checkpoint of this namespace is saved with: its
    /// name, catalog, seq and idempotency key table.
    pub fn graph_meta(&self) -> codec::GraphMeta {
        codec::GraphMeta {
            namespace: self.name.clone(),
            catalog: self.catalog.clone(),
            seq: self.seq,
            keys: self.keys.clone(),
        }
    }

    pub fn name(&self) -> &NamespaceName {
        &self.name
    }

    /// The graph, for reading. Its property indexes match
    /// [`NamespaceCatalog::index_paths`] and are flushed.
    pub fn graph(&self) -> &DbGraph {
        &self.graph
    }

    pub fn catalog(&self) -> &NamespaceCatalog {
        &self.catalog
    }

    /// The `seq` of the last applied commit (0: none yet).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The recent keyed commits (step 8).
    pub fn keys(&self) -> &KeyTable {
        &self.keys
    }

    /// The handles of every node, for [`scan_index_keys`](Self::scan_index_keys)
    /// in chunks. O(n).
    pub fn node_handles(&self) -> Vec<NodeIx> {
        self.graph.node_indices().collect()
    }

    /// Read the index keys of the nodes `handles` (handles that are stale
    /// by now are skipped) into `build`. Reads only: run it under the read
    /// lock, a chunk at a time.
    pub fn scan_index_keys(&self, handles: &[NodeIx], build: &mut IndexBuild) -> Result<(), Error> {
        use ironweaver_core::Attributes;
        for &ix in handles {
            let Some(node) = self.graph.node(ix) else { continue };
            let key = node.data.with_value(build.path.keys(), |v| v.and_then(Key::of))?;
            build.keys.push((ix, key, node.data.version));
        }
        Ok(())
    }

    /// The index path a catalog change needs and the graph lacks, if any:
    /// what an online build would build.
    pub fn index_needed(&self, change: &CatalogChange) -> Option<AttrPath> {
        let path = match change {
            CatalogChange::CreateIndex(index) => &index.path,
            CatalogChange::AddConstraint(c) if c.kind == ConstraintKind::Unique => &c.path,
            _ => return None,
        };
        (path.keys() != ["labels"] && !self.graph.has_index(path.keys())).then(|| path.clone())
    }

    /// Whether a failed apply poisoned the namespace (see the type docs).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Prepare and apply a data transaction: all mutations or none.
    pub fn commit(&mut self, mutations: &[Mutation]) -> Result<CommitResult, Error> {
        let prepared = self.prepare(mutations)?;
        self.apply(prepared, None)
    }

    /// Prepare and apply a catalog change.
    pub fn commit_catalog(&mut self, change: CatalogChange) -> Result<CommitResult, Error> {
        let prepared = self.prepare_catalog(change)?;
        self.apply(prepared, None)
    }

    /// [`prepare`](Self::prepare) with an idempotency key: the original
    /// result if the key's commit is in the table with the same mutations
    /// ([`Prepare::Duplicate`]), [`Error::IdempotencyKeyReused`] if it is
    /// with others; otherwise a new commit whose record carries the key.
    /// The lookup comes first, so a duplicate is found even if the request
    /// would fail validation now. O(request) for the fingerprint.
    pub fn prepare_keyed(&self, mutations: &[Mutation], key: Option<&IdempotencyKey>) -> Result<Prepare, Error> {
        let Some(key) = key else { return Ok(Prepare::New(self.prepare(mutations)?)) };
        let fingerprint = fingerprint_data(mutations)?;
        if let Some(result) = self.duplicate(key, fingerprint)? {
            return Ok(Prepare::Duplicate(result));
        }
        Ok(Prepare::New(self.prepare(mutations)?.with_key(key.clone(), fingerprint)))
    }

    /// [`prepare_catalog`](Self::prepare_catalog) with an idempotency key,
    /// like [`prepare_keyed`](Self::prepare_keyed).
    pub fn prepare_catalog_keyed(&self, change: CatalogChange, key: Option<&IdempotencyKey>) -> Result<Prepare, Error> {
        let Some(key) = key else { return Ok(Prepare::New(self.prepare_catalog(change)?)) };
        let fingerprint = fingerprint_catalog(&change)?;
        if let Some(result) = self.duplicate(key, fingerprint)? {
            return Ok(Prepare::Duplicate(result));
        }
        Ok(Prepare::New(self.prepare_catalog(change)?.with_key(key.clone(), fingerprint)))
    }

    /// The original result of the commit with `key`, if the table holds it
    /// with the same fingerprint.
    fn duplicate(&self, key: &IdempotencyKey, fingerprint: u32) -> Result<Option<CommitResult>, Error> {
        match self.keys.get(key) {
            None => Ok(None),
            Some(entry) if entry.fingerprint == fingerprint => {
                Ok(Some(CommitResult { deduplicated: true, ..entry.result.clone() }))
            }
            Some(entry) => Err(Error::IdempotencyKeyReused { key: key.clone(), seq: entry.result.seq }),
        }
    }

    /// Resolve and validate a data transaction. Doesn't change anything.
    /// O(size of the transaction and the entities it touches), plus one
    /// index lookup per unique constraint and written node.
    pub fn prepare(&self, mutations: &[Mutation]) -> Result<Prepared, Error> {
        self.check_usable()?;
        if mutations.is_empty() {
            return Err(Error::EmptyTransaction);
        }
        let seq = self.next_seq()?;
        let resolved = resolve::resolve(&self.graph, &self.catalog, mutations)?;
        Ok(Prepared {
            record: CommitRecord::new(seq, Change::Data(resolved.ops)),
            result: CommitResult {
                seq,
                edge_ids: resolved.edge_ids,
                versions: resolved.versions,
                ..Default::default()
            },
        })
    }

    /// Validate a catalog change against the catalog and the data. Adding
    /// a constraint scans the nodes with its label. Doesn't change anything.
    pub fn prepare_catalog(&self, change: CatalogChange) -> Result<Prepared, Error> {
        self.check_usable()?;
        let seq = self.next_seq()?;
        match &change {
            CatalogChange::CreateIndex(index) => {
                if self.catalog.has_index(index) {
                    return Err(Error::IndexExists { path: index.path.clone() });
                }
                check_indexable(&index.path)?;
            }
            CatalogChange::DropIndex(index) => {
                if !self.catalog.has_index(index) {
                    return Err(Error::NoSuchIndex { path: index.path.clone() });
                }
            }
            CatalogChange::AddConstraint(constraint) => {
                if self.catalog.has_constraint(constraint) {
                    return Err(Error::ConstraintExists { constraint: constraint.clone() });
                }
                if constraint.kind == ConstraintKind::Unique {
                    check_indexable(&constraint.path)?;
                }
                resolve::check_existing(&self.graph, constraint)?;
            }
            CatalogChange::DropConstraint(constraint) => {
                if !self.catalog.has_constraint(constraint) {
                    return Err(Error::NoSuchConstraint { constraint: constraint.clone() });
                }
            }
        }
        Ok(Prepared {
            record: CommitRecord::new(seq, Change::Catalog(change)),
            result: CommitResult { seq, ..CommitResult::default() },
        })
    }

    /// Apply a prepared commit, appended to the log at `time` (`None`
    /// without a log). Fails with [`Error::OutOfOrder`] if another commit
    /// was applied since it was prepared (nothing changes then), and with
    /// [`Error::ApplyFailed`] (poisoning the namespace) if the graph
    /// rejects it. Returns the result with its time.
    pub fn apply(&mut self, prepared: Prepared, time: Option<CommitTime>) -> Result<CommitResult, Error> {
        self.apply_built(prepared, time, None)
    }

    /// [`apply`](Self::apply) a catalog change with the keys of its index
    /// already read ([`scan_index_keys`](Self::scan_index_keys)): the index
    /// is created from them, minus every node whose version changed or
    /// that is gone, and the nodes the scan didn't see are indexed by the
    /// flush that follows. So the write lock is held for the insertion of
    /// the keys, not for reading every node's payload.
    pub fn apply_built(
        &mut self,
        prepared: Prepared,
        time: Option<CommitTime>,
        build: Option<IndexBuild>,
    ) -> Result<CommitResult, Error> {
        let Prepared { record, result } = prepared;
        if let Some(build) = build {
            if !self.poisoned && record.seq == self.seq.wrapping_add(1) && !self.graph.has_index(build.path.keys()) {
                let graph = &self.graph;
                let fresh: Vec<_> = build
                    .keys
                    .into_iter()
                    .filter(|(ix, _, version)| graph.node(*ix).is_some_and(|n| n.data.version == *version))
                    .map(|(ix, key, _)| (ix, key))
                    .collect();
                // An error leaves the graph without the index; the apply
                // below then builds it the plain way
                let _ = self.graph.create_index_with_keys(build.path.keys(), fresh);
            }
        }
        self.apply_record(record, time)?;
        Ok(CommitResult { time, ..result })
    }

    /// Apply a record from the log without validating it again (recovery
    /// and replicas), with its commit time from the log (the key table
    /// keeps it for keyed records). Records must come in `seq` order,
    /// without gaps: otherwise [`Error::OutOfOrder`], and nothing changes.
    /// If the graph rejects the record, the namespace is poisoned.
    pub fn replay(&mut self, record: CommitRecord, time: Option<CommitTime>) -> Result<(), Error> {
        self.apply_record(record, time)
    }

    fn check_usable(&self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        Ok(())
    }

    fn next_seq(&self) -> Result<u64, Error> {
        self.seq.checked_add(1).ok_or(Error::SeqExhausted)
    }

    fn apply_record(&mut self, record: CommitRecord, time: Option<CommitTime>) -> Result<(), Error> {
        self.check_usable()?;
        let expected = self.next_seq()?;
        if record.seq != expected {
            return Err(Error::OutOfOrder { expected, found: record.seq });
        }
        let outcome = match record.change {
            Change::Data(ops) => match self.graph.apply_all(ops) {
                Ok(_) => self.graph.flush_indexes(),
                Err((_, error)) => Err(error),
            },
            Change::Catalog(change) => {
                match change {
                    CatalogChange::CreateIndex(index) => self.catalog.add_index(index),
                    CatalogChange::DropIndex(index) => self.catalog.remove_index(&index),
                    CatalogChange::AddConstraint(constraint) => self.catalog.add_constraint(constraint),
                    CatalogChange::DropConstraint(constraint) => self.catalog.remove_constraint(&constraint),
                };
                self.catalog.apply_indexes(&mut self.graph).map(drop).map_err(|e| match e {
                    Error::Graph(error) => error,
                    other => GraphError::Internal(other.to_string()),
                })
            }
        };
        self.finish_apply(record.seq, outcome)?;
        if let Some(Keyed { key, fingerprint, edge_ids, versions }) = record.keyed {
            let result = CommitResult { seq: record.seq, edge_ids, versions, time, deduplicated: false };
            self.keys.insert(KeyEntry { key, fingerprint, result });
        }
        Ok(())
    }

    /// Advance `seq`, or poison the namespace if applying failed.
    fn finish_apply(&mut self, seq: u64, outcome: Result<(), GraphError>) -> Result<(), Error> {
        match outcome {
            Ok(()) => {
                self.seq = seq;
                Ok(())
            }
            Err(error) => {
                self.poisoned = true;
                Err(Error::ApplyFailed { seq, error })
            }
        }
    }
}

/// The core can't index `labels` (the node's labels).
fn check_indexable(path: &AttrPath) -> Result<(), Error> {
    if path.keys() == ["labels"] {
        return Err(Error::UnindexablePath { path: path.clone() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::Target;
    use ironweaver_core::Attrs;

    fn upsert(id: &str) -> Mutation {
        Mutation::UpsertNode {
            id: id.into(),
            labels: vec![],
            attr: Attrs::new(),
            meta: Attrs::new(),
            expected_version: None,
        }
    }

    #[test]
    fn a_failed_apply_poisons_the_namespace() {
        let mut ns = Namespace::new(NamespaceName::new("n").expect("name"));
        ns.commit(&[upsert("a")]).expect("commit");
        let prepared = ns.prepare(&[upsert("b")]).expect("prepare");

        // As if `apply_all` had returned an internal error for the next record
        let error = GraphError::Internal("undo failed".into());
        assert_eq!(ns.finish_apply(2, Err(error.clone())), Err(Error::ApplyFailed { seq: 2, error }));
        assert!(ns.is_poisoned());
        assert_eq!(ns.seq(), 1);
        assert_eq!(ns.apply(prepared, None), Err(Error::Poisoned));
        assert_eq!(ns.commit(&[upsert("c")]), Err(Error::Poisoned));
        assert_eq!(ns.prepare(&[upsert("c")]), Err(Error::Poisoned));
        let record = CommitRecord::new(2, Change::Data(vec![]));
        assert_eq!(ns.replay(record, None), Err(Error::Poisoned));
    }

    #[test]
    fn a_record_the_graph_rejects_poisons_the_namespace() {
        let mut ns = Namespace::new(NamespaceName::new("n").expect("name"));
        let bad = CommitRecord::new(1, Change::Data(vec![ironweaver_core::Op::RemoveNode { id: "x".into() }]));
        assert_eq!(
            ns.replay(bad, None),
            Err(Error::ApplyFailed { seq: 1, error: GraphError::NodeNotFound("x".into()) })
        );
        assert!(ns.is_poisoned());
        assert_eq!(ns.seq(), 0);
    }

    #[test]
    fn prepared_commits_apply_in_order_only() {
        let mut ns = Namespace::new(NamespaceName::new("n").expect("name"));
        let first = ns.prepare(&[upsert("a")]).expect("prepare");
        let second = ns.prepare(&[upsert("b")]).expect("prepare");
        assert_eq!(first.record().seq, 1);
        assert_eq!(second.record().seq, 1);
        assert_eq!(first.result().versions, vec![(Target::Node("a".into()), 1)]);
        ns.apply(first, None).expect("apply");
        // `second` was resolved against the state before `first`
        assert_eq!(ns.apply(second, None), Err(Error::OutOfOrder { expected: 2, found: 1 }));
        assert!(!ns.is_poisoned());
        assert_eq!(ns.seq(), 1);
    }
}
