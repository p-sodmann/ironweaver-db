//! Every role × operation combination, over gRPC and REST (step 15's
//! criterion, step 15a), generated from one table: [`TABLE`] lists, per
//! operation, what each kind of caller gets. Each cell runs the operation
//! as that caller over both APIs and checks the outcome: allowed (the call
//! succeeds), `unauthenticated` or `permission_denied`.
//!
//! Each cell also checks the audit log (step 15c, ADR 0049): a refusal
//! leaves one entry with the caller, the code and the client's address;
//! an allowed call of an operation the table marks `Audited::Always`
//! leaves one success entry; any other call none. No entry holds a
//! password or token.
//!
//! The operator's reads and cancel (step 16c) are in the table too: who
//! sees what of the status, the requests, the readers and the metrics is
//! checked in their cells, and cancelling, which is always audited, names
//! the request and its owner. So are the admin writes (step 16e): a server
//! admin's only, always audited, with the namespace or backup they name.
//!
//! The callers: no credentials, a user without grants, users with `read`,
//! `write` and `admin` on the namespace under test, and a server-wide
//! admin. They authenticate with API tokens, which survive the password
//! changes some operations make.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ironweaver_core::Expr;
use ironweaver_core::algo::PageRank;
use iwdb::{Embedded, QueryConfig, Store};
use iwdb_engine::catalog::{AttrPath, IndexDef};
use iwdb_engine::{CatalogChange, Mutation};
use iwdb_query::audit::AuditEntry;
use iwdb_query::exec::block_on;
use iwdb_query::{
    Accounts, AnalyticsRequest, ChangesRequest, Code, CommitOptions, Database, Error, ExplainRequest, FindRequest, Job,
    JobState, MatchRequest, NeighbourhoodRequest, Order, PathRequest, ProjectionSpec, QueryOptions, Role, Secret,
    SubgraphRequest, TraverseRequest, UserInfo, WalkRequest,
};
use iwdb_query::{Admin, Audited, Operation, Via};
use iwdb_server::auth::AuthMode;
use iwdb_server::client::Remote;
#[cfg(feature = "rest")]
use iwdb_server::client::RestRemote;
use iwdb_server::proto as pb;
use iwdb_server::proto::database_service_client::DatabaseServiceClient;
use support::{Captured, FAST, Running, auth_settings, options};

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
    /// `not_found`: allowed, but there is nothing the caller may act on
    /// (cancelling a request that isn't its own).
    N,
}

use Outcome::{A, D, N, U};

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
    Schema,
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
    ServerStatus,
    ActiveRequests,
    CancelOwnRequest,
    CancelOthersRequest,
    Consumers,
    Metrics,
    Log,
    Checkpoint,
    Backup,
    Verify,
    PruneArchive,
    StartJob,
    ListJobs,
    GetOwnJob,
    GetOthersJob,
    CancelOwnJob,
    CancelOthersJob,
    OwnJobResult,
    OthersJobResult,
}

impl Op {
    /// The operation of the authorisation point's table this call runs.
    fn operation(self) -> Operation {
        match self {
            Op::Commit => Operation::Commit,
            Op::CommitCatalog => Operation::CommitCatalog,
            Op::WaitForSeq => Operation::WaitForSeq,
            Op::GetNodes => Operation::GetNodes,
            Op::GetEdges => Operation::GetEdges,
            Op::Find => Operation::Find,
            Op::Explain => Operation::Explain,
            Op::Neighbourhood => Operation::Neighbourhood,
            Op::Traverse => Operation::Traverse,
            Op::ShortestPath => Operation::ShortestPath,
            Op::RandomWalks => Operation::RandomWalks,
            Op::Subgraph => Operation::Subgraph,
            Op::MatchPattern => Operation::MatchPattern,
            Op::Analyze => Operation::Analyze,
            Op::Changes | Op::Watch => Operation::Changes,
            Op::Catalog => Operation::Catalog,
            Op::Schema => Operation::Schema,
            Op::NamespaceStatus => Operation::NamespaceStatus,
            Op::Namespaces => Operation::Namespaces,
            Op::CreateNamespace => Operation::CreateNamespace,
            Op::DropNamespace => Operation::DropNamespace,
            Op::Users => Operation::Users,
            Op::CreateUser => Operation::CreateUser,
            Op::DeleteUser => Operation::DeleteUser,
            Op::SetAdmin => Operation::SetAdmin,
            Op::Grant => Operation::Grant,
            Op::Revoke => Operation::Revoke,
            Op::SetOwnPassword | Op::SetOthersPassword => Operation::SetPassword,
            Op::CreateOwnToken | Op::CreateOthersToken => Operation::CreateToken,
            Op::RevokeOwnToken | Op::RevokeOthersToken => Operation::RevokeToken,
            Op::OwnTokens | Op::OthersTokens => Operation::Tokens,
            Op::WhoAmI => Operation::WhoAmI,
            Op::Logout => Operation::Logout,
            Op::Login => Operation::Login,
            Op::ServerStatus => Operation::ServerStatus,
            Op::ActiveRequests => Operation::ActiveRequests,
            Op::CancelOwnRequest | Op::CancelOthersRequest => Operation::CancelRequest,
            Op::Consumers => Operation::Consumers,
            Op::Metrics => Operation::Metrics,
            Op::Log => Operation::Log,
            Op::Checkpoint => Operation::Checkpoint,
            Op::Backup => Operation::Backup,
            Op::Verify => Operation::Verify,
            Op::PruneArchive => Operation::PruneArchive,
            Op::StartJob => Operation::StartJob,
            Op::ListJobs => Operation::ListJobs,
            Op::GetOwnJob | Op::GetOthersJob => Operation::GetJob,
            Op::CancelOwnJob | Op::CancelOthersJob => Operation::CancelJob,
            Op::OwnJobResult | Op::OthersJobResult => Operation::GetJobResult,
        }
    }
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
    (Op::Schema, [U, D, A, A, A, A]),
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
    (Op::ServerStatus, [U, A, A, A, A, A]),
    (Op::ActiveRequests, [U, A, A, A, A, A]),
    // A user without grants can't start a request worth cancelling: it
    // cancels one that isn't running
    (Op::CancelOwnRequest, [U, N, A, A, A, A]),
    (Op::CancelOthersRequest, [U, N, N, N, N, A]),
    (Op::Consumers, [U, A, A, A, A, A]),
    (Op::Metrics, [U, A, A, A, A, A]),
    (Op::Log, [U, D, D, D, D, A]),
    // The admin writes: a server admin's, not a namespace admin's
    (Op::Checkpoint, [U, D, D, D, D, A]),
    (Op::Backup, [U, D, D, D, D, A]),
    (Op::Verify, [U, D, D, D, D, A]),
    (Op::PruneArchive, [U, D, D, D, D, A]),
    // Managed jobs: started with `read`; others' are `not_found` but for a
    // server admin. A user without grants has no job of its own: it asks
    // for one that doesn't exist
    (Op::StartJob, [U, D, A, A, A, A]),
    (Op::ListJobs, [U, A, A, A, A, A]),
    (Op::GetOwnJob, [U, N, A, A, A, A]),
    (Op::GetOthersJob, [U, N, N, N, N, A]),
    (Op::CancelOwnJob, [U, N, A, A, A, A]),
    (Op::CancelOthersJob, [U, N, N, N, N, A]),
    (Op::OwnJobResult, [U, N, A, A, A, A]),
    (Op::OthersJobResult, [U, N, N, N, N, A]),
];

/// The backup the server's backup directory starts with (`PruneArchive`).
const SEED_BACKUP: &str = "seed";
/// The names of the backups the `Backup` cells take start so.
const BACKUP_PREFIX: &str = "cell";

/// What the test needs of a client, over gRPC or REST.
trait Client: Database + Accounts + Admin {
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
    /// The server's audit entries.
    audit: Arc<Captured>,
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
        let options = iwdb::StoreOptions { archive: Some(dir.path().join("archive")), ..options() };
        let store = Store::open(&dir.path().join("data"), options).unwrap();
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
        let backups = dir.path().join("backups");
        std::fs::create_dir(&backups).unwrap();
        store.backup(&backups.join(SEED_BACKUP)).unwrap();
        let db = Embedded::new(store, QueryConfig::default())
            .unwrap()
            .with_auth(auth_settings())
            .with_backup_dir(&backups)
            .unwrap();
        let audit = Captured::new();
        let sink = audit.clone();
        let server = Running::start_built(db, |s| s.auth(AuthMode { enabled: true }).audit(sink));
        let admin = server.client().with_token(tokens.last().unwrap().1.clone().unwrap());
        World { server, _dir: dir, admin, tokens, counter: AtomicUsize::new(0), audit }
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
        self.audit.paused(|| {
            block_on(self.admin.create_user(&name, Secret::new("victim-password"), false)).unwrap();
            block_on(self.admin.create_token(&name, "victim", None)).unwrap();
        });
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
            Op::Schema => block_on(c.schema(NS, o())).map(drop),
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
                    self.audit.paused(|| block_on(self.admin.drop_namespace(&name, None)).unwrap());
                })
            }
            Op::DropNamespace => {
                // A namespace with the same grants as NS
                let name = self.unique("drop");
                self.audit.paused(|| {
                    block_on(self.admin.create_namespace(&name, None)).unwrap();
                    for (who, role) in [(Who::Read, Role::Read), (Who::Write, Role::Write), (Who::NsAdmin, Role::Admin)]
                    {
                        block_on(self.admin.grant(who.user(), &name, role)).unwrap();
                    }
                });
                block_on(c.drop_namespace(&name, None)).map(drop).inspect_err(|_| {
                    self.audit.paused(|| block_on(self.admin.drop_namespace(&name, None)).unwrap());
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
                    self.audit.paused(|| block_on(self.admin.create_token(me, &name, None)).unwrap());
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
            Op::ServerStatus => {
                let status = block_on(c.server_status())?;
                let names: Vec<&str> = status.namespaces.iter().map(|n| n.name.as_str()).collect();
                assert_eq!(names, self.visible(who), "{:?}", who);
                assert!(status.ready && !status.version.is_empty());
                Ok(())
            }
            Op::ActiveRequests => {
                let list = block_on(c.active_requests(None, None))?;
                // At least this very call; only the caller's own unless it
                // is a server admin
                assert!(list.items.iter().any(|r| r.operation == Operation::ActiveRequests && r.user == me));
                if who != Who::Admin {
                    assert!(list.items.iter().all(|r| r.user == me), "{:?}: {:?}", who, list.items);
                }
                Ok(())
            }
            Op::CancelOwnRequest if who == Who::NoGrant => block_on(c.cancel_request(u64::MAX, None)).map(drop),
            Op::CancelOwnRequest | Op::CancelOthersRequest => {
                let owner = if op == Op::CancelOwnRequest { who } else { someone_else(who) };
                // The owner's long poll, cancelled here (or, if this call
                // may not, by the admin afterwards)
                let (id, poll) = self.long_poll(if who == Who::Anonymous { Who::Read } else { owner });
                let result = block_on(c.cancel_request(id, None));
                if result.is_err() {
                    self.audit.paused(|| block_on(self.admin.cancel_request(id, None))).unwrap();
                }
                let polled = poll.join().unwrap();
                assert_eq!(polled.map(drop).unwrap_err().code(), Code::Cancelled);
                result.map(|r| {
                    assert_eq!((r.id, r.user.as_str(), r.namespace.as_deref()), (id, owner.user(), Some(NS)));
                })
            }
            Op::Consumers => {
                // A reader of NS, seen by those who can read NS
                self.audit.paused(|| {
                    block_on(self.admin.changes(NS, ChangesRequest { from_seq: 1, wait: false }, o())).unwrap()
                });
                let list = block_on(c.consumers())?;
                let seen = list.iter().any(|r| r.namespace == NS && r.user == Who::Admin.user());
                assert_eq!(seen, who != Who::NoGrant, "{:?}: {:?}", who, list);
                assert!(list.iter().all(|r| self.visible(who).contains(&r.namespace.as_str())), "{:?}", list);
                Ok(())
            }
            Op::Metrics => {
                let metrics = block_on(c.metrics())?;
                let mut names: Vec<&str> = metrics
                    .family(iwdb_query::metrics::NAMESPACE_NODES)
                    .unwrap()
                    .samples
                    .iter()
                    .filter_map(|s| s.label("namespace"))
                    .collect();
                names.sort_unstable();
                assert_eq!(names, self.visible(who), "{:?}", who);
                Ok(())
            }
            Op::Log => block_on(c.log(0, Some(10))).map(drop),
            Op::Checkpoint => block_on(c.checkpoint(Some(NS.into()))).map(drop),
            Op::Backup => {
                let request = iwdb_query::BackupRequest {
                    name: format!("{}-{}", BACKUP_PREFIX, self.counter.fetch_add(1, Ordering::Relaxed)),
                    max_bytes_per_second: None,
                    verify: false,
                };
                block_on(c.backup(request)).map(drop)
            }
            Op::Verify => block_on(c.verify(iwdb_query::VerifyTarget::Store)).map(drop),
            Op::PruneArchive => block_on(c.prune_archive(SEED_BACKUP.into(), true)).map(drop),
            Op::StartJob => {
                let job = block_on(c.start_job(NS.into(), degree(), o(), None))?;
                assert_eq!((job.user.as_str(), job.namespace.as_str()), (me, NS), "the job is the caller's");
                Ok(())
            }
            Op::ListJobs => {
                // A job of the reader's: listed for the reader and the admin
                self.job(Who::Read, false);
                let list = block_on(c.jobs(None, None))?;
                let readers = list.items.iter().any(|j| j.user == Who::Read.user());
                assert_eq!(readers, matches!(who, Who::Read | Who::Admin), "{:?}: {:?}", who, list.items);
                if who != Who::Admin {
                    assert!(list.items.iter().all(|j| j.user == me), "{:?}: {:?}", who, list.items);
                }
                Ok(())
            }
            Op::GetOwnJob | Op::CancelOwnJob | Op::OwnJobResult if who == Who::NoGrant => match op {
                Op::GetOwnJob => block_on(c.job(u64::MAX, None)).map(drop),
                Op::CancelOwnJob => block_on(c.cancel_job(u64::MAX, None)).map(drop),
                _ => block_on(c.job_result(u64::MAX, None, 0, None)).map(drop),
            },
            Op::GetOwnJob | Op::GetOthersJob => {
                let owner = job_owner(op == Op::GetOwnJob, who);
                let id = self.job(owner, false);
                block_on(c.job(id, None)).map(|j| assert_eq!((j.id, j.user.as_str()), (id, owner.user())))
            }
            Op::CancelOwnJob | Op::CancelOthersJob => {
                let owner = job_owner(op == Op::CancelOwnJob, who);
                let id = self.job(owner, true);
                let result = block_on(c.cancel_job(id, None));
                if result.is_err() {
                    self.audit.paused(|| block_on(self.admin.cancel_job(id, None))).unwrap();
                }
                result.map(|j| assert_eq!((j.state, j.user.as_str()), (JobState::Cancelled, owner.user())))
            }
            Op::OwnJobResult | Op::OthersJobResult => {
                let owner = job_owner(op == Op::OwnJobResult, who);
                let id = self.job(owner, false);
                block_on(c.job_result(id, None, 0, None)).map(|p| assert_eq!(p.job.id, id))
            }
        }
    }

    /// A job of `owner`'s on NS (started over gRPC, not audited): one that
    /// runs until cancelled, or a quick one, waited for until it is done.
    fn job(&self, owner: Who, endless: bool) -> u64 {
        self.audit.paused(|| {
            let job = if endless {
                Job::PageRank(PageRank { tol: 0.0, max_iter: 1 << 40, ..PageRank::default() })
            } else {
                Job::Degree { incoming: false }
            };
            let request = AnalyticsRequest { projection: ProjectionSpec::default(), job };
            let client = self.grpc(owner);
            let id = block_on(client.start_job(NS.into(), request, QueryOptions::default(), None)).unwrap().id;
            if !endless {
                let start = std::time::Instant::now();
                while block_on(client.job(id, None)).unwrap().state != JobState::Done {
                    assert!(start.elapsed() < Duration::from_secs(10), "job {} never finished", id);
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            id
        })
    }

    /// The namespaces `who` has a role on, by name.
    fn visible(&self, who: Who) -> Vec<&'static str> {
        match who {
            Who::Admin => vec!["default", NS],
            Who::NoGrant | Who::Anonymous => vec![],
            _ => vec![NS],
        }
    }

    /// A long poll of NS's changes by `who` on a thread of its own (from a
    /// seq no commit reaches), and its request id once it runs.
    fn long_poll(
        &self,
        who: Who,
    ) -> (u64, std::thread::JoinHandle<Result<iwdb_query::Answer<iwdb_query::Changes>, Error>>) {
        let client = self.grpc(who);
        let poll = std::thread::spawn(move || {
            let options = QueryOptions { timeout: Some(std::time::Duration::from_secs(30)), ..QueryOptions::default() };
            block_on(client.changes(NS, ChangesRequest { from_seq: 1 << 40, wait: true }, options))
        });
        let start = std::time::Instant::now();
        loop {
            let list =
                self.audit.paused(|| block_on(self.admin.active_requests(Some(who.user().into()), None))).unwrap();
            if let Some(r) = list.items.iter().find(|r| r.operation == Operation::Changes) {
                return (r.id, poll);
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(10), "the long poll never ran");
            std::thread::sleep(std::time::Duration::from_millis(5));
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

/// The operations whose successful calls aren't audited: reads, data
/// commits and lists (step 15c's non-goal). Every other operation (any
/// new one too, until it is listed here) must leave an entry per call.
const NOT_AUDITED: &[Operation] = &[
    Operation::Commit,
    Operation::WaitForSeq,
    Operation::GetNodes,
    Operation::GetEdges,
    Operation::Find,
    Operation::Explain,
    Operation::Neighbourhood,
    Operation::Traverse,
    Operation::ShortestPath,
    Operation::RandomWalks,
    Operation::Subgraph,
    Operation::MatchPattern,
    Operation::Analyze,
    Operation::Changes,
    Operation::Catalog,
    Operation::Schema,
    Operation::NamespaceStatus,
    Operation::Namespaces,
    Operation::Users,
    Operation::Tokens,
    Operation::WhoAmI,
    Operation::ServerStatus,
    Operation::ActiveRequests,
    Operation::Consumers,
    Operation::Metrics,
    Operation::Log,
    Operation::ListJobs,
    Operation::GetJob,
    Operation::GetJobResult,
];

/// A quick job.
fn degree() -> AnalyticsRequest {
    AnalyticsRequest { projection: ProjectionSpec::default(), job: Job::Degree { incoming: false } }
}

/// Whose job a job cell acts on: the caller's own (the reader's for a
/// caller without credentials), or someone else's.
fn job_owner(own: bool, who: Who) -> Who {
    match (own, who) {
        (true, Who::Anonymous) => Who::Read,
        (true, _) => who,
        (false, _) => someone_else(who),
    }
}

/// A user other than `who` with a role on NS: whose request
/// `CancelOthersRequest` cancels.
fn someone_else(who: Who) -> Who {
    if who == Who::Read { Who::Write } else { Who::Read }
}

/// What is wrong with the audit entries of `op` run by `who` with outcome
/// `got` (empty: nothing).
fn audit_problems(op: Op, who: Who, got: Outcome, entries: &[AuditEntry]) -> Vec<String> {
    let operation = op.operation();
    // A refusal always; a `not_found` only where every call is audited
    let audited = matches!(got, U | D) || !NOT_AUDITED.contains(&operation);
    if !audited {
        return if entries.is_empty() { vec![] } else { vec![format!("expected no entry, got {:?}", entries)] };
    }
    let [e] = entries else {
        return vec![format!("expected one entry, got {:?}", entries)];
    };
    let mut problems = Vec::new();
    let mut expect = |what: &str, ok: bool| {
        if !ok {
            problems.push(format!("{} wrong in {:?}", what, e));
        }
    };
    expect("operation", e.operation == Some(operation));
    expect("client", e.client == Some("127.0.0.1".parse().unwrap()));
    let code = match got {
        A => None,
        U => Some(Code::Unauthenticated),
        D => Some(Code::PermissionDenied),
        N => Some(Code::NotFound),
    };
    expect("code", e.code == code);
    match (got, op) {
        (U, _) => expect("principal", e.user.is_none() && e.via.is_none()),
        (_, Op::Login) => {
            expect("principal", e.user.as_deref() == Some(Who::Read.user()) && e.via == Some(Via::Session))
        }
        _ => expect("principal", e.user.as_deref() == Some(who.user()) && e.via == Some(Via::ApiToken)),
    }
    if got == A {
        match op {
            Op::CommitCatalog => expect("seq", e.namespace.as_deref() == Some(NS) && e.seq.is_some()),
            Op::CreateNamespace | Op::DropNamespace => {
                expect("namespace_event", e.namespace.is_some() && e.namespace_event.is_some())
            }
            Op::Grant => expect("grant", e.subject.is_some() && e.namespace.as_deref() == Some(NS) && e.role.is_some()),
            Op::CreateOwnToken | Op::RevokeOwnToken => {
                expect("token", e.subject.as_deref() == Some(who.user()) && e.token_name.is_some())
            }
            Op::CancelOwnRequest => expect(
                "request",
                e.request.is_some() && e.subject.as_deref() == Some(who.user()) && e.namespace.as_deref() == Some(NS),
            ),
            Op::CancelOthersRequest => expect(
                "request",
                e.request.is_some()
                    && e.subject.as_deref() == Some(someone_else(who).user())
                    && e.namespace.as_deref() == Some(NS),
            ),
            Op::Checkpoint => expect("namespace", e.namespace.as_deref() == Some(NS)),
            Op::Backup => expect("backup", e.backup.as_deref().is_some_and(|b| b.starts_with(BACKUP_PREFIX))),
            Op::PruneArchive => expect("backup", e.backup.as_deref() == Some(SEED_BACKUP)),
            Op::StartJob => expect("job", e.namespace.as_deref() == Some(NS) && e.request.is_some()),
            Op::CancelOwnJob | Op::CancelOthersJob => {
                let owner = job_owner(op == Op::CancelOwnJob, who);
                expect(
                    "job",
                    e.request.is_some()
                        && e.subject.as_deref() == Some(owner.user())
                        && e.namespace.as_deref() == Some(NS),
                )
            }
            _ => {}
        }
    }
    if got == N {
        expect("request", e.request.is_some() && e.subject.is_none());
    }
    problems
}

fn check(transport: &str, world: &World, run: impl Fn(Op, Who) -> Result<(), Error>) -> Vec<String> {
    let mut failures = Vec::new();
    for (op, outcomes) in TABLE {
        for (who, expected) in CALLERS.into_iter().zip(outcomes) {
            world.audit.take();
            let result = run(*op, who);
            let entries = world.audit.take();
            let got = match &result {
                Ok(()) => A,
                Err(e) if e.code() == Code::Unauthenticated => U,
                Err(e) if e.code() == Code::PermissionDenied => D,
                Err(e) if e.code() == Code::NotFound => N,
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
            for problem in audit_problems(*op, who, got, &entries) {
                failures.push(format!("{} {:?} as {:?}: audit: {}", transport, op, who, problem));
            }
            // No password or token in any entry
            let text = format!("{:?}", entries);
            for (w, token) in &world.tokens {
                let leaked = token.as_ref().is_some_and(|t| text.contains(t.expose()));
                if leaked || text.contains(&w.password()) {
                    failures.push(format!("{} {:?} as {:?}: a secret in {}", transport, op, who, text));
                }
            }
        }
    }
    failures
}

#[test]
fn every_role_and_operation_over_grpc_and_rest() {
    // The table covers every operation the authorisation point knows, so
    // every audited one is checked too
    for &operation in Operation::ALL {
        assert!(TABLE.iter().any(|(op, _)| op.operation() == operation), "{:?} isn't in the table", operation);
        let expected = if NOT_AUDITED.contains(&operation) { Audited::Refusals } else { Audited::Always };
        assert_eq!(operation.audited(), expected, "{:?}: the operation table's audit column", operation);
    }
    let world = World::new();
    let grpc = check("gRPC", &world, |op, who| world.run(op, who, &world.grpc(who), true));
    #[cfg(feature = "rest")]
    let rest = check("REST", &world, |op, who| world.run(op, who, &world.rest(who), false));
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

/// A job's result holds the namespace's node ids: its owner fetches it only
/// while it can still read the namespace (ADR 0056). The refusal is
/// audited; the job's state stays visible.
#[test]
fn a_job_result_needs_read_on_the_namespace_still() {
    let world = World::new();
    let id = world.job(Who::Read, false);
    world.audit.paused(|| block_on(world.admin.revoke(Who::Read.user(), NS))).unwrap();
    world.audit.take();
    // A new token resolves the principal without the grant
    let reader = world.grpc(Who::Read);
    let e = block_on(reader.job_result(id, None, 0, None)).unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied, "{}", e);
    let entries = world.audit.take();
    let [entry] = &entries[..] else { panic!("one entry: {:?}", entries) };
    assert_eq!((entry.operation, entry.code), (Some(Operation::GetJobResult), Some(Code::PermissionDenied)));
    assert_eq!((entry.namespace.as_deref(), entry.request), (Some(NS), Some(id)));
    assert_eq!(block_on(reader.job(id, None)).unwrap().state, JobState::Done);
    assert!(block_on(world.admin.job_result(id, None, 0, None)).is_ok());
}
