//! Every role × operation combination, over gRPC and REST (step 15's
//! criterion, step 15a), generated from one table: [`TABLE`] lists, per
//! operation, what each kind of caller gets. Each cell runs the operation
//! as that caller over both APIs and checks the outcome: allowed (the call
//! succeeds), `unauthenticated` or `permission_denied`.
//!
//! The callers: no credentials, a user without grants, users with `read`,
//! `write` and `admin` on the namespace under test, and a server-wide
//! admin. They authenticate with API tokens, which survive the password
//! changes some operations make.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use ironweaver_core::Expr;
use iwdb::{Embedded, QueryConfig, Store};
use iwdb_engine::catalog::{AttrPath, IndexDef};
use iwdb_engine::{CatalogChange, Mutation};
use iwdb_query::exec::block_on;
use iwdb_query::{
    Accounts, AnalyticsRequest, ChangesRequest, Code, CommitOptions, Database, Error, ExplainRequest, FindRequest, Job,
    MatchRequest, NeighbourhoodRequest, Order, PathRequest, ProjectionSpec, QueryOptions, Role, Secret,
    SubgraphRequest, TraverseRequest, UserInfo, WalkRequest,
};
use iwdb_server::client::Remote;
#[cfg(feature = "rest")]
use iwdb_server::client::RestRemote;
use iwdb_server::proto as pb;
use iwdb_server::proto::database_service_client::DatabaseServiceClient;
use support::{FAST, Running, auth_settings, options};

mod support;

/// The namespace the namespace roles are granted on.
const NS: &str = "t";

/// Who calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
    /// No credentials.
    Anonymous,
    /// A user without grants.
    NoGrant,
    Read,
    Write,
    /// `admin` on [`NS`].
    NsAdmin,
    /// The server-wide admin.
    Admin,
}

const CALLERS: [Who; 6] = [Who::Anonymous, Who::NoGrant, Who::Read, Who::Write, Who::NsAdmin, Who::Admin];

impl Who {
    fn user(self) -> &'static str {
        match self {
            Who::Anonymous => "nobody-at-all",
            Who::NoGrant => "plain",
            Who::Read => "reader",
            Who::Write => "writer",
            Who::NsAdmin => "nsadmin",
            Who::Admin => "root",
        }
    }

    fn password(self) -> String {
        format!("{}-password", self.user())
    }
}

/// What a call gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// It succeeds.
    A,
    /// `unauthenticated`.
    U,
    /// `permission_denied`.
    D,
}

use Outcome::{A, D, U};

/// Every authorised operation (and login), each a call the test can make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Commit,
    CommitCatalog,
    WaitForSeq,
    GetNodes,
    GetEdges,
    Find,
    Explain,
    Neighbourhood,
    Traverse,
    ShortestPath,
    RandomWalks,
    Subgraph,
    MatchPattern,
    Analyze,
    Changes,
    Watch,
    Catalog,
    NamespaceStatus,
    Namespaces,
    CreateNamespace,
    DropNamespace,
    Users,
    CreateUser,
    DeleteUser,
    SetAdmin,
    Grant,
    Revoke,
    SetOwnPassword,
    SetOthersPassword,
    CreateOwnToken,
    CreateOthersToken,
    RevokeOwnToken,
    RevokeOthersToken,
    OwnTokens,
    OthersTokens,
    WhoAmI,
    Logout,
    Login,
}

/// The table: per operation, the outcome for each caller of [`CALLERS`]
/// (no credentials, no grant, read, write, namespace admin, server admin).
const TABLE: &[(Op, [Outcome; 6])] = &[
    (Op::Commit, [U, D, D, A, A, A]),
    (Op::CommitCatalog, [U, D, D, D, A, A]),
    (Op::WaitForSeq, [U, D, A, A, A, A]),
    (Op::GetNodes, [U, D, A, A, A, A]),
    (Op::GetEdges, [U, D, A, A, A, A]),
    (Op::Find, [U, D, A, A, A, A]),
    (Op::Explain, [U, D, A, A, A, A]),
    (Op::Neighbourhood, [U, D, A, A, A, A]),
    (Op::Traverse, [U, D, A, A, A, A]),
    (Op::ShortestPath, [U, D, A, A, A, A]),
    (Op::RandomWalks, [U, D, A, A, A, A]),
    (Op::Subgraph, [U, D, A, A, A, A]),
    (Op::MatchPattern, [U, D, A, A, A, A]),
    (Op::Analyze, [U, D, A, A, A, A]),
    (Op::Changes, [U, D, A, A, A, A]),
    (Op::Watch, [U, D, A, A, A, A]),
    (Op::Catalog, [U, D, A, A, A, A]),
    (Op::NamespaceStatus, [U, D, A, A, A, A]),
    (Op::Namespaces, [U, A, A, A, A, A]),
    (Op::CreateNamespace, [U, D, D, D, D, A]),
    (Op::DropNamespace, [U, D, D, D, A, A]),
    (Op::Users, [U, D, D, D, D, A]),
    (Op::CreateUser, [U, D, D, D, D, A]),
    (Op::DeleteUser, [U, D, D, D, D, A]),
    (Op::SetAdmin, [U, D, D, D, D, A]),
    (Op::Grant, [U, D, D, D, D, A]),
    (Op::Revoke, [U, D, D, D, D, A]),
    (Op::SetOwnPassword, [U, A, A, A, A, A]),
    (Op::SetOthersPassword, [U, D, D, D, D, A]),
    (Op::CreateOwnToken, [U, A, A, A, A, A]),
    (Op::CreateOthersToken, [U, D, D, D, D, A]),
    (Op::RevokeOwnToken, [U, A, A, A, A, A]),
    (Op::RevokeOthersToken, [U, D, D, D, D, A]),
    (Op::OwnTokens, [U, A, A, A, A, A]),
    (Op::OthersTokens, [U, D, D, D, D, A]),
    (Op::WhoAmI, [U, A, A, A, A, A]),
    (Op::Logout, [U, A, A, A, A, A]),
    (Op::Login, [A, A, A, A, A, A]),
];

/// What the test needs of a client, over gRPC or REST.
trait Client: Database + Accounts {
    fn whoami(&self) -> Result<UserInfo, Error>;
    fn logout(&self) -> Result<(), Error>;
    fn login(&self, user: &str, password: &str) -> Result<(), Error>;
}

impl Client for Remote {
    fn whoami(&self) -> Result<UserInfo, Error> {
        block_on(Remote::whoami(self)).map(|(u, _)| u)
    }
    fn logout(&self) -> Result<(), Error> {
        // Logging out of an API token's "session" ends nothing; keep the token
        let token = self.token();
        let result = block_on(Remote::logout(self));
        self.set_token(token);
        result
    }
    fn login(&self, user: &str, password: &str) -> Result<(), Error> {
        let token = self.token();
        let result = block_on(Remote::login(self, user, Secret::new(password))).map(drop);
        self.set_token(token);
        result
    }
}

#[cfg(feature = "rest")]
impl Client for RestRemote {
    fn whoami(&self) -> Result<UserInfo, Error> {
        block_on(RestRemote::whoami(self)).map(|(u, _)| u)
    }
    fn logout(&self) -> Result<(), Error> {
        let token = self.token();
        let result = block_on(RestRemote::logout(self));
        self.set_token(token);
        result
    }
    fn login(&self, user: &str, password: &str) -> Result<(), Error> {
        let token = self.token();
        let result = block_on(RestRemote::login(self, user, Secret::new(password))).map(drop);
        self.set_token(token);
        result
    }
}

/// The server and what the cells share.
struct World {
    server: Running<Embedded>,
    _dir: tempfile::TempDir,
    /// The server admin, for setting cells up.
    admin: Remote,
    /// Each caller's API token.
    tokens: Vec<(Who, Option<Secret>)>,
    counter: AtomicUsize,
}

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["P".into()],
        attr: [("n".to_owned(), ironweaver_core::Value::Int(1))].into(),
        meta: Default::default(),
        expected_version: None,
    }
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), options()).unwrap();
        let users = store.users().with_params(FAST);
        for who in CALLERS.into_iter().filter(|w| *w != Who::Anonymous) {
            users.create(who.user(), &Secret::new(who.password()), who == Who::Admin).unwrap();
        }
        store.create_namespace(NS, None).unwrap();
        for (who, role) in [(Who::Read, Role::Read), (Who::Write, Role::Write), (Who::NsAdmin, Role::Admin)] {
            users.grant(who.user(), NS, role).unwrap();
        }
        store.namespace(NS).unwrap().commit(&[node("a"), node("b")]).unwrap();
        store
            .namespace(NS)
            .unwrap()
            .commit(&[Mutation::AddEdge {
                from: "a".into(),
                to: "b".into(),
                ty: Some("k".into()),
                attr: Default::default(),
                meta: Default::default(),
            }])
            .unwrap();
        let mut tokens = Vec::new();
        for who in CALLERS {
            let token = (who != Who::Anonymous).then(|| users.create_token(who.user(), "test", None).unwrap().token);
            tokens.push((who, token));
        }
        let db = Embedded::new(store, QueryConfig::default()).unwrap().with_auth(auth_settings());
        let server = Running::start_auth(db);
        let admin = server.client().with_token(tokens.last().unwrap().1.clone().unwrap());
        World { server, _dir: dir, admin, tokens, counter: AtomicUsize::new(0) }
    }

    fn token(&self, who: Who) -> Option<Secret> {
        self.tokens.iter().find(|(w, _)| *w == who).and_then(|(_, t)| t.clone())
    }

    fn grpc(&self, who: Who) -> Remote {
        let client = self.server.client();
        client.set_token(self.token(who));
        client
    }

    #[cfg(feature = "rest")]
    fn rest(&self, who: Who) -> RestRemote {
        let client = self.server.rest_client();
        client.set_token(self.token(who));
        client
    }

    fn unique(&self, prefix: &str) -> String {
        format!("{}{}", prefix, self.counter.fetch_add(1, Ordering::Relaxed))
    }

    /// A user for an operation on someone else, with a token named "victim".
    fn victim(&self) -> String {
        let name = self.unique("victim");
        block_on(self.admin.create_user(&name, Secret::new("victim-password"), false)).unwrap();
        block_on(self.admin.create_token(&name, "victim", None)).unwrap();
        name
    }

    /// Run `op` as `who` through `c`.
    fn run(&self, op: Op, who: Who, c: &impl Client, grpc: bool) -> Result<(), Error> {
        let o = QueryOptions::default;
        let me = who.user();
        match op {
            Op::Commit => block_on(c.commit(NS, vec![node("c")], CommitOptions::default())).map(drop),
            Op::CommitCatalog => {
                let path = AttrPath::new([self.unique("p")]).unwrap();
                block_on(c.commit_catalog(NS, CatalogChange::CreateIndex(IndexDef { path }), CommitOptions::default()))
                    .map(drop)
            }
            Op::WaitForSeq => block_on(c.wait_for_seq(NS, 1, o())).map(drop),
            Op::GetNodes => block_on(c.get_nodes(NS, vec!["a".into()], o())).map(drop),
            Op::GetEdges => block_on(c.get_edges(NS, vec![iwdb::EdgeId(0)], o())).map(drop),
            Op::Find => block_on(c.find(NS, FindRequest { filter: Expr::Label("P".into()) }, o())).map(drop),
            Op::Explain => {
                block_on(c.explain(NS, ExplainRequest { filter: Expr::Label("P".into()), analyze: false }, o()))
                    .map(drop)
            }
            Op::Neighbourhood => block_on(c.neighbourhood(NS, NeighbourhoodRequest::new(["a"], 1), o())).map(drop),
            Op::Traverse => block_on(c.traverse(NS, TraverseRequest::new("a", Order::Bfs), o())).map(drop),
            Op::ShortestPath => block_on(c.shortest_path(NS, PathRequest::bfs("a", "b"), o())).map(drop),
            Op::RandomWalks => block_on(c.random_walks(NS, WalkRequest::new("a", 2, 1), o())).map(drop),
            Op::Subgraph => block_on(c.subgraph(NS, SubgraphRequest::new(["a"], 1), o())).map(drop),
            Op::MatchPattern => block_on(c.match_pattern(NS, MatchRequest::parse("(x:P)").unwrap(), o())).map(drop),
            Op::Analyze => {
                let request =
                    AnalyticsRequest { projection: ProjectionSpec::default(), job: Job::Degree { incoming: false } };
                block_on(c.analyze(NS, request, o())).map(drop)
            }
            Op::Changes => block_on(c.changes(NS, ChangesRequest { from_seq: 1, wait: false }, o())).map(drop),
            Op::Watch => self.watch(who, grpc),
            Op::Catalog => block_on(c.catalog(NS, o())).map(drop),
            Op::NamespaceStatus => block_on(c.namespace_status(NS)).map(drop),
            Op::Namespaces => {
                let names: Vec<String> = block_on(c.namespaces())?.into_iter().map(|n| n.name.to_string()).collect();
                // Only those the caller has a role on
                let expected: &[&str] = match who {
                    Who::Admin => &["default", NS],
                    Who::NoGrant => &[],
                    _ => &[NS],
                };
                assert!(names.iter().map(String::as_str).eq(expected.iter().copied()), "{:?}: {:?}", who, names);
                Ok(())
            }
            Op::CreateNamespace => {
                let name = self.unique("new");
                block_on(c.create_namespace(&name, None)).map(drop).inspect(|()| {
                    block_on(self.admin.drop_namespace(&name, None)).unwrap();
                })
            }
            Op::DropNamespace => {
                // A namespace with the same grants as NS
                let name = self.unique("drop");
                block_on(self.admin.create_namespace(&name, None)).unwrap();
                for (who, role) in [(Who::Read, Role::Read), (Who::Write, Role::Write), (Who::NsAdmin, Role::Admin)] {
                    block_on(self.admin.grant(who.user(), &name, role)).unwrap();
                }
                block_on(c.drop_namespace(&name, None)).map(drop).inspect_err(|_| {
                    block_on(self.admin.drop_namespace(&name, None)).unwrap();
                })
            }
            Op::Users => block_on(c.users()).map(drop),
            Op::CreateUser => {
                block_on(c.create_user(&self.unique("new-user"), Secret::new("new-user-password"), false)).map(drop)
            }
            Op::DeleteUser => block_on(c.delete_user(&self.victim())),
            Op::SetAdmin => block_on(c.set_admin(&self.victim(), false)).map(drop),
            Op::Grant => block_on(c.grant(&self.victim(), NS, Role::Read)).map(drop),
            Op::Revoke => block_on(c.revoke(&self.victim(), NS)).map(drop),
            Op::SetOwnPassword => {
                let pw = Secret::new(who.password());
                block_on(c.set_password(me, pw.clone(), Some(pw)))
            }
            Op::SetOthersPassword => block_on(c.set_password(&self.victim(), Secret::new("another-password"), None)),
            Op::CreateOwnToken => block_on(c.create_token(me, &self.unique("own"), None)).map(drop),
            Op::CreateOthersToken => block_on(c.create_token(&self.victim(), "other", None)).map(drop),
            Op::RevokeOwnToken => {
                let name = self.unique("own");
                if who != Who::Anonymous {
                    block_on(self.admin.create_token(me, &name, None)).unwrap();
                }
                block_on(c.revoke_token(me, &name))
            }
            Op::RevokeOthersToken => block_on(c.revoke_token(&self.victim(), "victim")),
            Op::OwnTokens => block_on(c.tokens(me)).map(drop),
            Op::OthersTokens => block_on(c.tokens(&self.victim())).map(drop),
            Op::WhoAmI => {
                let user = c.whoami()?;
                assert_eq!(user.name, me);
                Ok(())
            }
            Op::Logout => c.logout(),
            Op::Login => c.login(Who::Read.user(), &Who::Read.password()),
        }
    }

    /// Follow the change stream (gRPC `Watch`, REST Server-Sent Events) up
    /// to its first batch.
    fn watch(&self, who: Who, grpc: bool) -> Result<(), Error> {
        if grpc {
            return self.watch_grpc(who);
        }
        #[cfg(feature = "rest")]
        return self.watch_rest(who);
        #[cfg(not(feature = "rest"))]
        unreachable!("no REST in this build")
    }

    fn watch_grpc(&self, who: Who) -> Result<(), Error> {
        let (token, endpoint) = (self.token(who), self.server.endpoint());
        self.server.block_on(async move {
            let channel = tonic::transport::Endpoint::from_str(&endpoint).unwrap().connect().await.unwrap();
            let mut client = DatabaseServiceClient::new(channel);
            let mut request =
                tonic::Request::new(pb::WatchRequest { namespace: NS.into(), from_seq: 1, options: None });
            if let Some(t) = token {
                request.metadata_mut().insert("authorization", format!("Bearer {}", t.expose()).parse().unwrap());
            }
            let from = |s: tonic::Status| iwdb_server::status::from_status(&s);
            let mut stream = client.watch(request).await.map_err(from)?.into_inner();
            stream.message().await.map_err(from)?;
            Ok(())
        })
    }

    #[cfg(feature = "rest")]
    fn watch_rest(&self, who: Who) -> Result<(), Error> {
        use http_body_util::BodyExt;
        let (token, endpoint) = (self.token(who), self.server.endpoint());
        self.server.block_on(async move {
            let http = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(hyper_util::client::legacy::connect::HttpConnector::new());
            let mut request =
                http::Request::get(format!("{}/v1/namespaces/{}/changes/stream?from_seq=1", endpoint, NS));
            if let Some(t) = token {
                request = request.header("authorization", format!("Bearer {}", t.expose()));
            }
            let body = http_body_util::Full::new(bytes::Bytes::new());
            let response = http.request(request.body(body).unwrap()).await.unwrap();
            let status = response.status();
            if status == http::StatusCode::OK {
                return Ok(());
            }
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let error: pb::Error = serde_json::from_slice(&body).unwrap();
            Err(iwdb_server::status::from_http(status, Some(&error.code), &error.message))
        })
    }
}

fn check(transport: &str, run: impl Fn(Op, Who) -> Result<(), Error>) -> Vec<String> {
    let mut failures = Vec::new();
    for (op, outcomes) in TABLE {
        for (who, expected) in CALLERS.into_iter().zip(outcomes) {
            let result = run(*op, who);
            let got = match &result {
                Ok(()) => A,
                Err(e) if e.code() == Code::Unauthenticated => U,
                Err(e) if e.code() == Code::PermissionDenied => D,
                Err(e) => {
                    failures.push(format!(
                        "{} {:?} as {:?}: unexpected error {} ({})",
                        transport,
                        op,
                        who,
                        e.code(),
                        e
                    ));
                    continue;
                }
            };
            if got != *expected {
                failures.push(format!(
                    "{} {:?} as {:?}: {:?}, expected {:?} ({:?})",
                    transport, op, who, got, expected, result
                ));
            }
        }
    }
    failures
}

#[test]
fn every_role_and_operation_over_grpc_and_rest() {
    // The table covers every operation the authorisation point knows
    assert_eq!(
        TABLE.iter().filter(|(op, _)| !matches!(op, Op::Login | Op::Watch)).count(),
        iwdb_query::Operation::ALL.len() + 4,
        "own and others' variants of SetPassword, CreateToken, RevokeToken, ListTokens"
    );
    let world = World::new();
    let grpc = check("gRPC", |op, who| world.run(op, who, &world.grpc(who), true));
    #[cfg(feature = "rest")]
    let rest = check("REST", |op, who| world.run(op, who, &world.rest(who), false));
    #[cfg(not(feature = "rest"))]
    let rest = Vec::new();
    let failures = [grpc, rest].concat();
    assert!(failures.is_empty(), "{} cells failed:\n{}", failures.len(), failures.join("\n"));
}

/// A non-admin changing its own password must give the current one.
#[test]
fn users_change_their_own_password_with_the_current_one() {
    let world = World::new();
    let reader = world.grpc(Who::Read);
    let e = block_on(reader.set_password("reader", Secret::new("a new password"), None)).unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let e = block_on(reader.set_password("reader", Secret::new("a new password"), Some(Secret::new("wrong!!!!"))))
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    block_on(reader.set_password("reader", Secret::new("a new password"), Some(Secret::new("reader-password"))))
        .unwrap();
}
