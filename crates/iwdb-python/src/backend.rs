//! What serves an `iwdb.Store` (ADR 0035): the embedded store
//! (`iwdb::Embedded`) or a server over gRPC (`iwdb_server::client::Remote`).
//! [`Backend`] implements the `Database` trait by delegating to either, so
//! the bindings call one trait for both (design rule 8).
//!
//! The remote side adds read-your-writes across the network: it remembers
//! the seq of its last commit per namespace and sends it as `min_seq` when
//! a read gives none.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use ironweaver_core::EdgeId;
use iwdb::Embedded;
use iwdb_engine::catalog::NamespaceCatalog;
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, Mutation};
use iwdb_query::read::Explain;
use iwdb_query::{
    AnalyticsRequest, Answer, Changes, ChangesRequest, CommitOptions, Database, Edge, Error, ExplainRequest,
    FindRequest, JobResult, MatchRequest, MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Path, PathRequest,
    QueryOptions, Schema, Subgraph, SubgraphRequest, TraverseRequest, WalkRequest,
};
use iwdb_server::client::Remote;
use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};

/// A store's backend.
pub enum Backend {
    Embedded(Embedded),
    /// Boxed: a client holds a channel per service.
    Remote(Box<RemoteDb>),
}

/// A server's database, with the seq of this client's last commit per
/// namespace name.
pub struct RemoteDb {
    remote: Remote,
    seqs: Mutex<HashMap<String, u64>>,
}

impl RemoteDb {
    pub fn new(remote: Remote) -> Self {
        RemoteDb { remote, seqs: Mutex::new(HashMap::new()) }
    }

    fn seqs(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.seqs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `options`, waiting for this client's last commit to `namespace` if
    /// they don't name a seq.
    fn read(&self, namespace: &str, mut options: QueryOptions) -> QueryOptions {
        if options.min_seq.is_none() {
            options.min_seq = self.seqs().get(namespace).copied();
        }
        options
    }

    /// Remember a commit's seq (the highest one seen per namespace).
    fn committed(&self, namespace: &str, result: Result<CommitResult, Error>) -> Result<CommitResult, Error> {
        if let Ok(r) = &result {
            let mut seqs = self.seqs();
            let seq = seqs.entry(namespace.to_owned()).or_insert(0);
            *seq = (*seq).max(r.seq);
        }
        result
    }

    /// Forget the seq of a namespace that was created or dropped: a new
    /// namespace under the name starts its seqs again.
    fn forget(&self, name: &str) {
        self.seqs().remove(name);
    }
}

/// A read: delegated, with this client's last seq on the remote side.
macro_rules! read {
    ($self:ident, $method:ident, $ns:ident, $request:ident, $options:ident) => {
        match $self {
            Backend::Embedded(db) => db.$method($ns, $request, $options).await,
            Backend::Remote(db) => db.remote.$method($ns, $request, db.read($ns, $options)).await,
        }
    };
}

/// A call without read-your-writes: delegated as it is.
macro_rules! plain {
    ($self:ident, $method:ident ( $($arg:ident),* )) => {
        match $self {
            Backend::Embedded(db) => db.$method($($arg),*).await,
            Backend::Remote(db) => db.remote.$method($($arg),*).await,
        }
    };
}

impl Database for Backend {
    async fn commit(
        &self,
        namespace: &str,
        mutations: Vec<Mutation>,
        options: CommitOptions,
    ) -> Result<CommitResult, Error> {
        match self {
            Backend::Embedded(db) => db.commit(namespace, mutations, options).await,
            Backend::Remote(db) => db.committed(namespace, db.remote.commit(namespace, mutations, options).await),
        }
    }

    async fn commit_catalog(
        &self,
        namespace: &str,
        change: CatalogChange,
        options: CommitOptions,
    ) -> Result<CommitResult, Error> {
        match self {
            Backend::Embedded(db) => db.commit_catalog(namespace, change, options).await,
            Backend::Remote(db) => db.committed(namespace, db.remote.commit_catalog(namespace, change, options).await),
        }
    }

    async fn wait_for_seq(&self, namespace: &str, seq: u64, options: QueryOptions) -> Result<u64, Error> {
        plain!(self, wait_for_seq(namespace, seq, options))
    }

    async fn get_nodes(
        &self,
        namespace: &str,
        ids: Vec<String>,
        options: QueryOptions,
    ) -> Result<Answer<Vec<Option<Node>>>, Error> {
        read!(self, get_nodes, namespace, ids, options)
    }

    async fn get_edges(
        &self,
        namespace: &str,
        ids: Vec<EdgeId>,
        options: QueryOptions,
    ) -> Result<Answer<Vec<Option<Edge>>>, Error> {
        read!(self, get_edges, namespace, ids, options)
    }

    async fn find(
        &self,
        namespace: &str,
        request: FindRequest,
        options: QueryOptions,
    ) -> Result<Answer<Vec<Node>>, Error> {
        read!(self, find, namespace, request, options)
    }

    async fn explain(
        &self,
        namespace: &str,
        request: ExplainRequest,
        options: QueryOptions,
    ) -> Result<Answer<Explain>, Error> {
        read!(self, explain, namespace, request, options)
    }

    async fn neighbourhood(
        &self,
        namespace: &str,
        request: NeighbourhoodRequest,
        options: QueryOptions,
    ) -> Result<Answer<Vec<Node>>, Error> {
        read!(self, neighbourhood, namespace, request, options)
    }

    async fn traverse(
        &self,
        namespace: &str,
        request: TraverseRequest,
        options: QueryOptions,
    ) -> Result<Answer<Vec<String>>, Error> {
        read!(self, traverse, namespace, request, options)
    }

    async fn shortest_path(
        &self,
        namespace: &str,
        request: PathRequest,
        options: QueryOptions,
    ) -> Result<Answer<Option<Path>>, Error> {
        read!(self, shortest_path, namespace, request, options)
    }

    async fn random_walks(
        &self,
        namespace: &str,
        request: WalkRequest,
        options: QueryOptions,
    ) -> Result<Answer<Vec<Vec<String>>>, Error> {
        read!(self, random_walks, namespace, request, options)
    }

    async fn subgraph(
        &self,
        namespace: &str,
        request: SubgraphRequest,
        options: QueryOptions,
    ) -> Result<Answer<Subgraph>, Error> {
        read!(self, subgraph, namespace, request, options)
    }

    async fn match_pattern(
        &self,
        namespace: &str,
        request: MatchRequest,
        options: QueryOptions,
    ) -> Result<Answer<Vec<MatchRow>>, Error> {
        read!(self, match_pattern, namespace, request, options)
    }

    async fn analyze(
        &self,
        namespace: &str,
        request: AnalyticsRequest,
        options: QueryOptions,
    ) -> Result<Answer<JobResult>, Error> {
        read!(self, analyze, namespace, request, options)
    }

    async fn changes(
        &self,
        namespace: &str,
        request: ChangesRequest,
        options: QueryOptions,
    ) -> Result<Answer<Changes>, Error> {
        plain!(self, changes(namespace, request, options))
    }

    async fn catalog(&self, namespace: &str, options: QueryOptions) -> Result<Answer<NamespaceCatalog>, Error> {
        match self {
            Backend::Embedded(db) => db.catalog(namespace, options).await,
            Backend::Remote(db) => db.remote.catalog(namespace, db.read(namespace, options)).await,
        }
    }

    async fn schema(&self, namespace: &str, options: QueryOptions) -> Result<Answer<Schema>, Error> {
        match self {
            Backend::Embedded(db) => db.schema(namespace, options).await,
            Backend::Remote(db) => db.remote.schema(namespace, db.read(namespace, options)).await,
        }
    }

    async fn namespace_status(&self, namespace: &str) -> Result<NamespaceStatus, Error> {
        plain!(self, namespace_status(namespace))
    }

    async fn namespaces(&self) -> Result<Vec<NamespaceInfo>, Error> {
        plain!(self, namespaces())
    }

    async fn create_namespace(&self, name: &str, key: Option<IdempotencyKey>) -> Result<NamespaceResult, Error> {
        match self {
            Backend::Embedded(db) => db.create_namespace(name, key).await,
            Backend::Remote(db) => {
                let result = db.remote.create_namespace(name, key).await;
                if result.is_ok() {
                    db.forget(name);
                }
                result
            }
        }
    }

    async fn drop_namespace(&self, name: &str, key: Option<IdempotencyKey>) -> Result<NamespaceResult, Error> {
        match self {
            Backend::Embedded(db) => db.drop_namespace(name, key).await,
            Backend::Remote(db) => {
                let result = db.remote.drop_namespace(name, key).await;
                if result.is_ok() {
                    db.forget(name);
                }
                result
            }
        }
    }
}
