//! [`Embedded`]: the [`Database`] trait on an embedded [`Store`] (ADR 0020).
//!
//! Translation only (design rule 8): the read operations are
//! `iwdb_query::read`, the commit pipeline is the store's. This module runs
//! them on a worker pool, under the namespace's read lock and the store's
//! deadline timer, and maps the errors.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironweaver_core::cancel::Token;
use ironweaver_core::EdgeId;
use iwdb_engine::catalog::NamespaceCatalog;
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, Mutation, Namespace};
use iwdb_query::exec::{Pending, Pool};
use iwdb_query::read::{self, ReadContext};
use iwdb_query::{
    AnalyticsRequest, Answer, Code, CommitOptions, Database, Edge, Error, Explain, ExplainRequest, FindRequest,
    JobResult, LimitConfig, MatchRequest, MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Path, PathRequest,
    QueryOptions, Subgraph, SubgraphRequest, TraverseRequest, WalkRequest, Work,
};
use iwdb_storage::io::{LogFs, StdFs};
use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};

use crate::{Ns, ReadOptions, Store};

/// How an [`Embedded`] database runs requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryConfig {
    /// Default and maximum limits and timeouts of reads.
    pub limits: LimitConfig,
    /// Worker threads that run requests (reads, commits, admin). Default:
    /// the available parallelism, at least 2.
    pub workers: usize,
    /// Requests that may wait for a worker; more fail with `unavailable`.
    /// Default: 1024.
    pub queue: usize,
}

impl Default for QueryConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
        QueryConfig { limits: LimitConfig::default(), workers: cpus.max(2), queue: 1024 }
    }
}

/// The embedded store as a [`Database`]: what the Python bindings use, and
/// what the servers of steps 11 and 12 serve.
///
/// Every request runs on one of the config's worker threads, so the async
/// methods never block their caller's executor. A read waits for its
/// `min_seq`, then runs under the namespace's read lock and a cancel token
/// that the store's timer cancels at the deadline (the timeout counts from
/// the call, queueing included: a read still waiting for a worker at its
/// deadline fails then) and that dropping the future cancels too (ADR
/// 0020). Commits run on the workers as well, without a timeout.
pub struct Embedded<F: LogFs + Send + Sync + 'static = StdFs>
where
    F::File: Send,
{
    store: Arc<Store<F>>,
    pool: Pool,
    config: QueryConfig,
}

impl<F: LogFs + Send + Sync + 'static> std::fmt::Debug for Embedded<F>
where
    F::File: Send,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedded").field("store", &self.store).field("config", &self.config).finish()
    }
}

impl<F: LogFs + Clone + Send + Sync + 'static> Embedded<F>
where
    F::File: Send,
{
    /// Serve `store` with `config`. Errors: `invalid_argument` for invalid
    /// limits; `internal` if the worker threads can't start.
    pub fn new(store: Store<F>, config: QueryConfig) -> Result<Self, Error> {
        config.limits.check()?;
        let pool = Pool::new("iwdb-query", config.workers, config.queue)?;
        Ok(Embedded { store: Arc::new(store), pool, config })
    }

    /// The store, for what the trait doesn't cover: backups, checkpoints,
    /// syncs, the store's status.
    pub fn store(&self) -> &Store<F> {
        &self.store
    }

    pub fn config(&self) -> &QueryConfig {
        &self.config
    }

    /// Finish the queued requests, stop the workers, and close the store
    /// ([`Store::close`]).
    pub fn close(self) -> Result<(), crate::Error> {
        let Embedded { store, pool, .. } = self;
        pool.shutdown();
        drop(pool);
        match Arc::try_unwrap(store) {
            Ok(store) => store.close(),
            // Unreachable: only the workers clone the store, and they have
            // stopped
            Err(_) => Err(crate::Error::InvalidOptions("the store is still in use".into())),
        }
    }

    /// Run `f` on a worker with the store.
    fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store<F>, &Token) -> Result<T, Error> + Send + 'static,
    ) -> Pending<Result<T, Error>> {
        let store = self.store.clone();
        self.pool.submit(move |token| f(&store, token))
    }

    /// Run the request `f` on a worker; at the request's deadline it fails
    /// with `timeout`, even if it is still waiting for a worker.
    fn run_until<T: Send + 'static>(
        &self,
        deadline: Option<(Instant, Error)>,
        f: impl FnOnce(&Store<F>, &Token) -> Result<T, Error> + Send + 'static,
    ) -> Pending<Result<T, Error>> {
        let store = self.store.clone();
        self.pool.submit_until(deadline, move |token| f(&store, token))
    }

    /// Run the read `f` on namespace `namespace` (see the type docs).
    fn read<T: Send + 'static>(
        &self,
        namespace: &str,
        options: QueryOptions,
        f: impl FnOnce(&Ns<'_, F>, &Namespace, &ReadContext) -> Result<T, Error> + Send + 'static,
    ) -> Pending<Result<T, Error>> {
        let request = match Request::new(&self.config, options) {
            Ok(request) => request,
            Err(e) => return Pending::ready(Err(e)),
        };
        let name = namespace.to_owned();
        self.run_until(request.expiry(), move |store, token| {
            let ns = store.namespace(&name)?;
            let cx = ReadContext::new(request.bounds, &request.options, ns.id(), store.history());
            let read = request.read_options(token)?;
            match ns.read_with(&read, |n| f(&ns, n, &cx)) {
                Ok(result) => result,
                Err(e) => Err(request.error(e)),
            }
        })
    }
}

/// A read's resolved limits and deadline.
struct Request {
    options: QueryOptions,
    bounds: iwdb_query::Bounds,
    timeout: Duration,
    deadline: Option<Instant>,
}

impl Request {
    fn new(config: &QueryConfig, options: QueryOptions) -> Result<Self, Error> {
        let (bounds, timeout) = config.limits.resolve(&options)?;
        Ok(Request { options, bounds, timeout, deadline: Instant::now().checked_add(timeout) })
    }

    /// The store's read options for what is left of the timeout; a
    /// `timeout` error if nothing is.
    fn read_options(&self, token: &Token) -> Result<ReadOptions, Error> {
        let left = self.deadline.map_or(Duration::MAX, |d| d.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            return Err(self.timed_out("the request"));
        }
        Ok(ReadOptions {
            min_seq: self.options.min_seq,
            history: self.options.history,
            timeout: Some(left),
            cancel: Some(token.clone()),
        })
    }

    /// The deadline and the error the request fails with at it, for the
    /// pool (`None`: no deadline).
    fn expiry(&self) -> Option<(Instant, Error)> {
        self.deadline.map(|at| (at, self.timed_out("the request")))
    }

    fn timed_out(&self, what: &str) -> Error {
        Error::new(Code::Timeout, format!("{} timed out after {:?}", what, self.timeout))
    }

    /// A store error, with the request's whole timeout in a timeout's
    /// message (the store only knew what was left of it).
    fn error(&self, e: crate::Error) -> Error {
        match e {
            crate::Error::Timeout { what, .. } => self.timed_out(&what),
            other => other.into(),
        }
    }
}

fn key_options(key: Option<IdempotencyKey>) -> CommitOptions {
    CommitOptions { idempotency_key: key }
}

impl<F: LogFs + Clone + Send + Sync + 'static> Database for Embedded<F>
where
    F::File: Send,
{
    fn commit(
        &self,
        namespace: &str,
        mutations: Vec<Mutation>,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let name = namespace.to_owned();
        self.run(move |store, _| Ok(store.namespace(&name)?.commit_with(&mutations, &options)?))
    }

    fn commit_catalog(
        &self,
        namespace: &str,
        change: CatalogChange,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let name = namespace.to_owned();
        self.run(move |store, _| Ok(store.namespace(&name)?.commit_catalog_with(change, &options)?))
    }

    fn wait_for_seq(
        &self,
        namespace: &str,
        seq: u64,
        options: QueryOptions,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        let request = match Request::new(&self.config, options) {
            Ok(request) => request,
            Err(e) => return Pending::ready(Err(e)),
        };
        let name = namespace.to_owned();
        self.run_until(request.expiry(), move |store, token| {
            let ns = store.namespace(&name)?;
            ns.wait_for_seq(seq, &request.read_options(token)?).map_err(|e| request.error(e))
        })
    }

    fn get_nodes(
        &self,
        namespace: &str,
        ids: Vec<String>,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Node>>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::get_nodes(ns, &ids, cx))
    }

    fn get_edges(
        &self,
        namespace: &str,
        ids: Vec<EdgeId>,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Edge>>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::get_edges(ns, &ids, cx))
    }

    fn find(
        &self,
        namespace: &str,
        request: FindRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::find(ns, &request, cx))
    }

    fn explain(
        &self,
        namespace: &str,
        request: ExplainRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Explain>, Error>> + Send {
        self.read(namespace, options, move |handle, ns, cx| {
            // The builds are the live namespace's, not the graph's
            let building: Vec<Vec<String>> = handle.index_builds().iter().map(|p| p.keys().to_vec()).collect();
            read::explain(ns, &request, &building, cx)
        })
    }

    fn neighbourhood(
        &self,
        namespace: &str,
        request: NeighbourhoodRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::neighbourhood(ns, &request, cx))
    }

    fn traverse(
        &self,
        namespace: &str,
        request: TraverseRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<String>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::traverse(ns, &request, cx))
    }

    fn shortest_path(
        &self,
        namespace: &str,
        request: PathRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Option<Path>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::shortest_path(ns, &request, cx))
    }

    fn random_walks(
        &self,
        namespace: &str,
        request: WalkRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Vec<String>>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::random_walks(ns, &request, cx))
    }

    fn subgraph(
        &self,
        namespace: &str,
        request: SubgraphRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Subgraph>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::subgraph(ns, &request, cx))
    }

    fn match_pattern(
        &self,
        namespace: &str,
        request: MatchRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<MatchRow>>, Error>> + Send {
        self.read(namespace, options, move |_, ns, cx| read::match_pattern(ns, &request, cx))
    }

    fn analyze(
        &self,
        namespace: &str,
        request: AnalyticsRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<JobResult>, Error>> + Send {
        let resolved = match Request::new(&self.config, options) {
            Ok(resolved) => resolved,
            Err(e) => return Pending::ready(Err(e)),
        };
        let name = namespace.to_owned();
        self.run_until(resolved.expiry(), move |store, token| {
            let ns = store.namespace(&name)?;
            let bounds = resolved.bounds;
            let (nodes, edges) = ns.read(|n| (n.graph().node_count(), n.graph().edge_count()));
            if nodes > bounds.max_visited || edges > bounds.max_edges {
                return Err(Error::budget(format!(
                    "{} nodes visited and {} edges examined (the projection holds {} nodes and {} edges)",
                    bounds.max_visited, bounds.max_edges, nodes, edges
                )));
            }
            let job = &request.job;
            let analysis = ns
                .analyze(&request.projection, &resolved.read_options(token)?, |p| {
                    read::run_job(job, p, bounds.max_results)
                })
                .map_err(|e| resolved.error(e))?;
            let (value, truncated) = analysis.value;
            Ok(Answer { value, seq: analysis.seq, next: None, truncated, work: Work { visited: nodes, edges } })
        })
    }

    fn catalog(
        &self,
        namespace: &str,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<NamespaceCatalog>, Error>> + Send {
        self.read(namespace, options, |_, ns, _| Ok(Answer::at(ns.seq(), ns.catalog().clone())))
    }

    fn namespace_status(&self, namespace: &str) -> impl Future<Output = Result<NamespaceStatus, Error>> + Send {
        let name = namespace.to_owned();
        self.run(move |store, _| Ok(store.namespace(&name)?.status()))
    }

    fn namespaces(&self) -> impl Future<Output = Result<Vec<NamespaceInfo>, Error>> + Send {
        self.run(|store, _| Ok(store.namespaces()))
    }

    fn create_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let name = name.to_owned();
        self.run(move |store, _| Ok(store.create_namespace(&name, key_options(key).idempotency_key.as_ref())?))
    }

    fn drop_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let name = name.to_owned();
        self.run(move |store, _| Ok(store.drop_namespace(&name, key_options(key).idempotency_key.as_ref())?))
    }
}
