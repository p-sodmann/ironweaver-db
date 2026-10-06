//! The memory limit over the wire (step 16d, ADR 0054), the same case
//! through the gRPC and the REST client: with the store above its refusal
//! line, every write that adds fails with `resource_exhausted`, while
//! logins (the system namespace), deletes, drops and reads go on, and the
//! status and metrics report the limit and the state.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use iwdb::{CatalogChange, Embedded, IndexDef, MemoryOptions, QueryConfig, Store, StoreOptions};
use iwdb_engine::Mutation;
use iwdb_engine::catalog::AttrPath;
use iwdb_query::exec::block_on;
use iwdb_query::metrics::{MEMORY_LIMIT, MEMORY_STATE, Value as Metric};
use iwdb_query::{Admin, Code, CommitOptions, Database, LimitSource, MemoryState, QueryOptions, Secret};
use iwdb_server::auth::AuthMode;
use support::{ADMIN, FAST, Running, auth_settings, options};

const NS: &str = "default";

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    }
}

/// A server whose store refuses writes from the start: a limit of 1 byte.
/// Its namespace `doomed` and node `old` were written without a limit.
fn full() -> (Running<Embedded>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path(), options()).unwrap();
        store.create_namespace("doomed", None).unwrap();
        store.commit(&[node("old")]).unwrap();
        store.close().unwrap();
    }
    let memory = MemoryOptions { limit_bytes: Some(1), ..MemoryOptions::default() };
    let store = Store::open(dir.path(), StoreOptions { memory, ..options() }).unwrap();
    // The system namespace isn't limited: users and logins work
    store.users().with_params(FAST).create(ADMIN.0, &Secret::new(ADMIN.1), true).unwrap();
    let db = Embedded::new(store, QueryConfig::default()).unwrap().with_auth(auth_settings());
    (Running::start_tls(db, AuthMode { enabled: true }, None), dir)
}

async fn writes_are_refused_and_the_rest_goes_on<D: Database + Admin>(db: &D) {
    let options = CommitOptions::default;
    fn code<T: std::fmt::Debug>(r: Result<T, iwdb_query::Error>) -> Code {
        r.unwrap_err().code()
    }
    assert_eq!(code(db.commit(NS, vec![node("new")], options()).await), Code::ResourceExhausted);
    let index = CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["k"]).unwrap() });
    assert_eq!(code(db.commit_catalog(NS, index, options()).await), Code::ResourceExhausted);
    assert_eq!(code(db.create_namespace("new", None).await), Code::ResourceExhausted);
    let error = db.commit(NS, vec![node("new")], options()).await.unwrap_err();
    assert!(error.message().contains("deletes and drops are accepted"), "{}", error);

    // Reads, deletes and drops
    let nodes = db.get_nodes(NS, vec!["old".into()], QueryOptions::default()).await.unwrap();
    assert!(nodes.value[0].is_some());
    let delete = Mutation::DeleteNode { id: "old".into(), expected_version: None };
    db.commit(NS, vec![delete], options()).await.unwrap();
    db.drop_namespace("doomed", None).await.unwrap();

    let memory = db.server_status().await.unwrap().memory;
    assert_eq!(memory.state, MemoryState::RefusingWrites);
    assert_eq!((memory.limit_bytes, memory.limit_source), (Some(1), Some(LimitSource::Config)));
    assert_eq!((memory.warn_bytes, memory.refuse_writes_bytes), (Some(0), Some(0)));
    assert!(memory.used_bytes > 0);
    let metrics = db.metrics().await.unwrap();
    let gauge = |name: &str| {
        let family = metrics.families.iter().find(|f| f.name == name).unwrap();
        match family.samples[0].value {
            Metric::Gauge(v) => v,
            ref other => panic!("{:?}", other),
        }
    };
    assert_eq!((gauge(MEMORY_STATE), gauge(MEMORY_LIMIT)), (2.0, 1.0));
}

#[test]
fn over_grpc() {
    let (server, _dir) = full();
    let remote = server.client();
    block_on(remote.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap();
    block_on(writes_are_refused_and_the_rest_goes_on(&remote));
}

#[cfg(feature = "rest")]
#[test]
fn over_rest() {
    let (server, _dir) = full();
    let remote = server.rest_client();
    block_on(remote.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap();
    block_on(writes_are_refused_and_the_rest_goes_on(&remote));
}
