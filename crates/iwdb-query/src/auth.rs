//! Authentication and authorisation (step 15a): who a caller is
//! ([`Principal`]), what each operation needs ([`Operation::requires`]), and
//! the one place that decides ([`Authorized`], design rule 8, ADR 0045).
//!
//! - [`Accounts`]: users, grants and API tokens, as a service. `iwdb::Embedded`
//!   implements it on the store's system namespace (ADR 0043); the gRPC and
//!   REST clients implement it again over the wire.
//! - [`Authenticate`]: logging in and turning a token into a principal.
//!   Only the server's side has it (`iwdb::Embedded`); the server's gate
//!   calls it once per request.
//! - [`Authorized`]: a [`Database`] and [`Accounts`] that checks the
//!   caller's principal against [`Operation::requires`] before it
//!   delegates. Every adapter runs every call through it; none checks on
//!   its own.
//!
//! Secrets (passwords, tokens) travel as [`Secret`], whose `Debug` and
//! `Display` never show them, so they can't end up in a log or an error
//! message by accident.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use ironweaver_core::EdgeId;
use iwdb_engine::catalog::NamespaceCatalog;
use iwdb_engine::{CatalogChange, CommitResult, IdempotencyKey, Mutation};
use iwdb_storage::namespaces::{NamespaceInfo, NamespaceResult};

use crate::read::Explain;
use crate::{
    AnalyticsRequest, Answer, Changes, ChangesRequest, Code, CommitOptions, Database, Edge, Error, ExplainRequest,
    FindRequest, JobResult, MatchRequest, MatchRow, NamespaceStatus, NeighbourhoodRequest, Node, Path, PathRequest,
    QueryOptions, Subgraph, SubgraphRequest, TraverseRequest, WalkRequest,
};

/// A password or a token. Its `Debug` and `Display` print `***`; only
/// [`expose`](Self::expose) gives the text, where it is hashed, compared
/// or sent.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(text: impl Into<String>) -> Self {
        Secret(text.into())
    }

    /// The secret's text: for hashing, comparing and sending, never for
    /// logging.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<&str> for Secret {
    fn from(text: &str) -> Self {
        Secret(text.to_owned())
    }
}

impl From<String> for Secret {
    fn from(text: String) -> Self {
        Secret(text)
    }
}

/// A role on a namespace. Each includes the ones before it: `admin`
/// can write, `write` can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Every read, the change stream, the catalog and the status.
    Read,
    /// `read`, and commits.
    Write,
    /// `write`, and catalog changes and dropping the namespace.
    Admin,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Read, Role::Write, Role::Admin];

    /// The role's name: `read`, `write`, `admin`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    pub fn parse(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == name)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who makes a request: a user, whether it is a server-wide admin, and its
/// roles per namespace (by name, as of when the request was
/// authenticated).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub user: String,
    /// The server-wide `admin` role: users, grants, creating namespaces,
    /// and `admin` on every namespace.
    pub admin: bool,
    pub grants: BTreeMap<String, Role>,
}

impl Principal {
    /// The principal of every request when authentication is off (and of
    /// in-process callers): a server-wide admin. Its user name can't be a
    /// user's (user names start with a letter or digit).
    pub fn unauthenticated() -> Self {
        Principal { user: "(authentication off)".into(), admin: true, grants: BTreeMap::new() }
    }

    /// The principal's role on `namespace`: `admin` for a server-wide
    /// admin, its grant otherwise.
    pub fn role(&self, namespace: &str) -> Option<Role> {
        if self.admin { Some(Role::Admin) } else { self.grants.get(namespace).copied() }
    }
}

/// A user, as [`Accounts`] reports it (never with its password hash).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub admin: bool,
    /// Roles by namespace name (grants of dropped namespaces are gone).
    pub grants: BTreeMap<String, Role>,
}

/// An API token, as [`Accounts`] reports it (never with its secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    pub user: String,
    /// Unique per user.
    pub name: String,
    /// When it was made, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// When it expires, in milliseconds since the Unix epoch; `None`: never.
    pub expires_ms: Option<u64>,
}

/// A new API token: its secret is shown this once and stored only as a
/// hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewToken {
    pub info: TokenInfo,
    pub token: Secret,
}

/// A login's session: a bearer token until `expires_ms` (milliseconds
/// since the Unix epoch), or until logout, a password change or the user's
/// deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub token: Secret,
    pub user: UserInfo,
    pub expires_ms: u64,
}

/// Users, grants and API tokens (ADR 0043). Implemented by the embedded
/// store, by the clients over the wire, and by [`Authorized`], which
/// checks the caller first.
///
/// User names follow the rules of namespace names (1 to 64 ASCII letters,
/// digits, `_` or `-`, starting with a letter or digit). Passwords are at
/// least 8 and at most 1024 bytes. Errors: `invalid_argument`,
/// `not_found` (no such user, namespace or token), `conflict` (a user or
/// token name that exists), `unauthenticated` (a wrong current password),
/// `permission_denied` (through [`Authorized`]).
pub trait Accounts: Send + Sync {
    /// Every user, by name.
    fn users(&self) -> impl Future<Output = Result<Vec<UserInfo>, Error>> + Send;

    /// Create a user with a password; `admin` makes it a server-wide admin.
    fn create_user(
        &self,
        name: &str,
        password: Secret,
        admin: bool,
    ) -> impl Future<Output = Result<UserInfo, Error>> + Send;

    /// Set a user's password. With `current`, only if it is the user's
    /// password now (`unauthenticated` otherwise): how users change their
    /// own. Ends the user's sessions; API tokens stay valid.
    fn set_password(
        &self,
        name: &str,
        password: Secret,
        current: Option<Secret>,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Delete a user, its grants, sessions and API tokens.
    fn delete_user(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send;

    /// Make a user a server-wide admin, or not. The last admin can't stop
    /// being one (`invalid_argument`), nor be deleted.
    fn set_admin(&self, name: &str, admin: bool) -> impl Future<Output = Result<UserInfo, Error>> + Send;

    /// Give a user `role` on `namespace` (replacing its role there). The
    /// namespace must exist; the grant ends when it is dropped.
    fn grant(&self, name: &str, namespace: &str, role: Role) -> impl Future<Output = Result<UserInfo, Error>> + Send;

    /// Take a user's role on `namespace` away.
    fn revoke(&self, name: &str, namespace: &str) -> impl Future<Output = Result<UserInfo, Error>> + Send;

    /// Make an API token for `user`, named `name` (unique per user), that
    /// expires after `expires_in` (`None`: never).
    fn create_token(
        &self,
        user: &str,
        name: &str,
        expires_in: Option<Duration>,
    ) -> impl Future<Output = Result<NewToken, Error>> + Send;

    /// Revoke `user`'s API token `name`.
    fn revoke_token(&self, user: &str, name: &str) -> impl Future<Output = Result<(), Error>> + Send;

    /// `user`'s API tokens, by name (expired ones included).
    fn tokens(&self, user: &str) -> impl Future<Output = Result<Vec<TokenInfo>, Error>> + Send;
}

/// Logging in, and turning a bearer token into a [`Principal`]: the
/// server's side of authentication (ADR 0044). Only the embedded store
/// implements it; the server's gate calls [`authenticate`](Self::authenticate)
/// once per request.
pub trait Authenticate: Send + Sync {
    /// Check `user`'s password and start a session. Failed logins are
    /// slowed down per user and per `client` address; every failure is
    /// `unauthenticated` with the same message, whether the user exists or
    /// not.
    fn login(
        &self,
        user: &str,
        password: Secret,
        client: Option<IpAddr>,
    ) -> impl Future<Output = Result<Session, Error>> + Send;

    /// End the session of `token` (a session token; an API token is
    /// revoked with [`Accounts::revoke_token`]). Unknown tokens are fine.
    fn logout(&self, token: &Secret) -> impl Future<Output = Result<(), Error>> + Send;

    /// The principal of a session or API token; `unauthenticated` for an
    /// unknown, expired or revoked one.
    fn authenticate(&self, token: &Secret) -> impl Future<Output = Result<Principal, Error>> + Send;
}

/// What an operation needs of the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    /// Any authenticated caller.
    Authenticated,
    /// At least this role on the operation's namespace.
    Namespace(Role),
    /// The server-wide `admin` role.
    ServerAdmin,
    /// The server-wide `admin` role, or being the user the operation is
    /// about.
    SelfOrAdmin,
}

macro_rules! operations {
    ($($op:ident => $name:literal, $req:expr;)*) => {
        /// Every authorised operation: those of [`Database`] and
        /// [`Accounts`], plus `whoami` and `logout`. Login needs no
        /// principal and isn't one.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Operation { $($op,)* }

        impl Operation {
            pub const ALL: &'static [Operation] = &[$(Operation::$op,)*];

            /// The operation's name: its RPC in the protos.
            pub fn name(self) -> &'static str {
                match self { $(Operation::$op => $name,)* }
            }

            /// What the operation needs: the table of ADR 0045.
            pub fn requires(self) -> Requirement {
                use Requirement::*;
                match self { $(Operation::$op => $req,)* }
            }
        }
    };
}

operations! {
    Commit => "Commit", Namespace(Role::Write);
    CommitCatalog => "CommitCatalog", Namespace(Role::Admin);
    WaitForSeq => "WaitForSeq", Namespace(Role::Read);
    GetNodes => "GetNodes", Namespace(Role::Read);
    GetEdges => "GetEdges", Namespace(Role::Read);
    Find => "Find", Namespace(Role::Read);
    Explain => "Explain", Namespace(Role::Read);
    Neighbourhood => "Neighbourhood", Namespace(Role::Read);
    Traverse => "Traverse", Namespace(Role::Read);
    ShortestPath => "ShortestPath", Namespace(Role::Read);
    RandomWalks => "RandomWalks", Namespace(Role::Read);
    Subgraph => "Subgraph", Namespace(Role::Read);
    MatchPattern => "MatchPattern", Namespace(Role::Read);
    Analyze => "Analyze", Namespace(Role::Read);
    Changes => "GetChanges", Namespace(Role::Read);
    Catalog => "GetCatalog", Namespace(Role::Read);
    NamespaceStatus => "GetNamespaceStatus", Namespace(Role::Read);
    Namespaces => "ListNamespaces", Authenticated;
    CreateNamespace => "CreateNamespace", ServerAdmin;
    DropNamespace => "DropNamespace", Namespace(Role::Admin);
    Users => "ListUsers", ServerAdmin;
    CreateUser => "CreateUser", ServerAdmin;
    SetPassword => "SetPassword", SelfOrAdmin;
    DeleteUser => "DeleteUser", ServerAdmin;
    SetAdmin => "SetAdmin", ServerAdmin;
    Grant => "Grant", ServerAdmin;
    Revoke => "Revoke", ServerAdmin;
    CreateToken => "CreateToken", SelfOrAdmin;
    RevokeToken => "RevokeToken", SelfOrAdmin;
    Tokens => "ListTokens", SelfOrAdmin;
    WhoAmI => "WhoAmI", Authenticated;
    Logout => "Logout", Authenticated;
}

impl Operation {
    /// Whether `principal` may run this operation on `subject`: the
    /// namespace for a namespace operation, the user for a user operation.
    /// Errors: `permission_denied`, naming the operation and what it
    /// needs (never a secret).
    pub fn check(self, principal: &Principal, subject: &str) -> Result<(), Error> {
        let allowed = match self.requires() {
            Requirement::Authenticated => true,
            Requirement::Namespace(role) => principal.role(subject).is_some_and(|has| has >= role),
            Requirement::ServerAdmin => principal.admin,
            Requirement::SelfOrAdmin => principal.admin || principal.user == subject,
        };
        if allowed {
            return Ok(());
        }
        let needs = match self.requires() {
            Requirement::Namespace(role) => format!("the '{}' role on namespace '{}'", role, subject),
            Requirement::SelfOrAdmin => format!("to be user '{}' or a server admin", subject),
            _ => "the server-wide 'admin' role".to_owned(),
        };
        Err(Error::new(
            Code::PermissionDenied,
            format!("user '{}' may not {}: it needs {}", principal.user, self.name(), needs),
        ))
    }
}

/// A database and account service seen through a caller's [`Principal`]:
/// every call is checked against [`Operation::requires`] first, and only
/// then passed to the inner service (ADR 0045). This is the one place
/// that authorises; adapters build one per request from the principal the
/// gate authenticated.
///
/// `namespaces()` lists only the namespaces the caller has a role on (all
/// for an admin). A non-admin changing its own password must give its
/// current one.
pub struct Authorized<D> {
    inner: Arc<D>,
    principal: Arc<Principal>,
}

impl<D> Clone for Authorized<D> {
    fn clone(&self) -> Self {
        Authorized { inner: self.inner.clone(), principal: self.principal.clone() }
    }
}

impl<D> Authorized<D> {
    pub fn new(inner: Arc<D>, principal: Arc<Principal>) -> Self {
        Authorized { inner, principal }
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn inner(&self) -> &Arc<D> {
        &self.inner
    }

    /// The caller as a [`UserInfo`] (`whoami`).
    pub fn whoami(&self) -> UserInfo {
        let p = &self.principal;
        UserInfo { name: p.user.clone(), admin: p.admin, grants: p.grants.clone() }
    }

    fn check(&self, op: Operation, subject: &str) -> Result<(), Error> {
        op.check(&self.principal, subject)
    }
}

/// A namespace read: checked, then delegated with the same arguments.
macro_rules! read {
    ($method:ident, $op:ident, $request:ty, $answer:ty) => {
        fn $method(
            &self,
            namespace: &str,
            request: $request,
            options: QueryOptions,
        ) -> impl Future<Output = Result<$answer, Error>> + Send {
            let check = self.check(Operation::$op, namespace);
            async move {
                check?;
                self.inner.$method(namespace, request, options).await
            }
        }
    };
}

impl<D: Database + Accounts> Database for Authorized<D> {
    fn commit(
        &self,
        namespace: &str,
        mutations: Vec<Mutation>,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let check = self.check(Operation::Commit, namespace);
        async move {
            check?;
            self.inner.commit(namespace, mutations, options).await
        }
    }

    fn commit_catalog(
        &self,
        namespace: &str,
        change: CatalogChange,
        options: CommitOptions,
    ) -> impl Future<Output = Result<CommitResult, Error>> + Send {
        let check = self.check(Operation::CommitCatalog, namespace);
        async move {
            check?;
            self.inner.commit_catalog(namespace, change, options).await
        }
    }

    fn wait_for_seq(
        &self,
        namespace: &str,
        seq: u64,
        options: QueryOptions,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        let check = self.check(Operation::WaitForSeq, namespace);
        async move {
            check?;
            self.inner.wait_for_seq(namespace, seq, options).await
        }
    }

    read!(get_nodes, GetNodes, Vec<String>, Answer<Vec<Option<Node>>>);
    read!(get_edges, GetEdges, Vec<EdgeId>, Answer<Vec<Option<Edge>>>);
    read!(find, Find, FindRequest, Answer<Vec<Node>>);
    read!(explain, Explain, ExplainRequest, Answer<Explain>);
    read!(neighbourhood, Neighbourhood, NeighbourhoodRequest, Answer<Vec<Node>>);
    read!(traverse, Traverse, TraverseRequest, Answer<Vec<String>>);
    read!(shortest_path, ShortestPath, PathRequest, Answer<Option<Path>>);
    read!(random_walks, RandomWalks, WalkRequest, Answer<Vec<Vec<String>>>);
    read!(subgraph, Subgraph, SubgraphRequest, Answer<Subgraph>);
    read!(match_pattern, MatchPattern, MatchRequest, Answer<Vec<MatchRow>>);
    read!(analyze, Analyze, AnalyticsRequest, Answer<JobResult>);
    read!(changes, Changes, ChangesRequest, Answer<Changes>);

    fn catalog(
        &self,
        namespace: &str,
        options: QueryOptions,
    ) -> impl Future<Output = Result<Answer<NamespaceCatalog>, Error>> + Send {
        let check = self.check(Operation::Catalog, namespace);
        async move {
            check?;
            self.inner.catalog(namespace, options).await
        }
    }

    fn namespace_status(&self, namespace: &str) -> impl Future<Output = Result<NamespaceStatus, Error>> + Send {
        let check = self.check(Operation::NamespaceStatus, namespace);
        async move {
            check?;
            self.inner.namespace_status(namespace).await
        }
    }

    fn namespaces(&self) -> impl Future<Output = Result<Vec<NamespaceInfo>, Error>> + Send {
        let check = self.check(Operation::Namespaces, "");
        async move {
            check?;
            let mut list = self.inner.namespaces().await?;
            list.retain(|info| self.principal.role(info.name.as_str()).is_some());
            Ok(list)
        }
    }

    fn create_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let check = self.check(Operation::CreateNamespace, name);
        async move {
            check?;
            self.inner.create_namespace(name, key).await
        }
    }

    fn drop_namespace(
        &self,
        name: &str,
        key: Option<IdempotencyKey>,
    ) -> impl Future<Output = Result<NamespaceResult, Error>> + Send {
        let check = self.check(Operation::DropNamespace, name);
        async move {
            check?;
            self.inner.drop_namespace(name, key).await
        }
    }
}

impl<D: Database + Accounts> Accounts for Authorized<D> {
    fn users(&self) -> impl Future<Output = Result<Vec<UserInfo>, Error>> + Send {
        let check = self.check(Operation::Users, "");
        async move {
            check?;
            self.inner.users().await
        }
    }

    fn create_user(
        &self,
        name: &str,
        password: Secret,
        admin: bool,
    ) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let check = self.check(Operation::CreateUser, name);
        async move {
            check?;
            self.inner.create_user(name, password, admin).await
        }
    }

    fn set_password(
        &self,
        name: &str,
        password: Secret,
        current: Option<Secret>,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        let check = self.check(Operation::SetPassword, name).and_then(|()| {
            // Users change their own password with the current one, so a
            // stolen session can't lock the owner out
            if !self.principal.admin && current.is_none() {
                Err(Error::invalid("changing your own password needs your current password"))
            } else {
                Ok(())
            }
        });
        async move {
            check?;
            self.inner.set_password(name, password, current).await
        }
    }

    fn delete_user(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let check = self.check(Operation::DeleteUser, name);
        async move {
            check?;
            self.inner.delete_user(name).await
        }
    }

    fn set_admin(&self, name: &str, admin: bool) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let check = self.check(Operation::SetAdmin, name);
        async move {
            check?;
            self.inner.set_admin(name, admin).await
        }
    }

    fn grant(&self, name: &str, namespace: &str, role: Role) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let check = self.check(Operation::Grant, name);
        async move {
            check?;
            self.inner.grant(name, namespace, role).await
        }
    }

    fn revoke(&self, name: &str, namespace: &str) -> impl Future<Output = Result<UserInfo, Error>> + Send {
        let check = self.check(Operation::Revoke, name);
        async move {
            check?;
            self.inner.revoke(name, namespace).await
        }
    }

    fn create_token(
        &self,
        user: &str,
        name: &str,
        expires_in: Option<Duration>,
    ) -> impl Future<Output = Result<NewToken, Error>> + Send {
        let check = self.check(Operation::CreateToken, user);
        async move {
            check?;
            self.inner.create_token(user, name, expires_in).await
        }
    }

    fn revoke_token(&self, user: &str, name: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let check = self.check(Operation::RevokeToken, user);
        async move {
            check?;
            self.inner.revoke_token(user, name).await
        }
    }

    fn tokens(&self, user: &str) -> impl Future<Output = Result<Vec<TokenInfo>, Error>> + Send {
        let check = self.check(Operation::Tokens, user);
        async move {
            check?;
            self.inner.tokens(user).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(admin: bool, grants: &[(&str, Role)]) -> Principal {
        Principal { user: "ann".into(), admin, grants: grants.iter().map(|(n, r)| ((*n).to_owned(), *r)).collect() }
    }

    #[test]
    fn roles_include_the_ones_below() {
        assert!(Role::Admin > Role::Write && Role::Write > Role::Read);
        for role in Role::ALL {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("owner"), None);
        let writer = user(false, &[("social", Role::Write)]);
        assert!(Operation::Find.check(&writer, "social").is_ok());
        assert!(Operation::Commit.check(&writer, "social").is_ok());
        let denied = Operation::CommitCatalog.check(&writer, "social").expect_err("denied");
        assert_eq!(denied.code(), Code::PermissionDenied);
        assert!(denied.message().contains("'admin' role on namespace 'social'"), "{}", denied);
        assert!(Operation::Find.check(&writer, "other").is_err());
    }

    #[test]
    fn admins_may_do_everything_and_users_manage_themselves() {
        let admin = user(true, &[]);
        let ann = user(false, &[]);
        for &op in Operation::ALL {
            assert!(op.check(&admin, "anything").is_ok(), "{:?}", op);
        }
        assert!(Operation::SetPassword.check(&ann, "ann").is_ok());
        assert!(Operation::CreateToken.check(&ann, "ann").is_ok());
        assert!(Operation::SetPassword.check(&ann, "bob").is_err());
        assert!(Operation::CreateUser.check(&ann, "ann").is_err());
        assert!(Operation::WhoAmI.check(&ann, "").is_ok());
        assert!(Principal::unauthenticated().admin);
    }

    #[test]
    fn operation_names_are_unique() {
        let mut names: Vec<_> = Operation::ALL.iter().map(|o| o.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Operation::ALL.len());
    }

    #[test]
    fn secrets_never_print() {
        let s = Secret::new("hunter2-hunter2");
        assert_eq!(format!("{} {:?}", s, s), "*** Secret(***)");
        assert_eq!(s.expose(), "hunter2-hunter2");
    }
}
