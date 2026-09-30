//! [`Namespace`]: a graph with its catalog and commit position, changed
//! only through the commit pipeline.

use ironweaver_core::GraphError;

use crate::catalog::{AttrPath, ConstraintKind, NamespaceCatalog, NamespaceName};
use crate::mutation::{CatalogChange, Change, CommitRecord, CommitResult, Mutation};
use crate::{codec, resolve, DbGraph, Error};

/// One namespace in memory: its graph, its catalog and the `seq` of its
/// last commit.
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
/// [`commit`](Self::commit) does both. The write-ahead log (step 4) goes
/// between the two: the record is logged after it is validated and before
/// it is applied. A commit that fails to prepare changes nothing and uses
/// no `seq`.
///
/// Guarantees: a commit is all or nothing; it sees and checks the state
/// after all of its mutations; replaying the records of every successful
/// commit in order onto an empty namespace ([`replay`](Self::replay))
/// gives the same graph (as compared by
/// [`canonical`](crate::testutil::canonical)), catalog and `seq`. Not
/// thread-safe by itself (`&mut self`); concurrency is step 8.
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
    poisoned: bool,
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

    /// What [`Namespace::apply`] will return.
    pub fn result(&self) -> &CommitResult {
        &self.result
    }
}

impl Namespace {
    /// An empty namespace with an empty catalog, at `seq` 0.
    pub fn new(name: NamespaceName) -> Self {
        Namespace { name, catalog: NamespaceCatalog::new(), graph: DbGraph::new(), seq: 0, poisoned: false }
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
        Namespace { name: meta.namespace, catalog: meta.catalog, graph, seq: meta.seq, poisoned: false }
    }

    /// The graph meta a checkpoint of this namespace is saved with: its
    /// name, catalog and seq.
    pub fn graph_meta(&self) -> codec::GraphMeta {
        codec::GraphMeta { namespace: self.name.clone(), catalog: self.catalog.clone(), seq: self.seq }
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

    /// Whether a failed apply poisoned the namespace (see the type docs).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Prepare and apply a data transaction: all mutations or none.
    pub fn commit(&mut self, mutations: &[Mutation]) -> Result<CommitResult, Error> {
        let prepared = self.prepare(mutations)?;
        self.apply(prepared)
    }

    /// Prepare and apply a catalog change.
    pub fn commit_catalog(&mut self, change: CatalogChange) -> Result<CommitResult, Error> {
        let prepared = self.prepare_catalog(change)?;
        self.apply(prepared)
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
            record: CommitRecord { seq, change: Change::Data(resolved.ops) },
            result: CommitResult { seq, edge_ids: resolved.edge_ids, versions: resolved.versions },
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
            record: CommitRecord { seq, change: Change::Catalog(change) },
            result: CommitResult { seq, ..CommitResult::default() },
        })
    }

    /// Apply a prepared commit. Fails with [`Error::OutOfOrder`] if another
    /// commit was applied since it was prepared (nothing changes then), and
    /// with [`Error::ApplyFailed`] (poisoning the namespace) if the graph
    /// rejects it.
    pub fn apply(&mut self, prepared: Prepared) -> Result<CommitResult, Error> {
        let Prepared { record, result } = prepared;
        self.apply_record(record)?;
        Ok(result)
    }

    /// Apply a record from the log without validating it again (recovery
    /// and replicas). Records must come in `seq` order, without gaps:
    /// otherwise [`Error::OutOfOrder`], and nothing changes. If the graph
    /// rejects the record, the namespace is poisoned.
    pub fn replay(&mut self, record: CommitRecord) -> Result<(), Error> {
        self.apply_record(record)
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

    fn apply_record(&mut self, record: CommitRecord) -> Result<(), Error> {
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
        self.finish_apply(record.seq, outcome)
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
        assert_eq!(ns.apply(prepared), Err(Error::Poisoned));
        assert_eq!(ns.commit(&[upsert("c")]), Err(Error::Poisoned));
        assert_eq!(ns.prepare(&[upsert("c")]), Err(Error::Poisoned));
        let record = CommitRecord { seq: 2, change: Change::Data(vec![]) };
        assert_eq!(ns.replay(record), Err(Error::Poisoned));
    }

    #[test]
    fn a_record_the_graph_rejects_poisons_the_namespace() {
        let mut ns = Namespace::new(NamespaceName::new("n").expect("name"));
        let bad =
            CommitRecord { seq: 1, change: Change::Data(vec![ironweaver_core::Op::RemoveNode { id: "x".into() }]) };
        assert_eq!(ns.replay(bad), Err(Error::ApplyFailed { seq: 1, error: GraphError::NodeNotFound("x".into()) }));
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
        ns.apply(first).expect("apply");
        // `second` was resolved against the state before `first`
        assert_eq!(ns.apply(second), Err(Error::OutOfOrder { expected: 2, found: 1 }));
        assert!(!ns.is_poisoned());
        assert_eq!(ns.seq(), 1);
    }
}
