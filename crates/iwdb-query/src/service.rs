//! The [`Database`] trait: the one service interface every access method
//! uses (design rule 8, ADR 0020).

use std::future::Future;

use ironweaver_core::EdgeId;
use iwdb_engine::catalog::NamespaceCatalog;
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, Mutation};
use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};

use crate::read::Explain;
use crate::{
    AnalyticsRequest, Answer, CommitOptions, Edge, Error, ExplainRequest, FindRequest, JobResult, MatchRequest,
    MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Path, PathRequest, QueryOptions, Subgraph, SubgraphRequest,
    TraverseRequest, WalkRequest,
};

/// A graph database: namespaces, commits, bounded reads, analytics and the
/// catalog. The embedded store (`iwdb::Embedded`) implements it; the gRPC
/// and REST servers (steps 11, 12) serve it, and their clients implement
/// it again, so the conformance suite (`conformance_tests!`, feature
/// `conformance`) runs against every access method.
///
/// **Every operation names a namespace.** Seqs, cursors, idempotency keys
/// and the catalog are per namespace.
///
/// **Every read is bounded** (design rule 5): by the limits of its
/// [`QueryOptions`] (or the server's defaults, never above its caps) and
/// by a timeout. Each operation says what it counts. A read reaching a
/// limit fails with `budget_exceeded`, or with `partial` answers what it
/// found ([`Answer::truncated`]); a timeout fails with `timeout`. A read
/// sees one committed state, never part of a commit; [`Answer::seq`] is
/// its seq. Reads hold the namespace's read lock while they run, so
/// commits wait for them to finish: the limits keep that short. Analytics
/// run on a projection without the lock.
///
/// **Paginated reads** (`find`, `neighbourhood`, `match_pattern`) return
/// results sorted by a key and a cursor ([`Answer::next`]) while there are
/// more. The next page is served at the seq of the first, or fails with
/// `cursor_expired` if the namespace has changed (no MVCC yet).
///
/// **Async.** Methods return `Send` futures (RPITIT, no `async-trait`;
/// the trait is not object safe: use it as a generic). Dropping a future
/// cancels its read. A commit's future, dropped, doesn't undo the commit:
/// its outcome is then unknown, and a retry with the same idempotency key
/// applies it at most once.
///
/// **Errors** have stable codes ([`Code`](crate::Code),
/// `documentation/api/errors.md`).
pub trait Database: Send + Sync {
    // ---- writes ----

    /// Commit a transaction: all mutations or none, validated against the
    /// state after all of them; durable per the store's fsync policy when
    /// it returns. Errors: `invalid_argument`, `not_found`, `conflict`,
    /// `constraint_violation`, `read_only`, `io` (outcome unknown).
    fn commit(
        &self,
        namespace: &str,
        mutations: Vec<Mutation>,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send;

    /// Commit a catalog change: create or drop an index or a constraint.
    /// An index is built online; adding a constraint validates the data
    /// first, holding the writer meanwhile (ADR 0019).
    fn commit_catalog(
        &self,
        namespace: &str,
        change: CatalogChange,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send;

    // ---- reads ----

    /// Wait until the namespace has applied `seq`, at most until the
    /// timeout of `options`; returns the namespace's seq then.
    fn wait_for_seq(
        &self,
        namespace: &str,
        seq: u64,
        options: QueryOptions,
    ) -> impl Future<Output = Result<u64, Error>> + Send;

    /// The nodes `ids` in the order asked, `None` for those that don't
    /// exist. At most `max_results` ids.
    fn get_nodes(
        &self,
        namespace: &str,
        ids: Vec<String>,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Node>>>, Error>> + Send;

    /// The edges `ids` in the order asked, `None` for those that don't
    /// exist. At most `max_results` ids.
    fn get_edges(
        &self,
        namespace: &str,
        ids: Vec<EdgeId>,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Edge>>>, Error>> + Send;

    /// Nodes matching a filter, by index or scan; paginated by id. See
    /// [`read::find`](crate::read::find).
    fn find(
        &self,
        namespace: &str,
        request: FindRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send;

    /// How `find` would read a filter: the index it uses, if any, and the
    /// estimated scan size. See [`read::explain`](crate::read::explain).
    fn explain(
        &self,
        namespace: &str,
        request: ExplainRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Explain>, Error>> + Send;

    /// The nodes within `depth` edges of seeds; paginated by id. See
    /// [`read::neighbourhood`](crate::read::neighbourhood).
    fn neighbourhood(
        &self,
        namespace: &str,
        request: NeighbourhoodRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send;

    /// A BFS or DFS along edges in a direction, in traversal order. See
    /// [`read::traverse`](crate::read::traverse).
    fn traverse(
        &self,
        namespace: &str,
        request: TraverseRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<String>>, Error>> + Send;

    /// A shortest path (BFS, Dijkstra or A*). See
    /// [`read::shortest_path`](crate::read::shortest_path).
    fn shortest_path(
        &self,
        namespace: &str,
        request: PathRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Option<Path>>, Error>> + Send;

    /// Random walks from a node. See
    /// [`read::random_walks`](crate::read::random_walks).
    fn random_walks(
        &self,
        namespace: &str,
        request: WalkRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Vec<String>>>, Error>> + Send;

    /// An induced subgraph around seeds. See
    /// [`read::subgraph`](crate::read::subgraph).
    fn subgraph(
        &self,
        namespace: &str,
        request: SubgraphRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Subgraph>, Error>> + Send;

    /// Every match of a pattern; paginated by row. See
    /// [`read::match_pattern`](crate::read::match_pattern).
    fn match_pattern(
        &self,
        namespace: &str,
        request: MatchRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<MatchRow>>, Error>> + Send;

    /// An analytics job on a projection (ADR 0022). The projection is
    /// collected under a short read lock, which needs `max_visited` of at
    /// least the number of nodes and `max_edges` of at least the number of
    /// edges; the job then runs without the lock, until it ends or the
    /// timeout. `max_results` keeps the top rows ([`JobResult`]), with
    /// `truncated` set if rows were cut.
    fn analyze(
        &self,
        namespace: &str,
        request: AnalyticsRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<JobResult>, Error>> + Send;

    // ---- catalog and namespaces ----

    /// The namespace's catalog: its indexes and constraints.
    fn catalog(
        &self,
        namespace: &str,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<NamespaceCatalog>, Error>> + Send;

    /// The namespace's state: seqs, counts, indexes (with builds in
    /// progress), memory.
    fn namespace_status(&self, namespace: &str) -> impl Future<Output = Result<NamespaceStatus, Error>> + Send;

    /// The live namespaces, by name.
    fn namespaces(&self) -> impl Future<Output = Result<Vec<NamespaceInfo>, Error>> + Send;

    /// Create a namespace. With a key, a retry returns the original event.
    /// Errors: `conflict` if it exists, `invalid_argument` for a bad name.
    fn create_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send;

    /// Drop a namespace and its data (archived first if the store
    /// archives). `default` can't be dropped. With a key, a retry returns
    /// the original event. Errors: `not_found`, `invalid_argument`.
    fn drop_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send;
}
