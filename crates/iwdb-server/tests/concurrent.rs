//! The conformance suite with concurrent clients: every case runs against
//! **one** server shared by the whole test binary, each through its own
//! `Remote` client, and the test harness runs the cases in parallel.
//!
//! A case expects a database with only `default`. Each client therefore
//! works in namespaces of its own: `Prefixed` (test code, not part of the
//! server) renames the namespaces a case names to `c<N>-<name>`, and back in
//! what it reads. Dropping `default` goes to the real `default`, which can't
//! be dropped, as the case expects.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use ironweaver_core::EdgeId;
use iwdb::{Embedded, QueryConfig, Store};
use iwdb_engine::catalog::{NamespaceCatalog, NamespaceName};
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, Mutation};
use iwdb_query::read::Explain;
use iwdb_query::{
    AnalyticsRequest, Answer, Changes, ChangesRequest, CommitOptions, Database, Edge, Error, ExplainRequest,
    FindRequest, JobResult, MatchRequest, MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Path, PathRequest,
    QueryOptions, Subgraph, SubgraphRequest, TraverseRequest, WalkRequest,
};
use iwdb_server::client::Remote;
use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};

mod support;

const DEFAULT: &str = "default";

/// The server all cases share, and its directory (kept until the process
/// exits).
fn server() -> &'static support::Running<Embedded> {
    static SERVER: OnceLock<(support::Running<Embedded>, tempfile::TempDir)> = OnceLock::new();
    &SERVER
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path(), support::options()).unwrap();
            (support::Running::start(Embedded::new(store, QueryConfig::default()).unwrap()), dir)
        })
        .0
}

/// A client whose namespaces are `<prefix><name>`.
struct Prefixed {
    remote: Remote,
    prefix: String,
}

/// A new client of the shared server, with its own `default` (boxed: the
/// fixture must dereference to the database).
fn client() -> Box<Prefixed> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let prefix = format!("c{}-", NEXT.fetch_add(1, Ordering::Relaxed));
    let remote = server().client();
    let own_default = format!("{}{}", prefix, DEFAULT);
    iwdb_query::exec::block_on(remote.create_namespace(&own_default, None)).unwrap();
    Box::new(Prefixed { remote, prefix })
}

impl Prefixed {
    fn name(&self, namespace: &str) -> String {
        format!("{}{}", self.prefix, namespace)
    }

    fn strip(&self, name: &str) -> String {
        name.strip_prefix(&self.prefix).unwrap_or(name).to_owned()
    }

    fn strip_result(&self, mut result: NamespaceResult) -> NamespaceResult {
        result.event.name = NamespaceName::new(self.strip(result.event.name.as_str())).unwrap();
        result
    }
}

impl Database for Prefixed {
    fn commit(
        &self,
        namespace: &str,
        mutations: Vec<Mutation>,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.commit(&name, mutations, options).await }
    }

    fn commit_catalog(
        &self,
        namespace: &str,
        change: CatalogChange,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.commit_catalog(&name, change, options).await }
    }

    fn wait_for_seq(
        &self,
        namespace: &str,
        seq: u64,
        options: QueryOptions,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.wait_for_seq(&name, seq, options).await }
    }

    fn changes(
        &self,
        namespace: &str,
        request: ChangesRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Changes>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.changes(&name, request, options).await }
    }

    fn get_nodes(
        &self,
        namespace: &str,
        ids: Vec<String>,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Node>>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.get_nodes(&name, ids, options).await }
    }

    fn get_edges(
        &self,
        namespace: &str,
        ids: Vec<EdgeId>,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Option<Edge>>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.get_edges(&name, ids, options).await }
    }

    fn find(
        &self,
        namespace: &str,
        request: FindRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.find(&name, request, options).await }
    }

    fn explain(
        &self,
        namespace: &str,
        request: ExplainRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Explain>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.explain(&name, request, options).await }
    }

    fn neighbourhood(
        &self,
        namespace: &str,
        request: NeighbourhoodRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Node>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.neighbourhood(&name, request, options).await }
    }

    fn traverse(
        &self,
        namespace: &str,
        request: TraverseRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<String>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.traverse(&name, request, options).await }
    }

    fn shortest_path(
        &self,
        namespace: &str,
        request: PathRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Option<Path>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.shortest_path(&name, request, options).await }
    }

    fn random_walks(
        &self,
        namespace: &str,
        request: WalkRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<Vec<String>>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.random_walks(&name, request, options).await }
    }

    fn subgraph(
        &self,
        namespace: &str,
        request: SubgraphRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Subgraph>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.subgraph(&name, request, options).await }
    }

    fn match_pattern(
        &self,
        namespace: &str,
        request: MatchRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<Vec<MatchRow>>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.match_pattern(&name, request, options).await }
    }

    fn analyze(
        &self,
        namespace: &str,
        request: AnalyticsRequest,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<JobResult>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.analyze(&name, request, options).await }
    }

    fn catalog(
        &self,
        namespace: &str,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<NamespaceCatalog>, Error>> + Send {
        let name = self.name(namespace);
        async move { self.remote.catalog(&name, options).await }
    }

    fn namespace_status(&self, namespace: &str) -> impl Future<Output = Result<NamespaceStatus, Error>> + Send {
        let name = self.name(namespace);
        async move {
            let mut status = self.remote.namespace_status(&name).await?;
            status.name = self.strip(&status.name);
            Ok(status)
        }
    }

    fn namespaces(&self) -> impl Future<Output = Result<Vec<NamespaceInfo>, Error>> + Send {
        let all = self.remote.namespaces();
        async move {
            let mine = all.await?.into_iter().filter(|n| n.name.as_str().starts_with(&self.prefix));
            Ok(mine
                .map(|mut n| {
                    n.name = NamespaceName::new(self.strip(n.name.as_str())).unwrap();
                    n
                })
                .collect())
        }
    }

    fn create_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let name = self.name(name);
        async move { self.remote.create_namespace(&name, key).await.map(|r| self.strip_result(r)) }
    }

    fn drop_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        // `default` is never dropped; the server says so for the real one
        let target = if name == DEFAULT { DEFAULT.to_owned() } else { self.name(name) };
        async move { self.remote.drop_namespace(&target, key).await.map(|r| self.strip_result(r)) }
    }
}

iwdb_query::conformance_tests!(client());
