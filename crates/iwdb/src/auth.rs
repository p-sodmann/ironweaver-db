//! Users, grants and API tokens (step 15a): durable in the store's reserved
//! system namespace (ADR 0043), passwords hashed with argon2id and tokens
//! stored as SHA-256 hashes (ADR 0044).
//!
//! - [`Users`] ([`Store::users`]): create, change and delete users, grant
//!   and revoke roles, make and revoke API tokens, check a password. Every
//!   change is one commit to the system namespace, through the store's
//!   commit pipeline and WAL (design rule 2): all or nothing, durable per
//!   the fsync policy, recovered, backed up and restored with the rest of
//!   the store. The namespace is created with the first user, so a store
//!   without users is exactly what it was before step 15a.
//! - [`Sessions`] and [`Throttle`]: the server's in-memory login sessions
//!   and its slowdown of failed logins. Sessions end when the process does.
//!
//! **Records** (`documentation/formats/auth.md`), nodes of the system
//! namespace: `auth` (`{format: 1}`, written with the first user; a store
//! with a format this version doesn't know refuses every auth operation),
//! `user:<name>` (`name`, `password` as a PHC string, `admin`, `epoch`,
//! `grants` by namespace id, `created`), `token:<sha256 hex>` (`user`,
//! `name`, `created`, `expires`). Grants name namespaces by id, so a
//! namespace dropped and created again under its name has no grants.
//!
//! **Secrets.** Neither a password nor a token is stored, logged or put in
//! an error message: a password only as its argon2id hash, a token only as
//! its SHA-256 hash (tokens are 256 random bits, so a fast hash is enough).
//! Passwords are compared by argon2's verifier, which compares hashes in
//! constant time.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use ironweaver_core::{Attrs, Value};
use iwdb_engine::Mutation;
use iwdb_engine::catalog::NamespaceName;
use iwdb_query::{Code, CommitOptions, Error, NewToken, Role, Secret, TokenInfo, UserInfo};
use iwdb_storage::io::LogFs;
use sha2::{Digest, Sha256};

use crate::{Ns, Store};

/// The version of the records in the system namespace (`auth.md`).
pub const AUTH_FORMAT: i64 = 1;
/// Shortest password accepted.
pub const MIN_PASSWORD_BYTES: usize = 8;
/// Longest password accepted (argon2 takes any length; this bounds work).
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// API tokens and session tokens start with this, so secret scanners can
/// find them.
pub const TOKEN_PREFIX: &str = "iwdb_";

const FORMAT_NODE: &str = "auth";
const USER_NODE: &str = "user:";
const TOKEN_NODE: &str = "token:";

/// The argon2id cost parameters new hashes get. A hash records its own,
/// so raising them later affects new hashes only; a hash with other
/// parameters is replaced at the user's next successful login.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashParams {
    /// Memory in KiB (default 19 456, OWASP's recommendation).
    pub memory_kib: u32,
    /// Passes (default 2).
    pub iterations: u32,
    /// Lanes (default 1).
    pub parallelism: u32,
}

impl Default for HashParams {
    fn default() -> Self {
        HashParams {
            memory_kib: Params::DEFAULT_M_COST,
            iterations: Params::DEFAULT_T_COST,
            parallelism: Params::DEFAULT_P_COST,
        }
    }
}

impl HashParams {
    fn hasher(&self) -> Result<Argon2<'static>, Error> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, None)
            .map_err(|e| Error::invalid(format!("invalid password hash parameters: {}", e)))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }

    /// Hash `password` with a new random salt, as a PHC string.
    pub fn hash(&self, password: &Secret) -> Result<String, Error> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| Error::internal(format!("no random salt: {}", e)))?;
        let hash = self
            .hasher()?
            .hash_password_with_salt(password.expose().as_bytes(), &salt)
            .map_err(|e| Error::internal(format!("hashing a password failed: {}", e)))?;
        Ok(hash.to_string())
    }

    /// Whether `password` matches the PHC string `phc` (whatever its
    /// parameters), and whether the hash should be replaced because its
    /// parameters aren't these. A string that isn't an argon2id hash is
    /// `corrupt`.
    pub fn verify(&self, password: &Secret, phc: &str) -> Result<(bool, bool), Error> {
        let parsed =
            PasswordHash::new(phc).map_err(|_| Error::new(Code::Corrupt, "a stored password hash is invalid"))?;
        let ok = PasswordVerifier::<PasswordHash>::verify_password(
            &Argon2::default(),
            password.expose().as_bytes(),
            &parsed,
        )
        .is_ok();
        let current = Params::try_from(&parsed).ok().is_some_and(|p| {
            parsed.algorithm.as_str() == "argon2id"
                && p.m_cost() == self.memory_kib
                && p.t_cost() == self.iterations
                && p.p_cost() == self.parallelism
        });
        Ok((ok, !current))
    }
}

/// How a server authenticates: session lifetime, the login slowdown and
/// the hash parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthSettings {
    /// How long a login's session lasts (default 12 hours).
    pub session_lifetime: Duration,
    /// Failed logins per user, and per client address, within
    /// `failure_window` before further attempts are refused until the
    /// window has passed (default 5).
    pub max_failures: u32,
    /// Default 60 seconds.
    pub failure_window: Duration,
    /// Users and addresses the slowdown remembers; the least recently seen
    /// is forgotten first (default 10 000).
    pub table_size: usize,
    /// Most sessions at once; the one that expires first goes when a login
    /// would exceed it (default 65 536).
    pub max_sessions: usize,
    pub hash: HashParams,
}

impl Default for AuthSettings {
    fn default() -> Self {
        AuthSettings {
            session_lifetime: Duration::from_secs(12 * 3600),
            max_failures: 5,
            failure_window: Duration::from_secs(60),
            table_size: 10_000,
            max_sessions: 65_536,
            hash: HashParams::default(),
        }
    }
}

/// Milliseconds since the Unix epoch now.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A new token: [`TOKEN_PREFIX`] and 256 random bits in hex.
pub fn new_token() -> Result<Secret, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::internal(format!("no random token: {}", e)))?;
    Ok(Secret::new(format!("{}{}", TOKEN_PREFIX, hex(&bytes))))
}

/// The hash a token is stored and looked up by: SHA-256, in hex.
pub fn token_hash(token: &Secret) -> String {
    hex(&Sha256::digest(token.expose().as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// A user name: the rules of namespace names, so it is safe in logs and
/// node ids.
fn check_user_name(name: &str) -> Result<(), Error> {
    match NamespaceName::new(name) {
        Ok(n) if !n.is_reserved() => Ok(()),
        _ => Err(Error::invalid(format!(
            "invalid user name {:?}: 1 to 64 ASCII letters, digits, '_' or '-', starting with a letter or digit",
            name
        ))),
    }
}

fn check_password(password: &Secret) -> Result<(), Error> {
    let n = password.expose().len();
    if !(MIN_PASSWORD_BYTES..=MAX_PASSWORD_BYTES).contains(&n) {
        return Err(Error::invalid(format!(
            "a password must be {} to {} bytes long",
            MIN_PASSWORD_BYTES, MAX_PASSWORD_BYTES
        )));
    }
    Ok(())
}

fn check_token_name(name: &str) -> Result<(), Error> {
    let ok =
        !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(format!("invalid token name {:?}: 1 to 64 ASCII letters, digits, '_', '-' or '.'", name)))
    }
}

fn corrupt(what: &str) -> Error {
    Error::new(Code::Corrupt, format!("the system namespace holds an invalid {}", what))
}

/// A user record as stored.
#[derive(Clone, Debug)]
pub(crate) struct UserRecord {
    pub name: String,
    phc: String,
    pub admin: bool,
    /// Grows with every password change; sessions of an older epoch end.
    pub epoch: i64,
    /// Roles by namespace id.
    grants: BTreeMap<u64, Role>,
    created: i64,
}

impl UserRecord {
    fn from_attrs(attr: &Attrs) -> Result<Self, Error> {
        let string = |k: &str| match attr.get(k) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(corrupt("user")),
        };
        let int = |k: &str| match attr.get(k) {
            Some(Value::Int(i)) => Ok(*i),
            _ => Err(corrupt("user")),
        };
        let admin = match attr.get("admin") {
            Some(Value::Bool(b)) => *b,
            _ => return Err(corrupt("user")),
        };
        let mut grants = BTreeMap::new();
        match attr.get("grants") {
            Some(Value::Dict(map)) => {
                for (id, role) in map {
                    let id: u64 = id.parse().map_err(|_| corrupt("grant"))?;
                    let role = match role {
                        Value::String(r) => Role::parse(r).ok_or_else(|| corrupt("grant"))?,
                        _ => return Err(corrupt("grant")),
                    };
                    grants.insert(id, role);
                }
            }
            _ => return Err(corrupt("user")),
        }
        Ok(UserRecord {
            name: string("name")?,
            phc: string("password")?,
            admin,
            epoch: int("epoch")?,
            grants,
            created: int("created")?,
        })
    }

    fn attrs(&self) -> Attrs {
        let grants = self.grants.iter().map(|(id, r)| (id.to_string(), Value::String(r.as_str().into()))).collect();
        Attrs::from([
            ("name".to_owned(), Value::String(self.name.clone())),
            ("password".to_owned(), Value::String(self.phc.clone())),
            ("admin".to_owned(), Value::Bool(self.admin)),
            ("epoch".to_owned(), Value::Int(self.epoch)),
            ("grants".to_owned(), Value::Dict(grants)),
            ("created".to_owned(), Value::Int(self.created)),
        ])
    }

    fn upsert(&self) -> Mutation {
        Mutation::UpsertNode {
            id: format!("{}{}", USER_NODE, self.name),
            labels: vec!["User".into()],
            attr: self.attrs(),
            meta: Attrs::new(),
            expected_version: None,
        }
    }

    /// As [`UserInfo`], with grants by name of the live namespaces in
    /// `names` (id to name).
    pub(crate) fn info(&self, names: &HashMap<u64, String>) -> UserInfo {
        let grants = self.grants.iter().filter_map(|(id, r)| Some((names.get(id)?.clone(), *r))).collect();
        UserInfo { name: self.name.clone(), admin: self.admin, grants }
    }
}

/// A token record as stored.
#[derive(Clone, Debug)]
pub(crate) struct TokenRecord {
    hash: String,
    pub user: String,
    name: String,
    created_ms: u64,
    pub expires_ms: Option<u64>,
}

impl TokenRecord {
    fn from_attrs(hash: &str, attr: &Attrs) -> Result<Self, Error> {
        let string = |k: &str| match attr.get(k) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(corrupt("token")),
        };
        let ms = |v: Option<&Value>| match v {
            Some(Value::Int(i)) => u64::try_from(*i).map_err(|_| corrupt("token")),
            _ => Err(corrupt("token")),
        };
        let expires_ms = match attr.get("expires") {
            None | Some(Value::None) => None,
            v => Some(ms(v)?),
        };
        Ok(TokenRecord {
            hash: hash.to_owned(),
            user: string("user")?,
            name: string("name")?,
            created_ms: ms(attr.get("created"))?,
            expires_ms,
        })
    }

    fn info(&self) -> TokenInfo {
        TokenInfo {
            user: self.user.clone(),
            name: self.name.clone(),
            created_ms: self.created_ms,
            expires_ms: self.expires_ms,
        }
    }

    pub(crate) fn expired(&self, now: u64) -> bool {
        self.expires_ms.is_some_and(|e| e <= now)
    }
}

fn int(ms: u64) -> Value {
    Value::Int(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// What a password check found.
pub(crate) struct Checked {
    pub user: UserRecord,
    /// The hash's parameters aren't the current ones.
    pub rehash: bool,
}

impl Checked {
    /// The hash that was checked.
    pub(crate) fn phc(&self) -> &str {
        &self.user.phc
    }
}

/// The users, grants and API tokens of a store ([`Store::users`]).
///
/// Every change is one commit to the system namespace (ADR 0043): all or
/// nothing, durable per the store's fsync policy when it returns, and
/// serialized with the other changes of this store. Reads see the last
/// commit.
pub struct Users<'a, F: LogFs + Send + Sync + 'static>
where
    F::File: Send,
{
    store: &'a Store<F>,
    params: HashParams,
}

impl<F: LogFs + Clone + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// The store's users, grants and API tokens (ADR 0043), hashing new
    /// passwords with the default parameters.
    pub fn users(&self) -> Users<'_, F> {
        Users { store: self, params: HashParams::default() }
    }
}

impl<'a, F: LogFs + Clone + Send + Sync + 'static> Users<'a, F>
where
    F::File: Send,
{
    /// Hash new passwords with `params` instead of the defaults.
    pub fn with_params(mut self, params: HashParams) -> Self {
        self.params = params;
        self
    }

    /// The system namespace, if the store has one, with its format
    /// checked.
    fn ns(&self) -> Result<Option<Ns<'a, F>>, Error> {
        let Some(ns) = self.store.system_namespace(false)? else { return Ok(None) };
        let format = ns.node(FORMAT_NODE).and_then(|n| match n.attr.get("format") {
            Some(Value::Int(f)) => Some(*f),
            _ => None,
        });
        match format {
            Some(AUTH_FORMAT) => Ok(Some(ns)),
            // Created, but the first user's commit didn't happen (a crash
            // in between): no users yet
            None if ns.seq() == 0 => Ok(None),
            Some(f) => Err(Error::new(
                Code::Corrupt,
                format!("the store's users are in format {}, this version reads format {}", f, AUTH_FORMAT),
            )),
            None => Err(corrupt("format record")),
        }
    }

    fn names(&self) -> HashMap<u64, String> {
        self.store.namespaces().into_iter().map(|i| (i.id, i.name.to_string())).collect()
    }

    pub(crate) fn record(&self, name: &str) -> Result<Option<UserRecord>, Error> {
        let Some(ns) = self.ns()? else { return Ok(None) };
        ns.node(&format!("{}{}", USER_NODE, name)).map(|n| UserRecord::from_attrs(&n.attr)).transpose()
    }

    fn records(&self) -> Result<Vec<UserRecord>, Error> {
        self.nodes(USER_NODE, |_, attr| UserRecord::from_attrs(attr))
    }

    fn token_records(&self) -> Result<Vec<TokenRecord>, Error> {
        self.nodes(TOKEN_NODE, TokenRecord::from_attrs)
    }

    /// Every node whose id starts with `prefix`, read by `read` (with the
    /// rest of its id).
    fn nodes<T>(&self, prefix: &str, read: impl Fn(&str, &Attrs) -> Result<T, Error>) -> Result<Vec<T>, Error> {
        let Some(ns) = self.ns()? else { return Ok(Vec::new()) };
        ns.read(|n| {
            let g = n.graph();
            let mut out = Vec::new();
            for ix in n.node_handles() {
                if let Some(node) = g.node(ix)
                    && let Some(rest) = node.id().strip_prefix(prefix)
                {
                    out.push(read(rest, &node.data.attr)?);
                }
            }
            Ok(out)
        })
    }

    /// Commit `mutations` to the system namespace (creating it, and the
    /// format record, with the first user).
    fn commit(&self, mut mutations: Vec<Mutation>) -> Result<(), Error> {
        let ns = match self.ns()? {
            Some(ns) => ns,
            None => {
                let ns = self
                    .store
                    .system_namespace(true)?
                    .ok_or_else(|| Error::internal("the system namespace wasn't created"))?;
                mutations.insert(
                    0,
                    Mutation::UpsertNode {
                        id: FORMAT_NODE.into(),
                        labels: vec!["AuthFormat".into()],
                        attr: Attrs::from([("format".to_owned(), Value::Int(AUTH_FORMAT))]),
                        meta: Attrs::new(),
                        expected_version: None,
                    },
                );
                ns
            }
        };
        ns.commit_with(&mutations, &CommitOptions::default())?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.store.auth_lock()
    }

    fn existing(&self, name: &str) -> Result<UserRecord, Error> {
        self.record(name)?.ok_or_else(|| Error::not_found(format!("no user '{}'", name)))
    }

    /// Whether the store has any user.
    pub fn exist(&self) -> Result<bool, Error> {
        Ok(!self.records()?.is_empty())
    }

    /// Every user, by name.
    pub fn list(&self) -> Result<Vec<UserInfo>, Error> {
        let names = self.names();
        let mut users: Vec<UserInfo> = self.records()?.iter().map(|u| u.info(&names)).collect();
        users.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(users)
    }

    /// The user `name`, if it exists.
    pub fn get(&self, name: &str) -> Result<Option<UserInfo>, Error> {
        Ok(self.record(name)?.map(|u| u.info(&self.names())))
    }

    /// Create the user `name`. Errors: `invalid_argument` (name, password),
    /// `conflict` (it exists).
    pub fn create(&self, name: &str, password: &Secret, admin: bool) -> Result<UserInfo, Error> {
        check_user_name(name)?;
        check_password(password)?;
        let phc = self.params.hash(password)?;
        let _guard = self.lock();
        if self.record(name)?.is_some() {
            return Err(Error::new(Code::Conflict, format!("user '{}' exists already", name)));
        }
        let user = UserRecord {
            name: name.to_owned(),
            phc,
            admin,
            epoch: 1,
            grants: BTreeMap::new(),
            created: i64::try_from(now_ms()).unwrap_or(i64::MAX),
        };
        self.commit(vec![user.upsert()])?;
        Ok(user.info(&self.names()))
    }

    /// Set `name`'s password; with `current`, only if that is its password
    /// now (`unauthenticated` otherwise). Ends its sessions (the epoch
    /// grows).
    pub fn set_password(&self, name: &str, password: &Secret, current: Option<&Secret>) -> Result<(), Error> {
        check_password(password)?;
        let phc = self.params.hash(password)?;
        let _guard = self.lock();
        let mut user = self.existing(name)?;
        if let Some(current) = current
            && !self.params.verify(current, &user.phc)?.0
        {
            return Err(Error::new(Code::Unauthenticated, "the current password is wrong"));
        }
        user.phc = phc;
        user.epoch = user.epoch.saturating_add(1);
        self.commit(vec![user.upsert()])
    }

    /// Replace a hash with old parameters by one with the current ones, if
    /// the user's password is still the one that was just checked.
    pub(crate) fn rehash(&self, name: &str, password: &Secret, old_phc: &str) -> Result<(), Error> {
        let phc = self.params.hash(password)?;
        let _guard = self.lock();
        let Some(mut user) = self.record(name)? else { return Ok(()) };
        if user.phc != old_phc {
            return Ok(());
        }
        user.phc = phc;
        self.commit(vec![user.upsert()])
    }

    fn other_admins(&self, name: &str) -> Result<usize, Error> {
        Ok(self.records()?.iter().filter(|u| u.admin && u.name != name).count())
    }

    /// Delete `name`, with its grants and API tokens. The last admin can't
    /// be deleted (`invalid_argument`).
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        let _guard = self.lock();
        let user = self.existing(name)?;
        if user.admin && self.other_admins(name)? == 0 {
            return Err(Error::invalid(format!("user '{}' is the last admin: make another one first", name)));
        }
        let mut mutations = vec![Mutation::DeleteNode { id: format!("{}{}", USER_NODE, name), expected_version: None }];
        for token in self.token_records()?.into_iter().filter(|t| t.user == name) {
            mutations
                .push(Mutation::DeleteNode { id: format!("{}{}", TOKEN_NODE, token.hash), expected_version: None });
        }
        self.commit(mutations)
    }

    /// Make `name` a server-wide admin or not. The last admin stays one
    /// (`invalid_argument`).
    pub fn set_admin(&self, name: &str, admin: bool) -> Result<UserInfo, Error> {
        let _guard = self.lock();
        let mut user = self.existing(name)?;
        if user.admin && !admin && self.other_admins(name)? == 0 {
            return Err(Error::invalid(format!("user '{}' is the last admin: make another one first", name)));
        }
        user.admin = admin;
        self.commit(vec![user.upsert()])?;
        Ok(user.info(&self.names()))
    }

    /// Give `name` `role` on `namespace` (which must exist).
    pub fn grant(&self, name: &str, namespace: &str, role: Role) -> Result<UserInfo, Error> {
        let id = self
            .store
            .namespace(namespace)
            .map_err(|_| Error::not_found(format!("no namespace '{}'", namespace)))?
            .id();
        let _guard = self.lock();
        let mut user = self.existing(name)?;
        user.grants.insert(id, role);
        self.commit(vec![user.upsert()])?;
        Ok(user.info(&self.names()))
    }

    /// Take `name`'s role on `namespace` away (fine if it has none).
    pub fn revoke(&self, name: &str, namespace: &str) -> Result<UserInfo, Error> {
        let id = self
            .store
            .namespace(namespace)
            .map_err(|_| Error::not_found(format!("no namespace '{}'", namespace)))?
            .id();
        let _guard = self.lock();
        let mut user = self.existing(name)?;
        if user.grants.remove(&id).is_some() {
            self.commit(vec![user.upsert()])?;
        }
        Ok(user.info(&self.names()))
    }

    /// Check `name`'s password. Unknown users take as long as wrong
    /// passwords (a hash is verified either way). Errors:
    /// `unauthenticated` for both, with the same message.
    pub(crate) fn check(&self, name: &str, password: &Secret) -> Result<Checked, Error> {
        let wrong = || Error::new(Code::Unauthenticated, "wrong user or password");
        let user = match self.record(name) {
            Ok(user) => user,
            // An invalid name is just an unknown user to a login
            Err(e) if e.code() == Code::Corrupt => return Err(e),
            Err(_) => None,
        };
        match user {
            Some(user) => {
                let (ok, rehash) = self.params.verify(password, &user.phc)?;
                if ok { Ok(Checked { user, rehash }) } else { Err(wrong()) }
            }
            None => {
                let _ = self.params.verify(password, dummy_hash(&self.params));
                Err(wrong())
            }
        }
    }

    /// Whether `password` is `name`'s password (`false` for an unknown
    /// user). Takes as long as a login's check (argon2).
    pub fn verify(&self, name: &str, password: &Secret) -> Result<bool, Error> {
        match self.check(name, password) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == Code::Unauthenticated => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Make an API token for `user` named `name`, expiring after
    /// `expires_in`. The secret is in the answer only.
    pub fn create_token(&self, user: &str, name: &str, expires_in: Option<Duration>) -> Result<NewToken, Error> {
        check_token_name(name)?;
        let token = new_token()?;
        let hash = token_hash(&token);
        let _guard = self.lock();
        self.existing(user)?;
        if self.token_records()?.iter().any(|t| t.user == user && t.name == name) {
            return Err(Error::new(Code::Conflict, format!("user '{}' has a token '{}' already", user, name)));
        }
        let created_ms = now_ms();
        let expires_ms =
            expires_in.map(|d| created_ms.saturating_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)));
        let mut attr = Attrs::from([
            ("user".to_owned(), Value::String(user.to_owned())),
            ("name".to_owned(), Value::String(name.to_owned())),
            ("created".to_owned(), int(created_ms)),
        ]);
        if let Some(e) = expires_ms {
            attr.insert("expires".into(), int(e));
        }
        self.commit(vec![Mutation::UpsertNode {
            id: format!("{}{}", TOKEN_NODE, hash),
            labels: vec!["Token".into()],
            attr,
            meta: Attrs::new(),
            expected_version: Some(0),
        }])?;
        let info = TokenInfo { user: user.to_owned(), name: name.to_owned(), created_ms, expires_ms };
        Ok(NewToken { info, token })
    }

    /// Revoke `user`'s token `name`.
    pub fn revoke_token(&self, user: &str, name: &str) -> Result<(), Error> {
        let _guard = self.lock();
        let token = self
            .token_records()?
            .into_iter()
            .find(|t| t.user == user && t.name == name)
            .ok_or_else(|| Error::not_found(format!("user '{}' has no token '{}'", user, name)))?;
        self.commit(vec![Mutation::DeleteNode { id: format!("{}{}", TOKEN_NODE, token.hash), expected_version: None }])
    }

    /// `user`'s API tokens, by name.
    pub fn tokens(&self, user: &str) -> Result<Vec<TokenInfo>, Error> {
        self.existing(user)?;
        let mut list: Vec<TokenInfo> =
            self.token_records()?.iter().filter(|t| t.user == user).map(TokenRecord::info).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    /// The API token with this hash, if there is one.
    pub(crate) fn token(&self, hash: &str) -> Result<Option<TokenRecord>, Error> {
        let Some(ns) = self.ns()? else { return Ok(None) };
        ns.node(&format!("{}{}", TOKEN_NODE, hash)).map(|n| TokenRecord::from_attrs(hash, &n.attr)).transpose()
    }

    /// The user's grants by namespace name, for a principal.
    pub(crate) fn info_of(&self, user: &UserRecord) -> UserInfo {
        user.info(&self.names())
    }
}

/// A hash to verify against for unknown users, so they take as long as
/// known ones.
fn dummy_hash(params: &HashParams) -> &'static str {
    static DUMMY: OnceLock<(HashParams, String)> = OnceLock::new();
    let (made_with, hash) =
        DUMMY.get_or_init(|| (*params, params.hash(&Secret::new("not a password: unknown user")).unwrap_or_default()));
    if made_with == params { hash } else { DUMMY_DEFAULT }
}

/// A fixed argon2id hash with the default parameters, for unknown users
/// when the parameters changed since [`dummy_hash`] made its own.
const DUMMY_DEFAULT: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZSByYW5kb20gc2FsdA$0Mz6/u2vf2J8HfOp0ZPwSEc8uN1Q8GAVPRXg+oHz0lM";

/// The server's login sessions, in memory: a session token's hash, its
/// user, the user's password epoch at login, and its expiry.
#[derive(Debug, Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, SessionEntry>>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionEntry {
    pub user: String,
    pub epoch: i64,
    pub expires_ms: u64,
}

impl Sessions {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Remember a session, forgetting expired ones (and, above `max`, the
    /// one that expires first).
    pub(crate) fn insert(&self, hash: String, entry: SessionEntry, max: usize) {
        let mut map = self.map();
        if map.len() >= max {
            let now = now_ms();
            map.retain(|_, e| e.expires_ms > now);
            while map.len() >= max.max(1) {
                let Some(first) = map.iter().min_by_key(|(_, e)| e.expires_ms).map(|(k, _)| k.clone()) else { break };
                map.remove(&first);
            }
        }
        map.insert(hash, entry);
    }

    pub(crate) fn get(&self, hash: &str) -> Option<SessionEntry> {
        let mut map = self.map();
        let entry = map.get(hash)?.clone();
        if entry.expires_ms <= now_ms() {
            map.remove(hash);
            return None;
        }
        Some(entry)
    }

    pub(crate) fn remove(&self, hash: &str) {
        self.map().remove(hash);
    }

    /// End every session of `user`.
    pub(crate) fn remove_user(&self, user: &str) {
        self.map().retain(|_, e| e.user != user);
    }

    /// Sessions held now (expired ones not yet forgotten included).
    pub fn len(&self) -> usize {
        self.map().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a failed-login entry counts.
#[derive(Clone, Copy, Debug)]
struct Failures {
    count: u32,
    since: Instant,
    last: Instant,
}

/// Who failed to log in: a user name or a client address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Who {
    User(String),
    Address(IpAddr),
}

/// The slowdown of failed logins: at most `max_failures` per user and per
/// client address within a window, then refused until the window has
/// passed. A bounded table: above `table_size` entries the least recently
/// seen is forgotten.
#[derive(Debug, Default)]
pub struct Throttle {
    table: Mutex<HashMap<Who, Failures>>,
}

impl Throttle {
    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<Who, Failures>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How long `who` must wait before trying again, if it must.
    pub(crate) fn wait(&self, who: &[Who], settings: &AuthSettings) -> Option<Duration> {
        let now = Instant::now();
        let table = self.table();
        who.iter()
            .filter_map(|w| table.get(w))
            .filter(|f| f.count >= settings.max_failures)
            .filter_map(|f| settings.failure_window.checked_sub(now.duration_since(f.since)))
            .filter(|left| !left.is_zero())
            .max()
    }

    pub(crate) fn failed(&self, who: &[Who], settings: &AuthSettings) {
        let now = Instant::now();
        let mut table = self.table();
        for w in who {
            if !table.contains_key(w) && table.len() >= settings.table_size.max(1) {
                let oldest = table.iter().min_by_key(|(_, f)| f.last).map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    table.remove(&oldest);
                }
            }
            let entry = table.entry(w.clone()).or_insert(Failures { count: 0, since: now, last: now });
            if now.duration_since(entry.since) >= settings.failure_window {
                *entry = Failures { count: 0, since: now, last: now };
            }
            entry.count = entry.count.saturating_add(1);
            entry.last = now;
        }
    }

    pub(crate) fn succeeded(&self, who: &Who) {
        self.table().remove(who);
    }

    /// Entries held now.
    pub fn len(&self) -> usize {
        self.table().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap parameters, so the tests don't spend their time hashing.
    pub(crate) const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

    #[test]
    fn hashes_record_their_parameters_and_verify() {
        let pw = Secret::new("correct horse battery");
        let phc = FAST.hash(&pw).expect("hash");
        assert!(phc.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{}", phc);
        assert!(!phc.contains("correct horse"));
        assert_eq!(FAST.verify(&pw, &phc).expect("verify"), (true, false));
        assert_eq!(FAST.verify(&Secret::new("wrong password"), &phc).expect("verify"), (false, false));
        // Other parameters: still verifies, and asks to be replaced
        let stronger = HashParams { memory_kib: 128, ..FAST };
        assert_eq!(stronger.verify(&pw, &phc).expect("verify"), (true, true));
        // Two hashes of one password differ (salt)
        assert_ne!(phc, FAST.hash(&pw).expect("hash"));
        assert_eq!(FAST.verify(&pw, "not a hash").expect_err("corrupt").code(), Code::Corrupt);
        // The fixed fallback is a valid hash
        assert!(PasswordHash::new(DUMMY_DEFAULT).is_ok());
    }

    #[test]
    fn tokens_are_256_random_bits_and_stored_as_hashes() {
        let a = new_token().expect("token");
        let b = new_token().expect("token");
        assert_ne!(a, b);
        assert!(a.expose().starts_with(TOKEN_PREFIX));
        assert_eq!(a.expose().len(), TOKEN_PREFIX.len() + 64);
        let h = token_hash(&a);
        assert_eq!(h.len(), 64);
        assert!(!h.contains(&a.expose()[TOKEN_PREFIX.len()..]));
        assert_eq!(h, token_hash(&a));
    }

    #[test]
    fn the_throttle_refuses_after_too_many_failures_and_forgets() {
        let settings = AuthSettings {
            max_failures: 3,
            failure_window: Duration::from_millis(200),
            table_size: 2,
            ..AuthSettings::default()
        };
        let t = Throttle::default();
        let ann = [Who::User("ann".into())];
        for _ in 0..3 {
            assert_eq!(t.wait(&ann, &settings), None);
            t.failed(&ann, &settings);
        }
        assert!(t.wait(&ann, &settings).is_some());
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(t.wait(&ann, &settings), None);
        // Bounded: a third key evicts the least recently seen
        t.failed(&[Who::User("bob".into())], &settings);
        t.failed(&[Who::Address("127.0.0.1".parse().expect("ip"))], &settings);
        assert_eq!(t.len(), 2);
        t.succeeded(&Who::User("bob".into()));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn sessions_expire_and_are_bounded() {
        let s = Sessions::default();
        let entry = |user: &str, expires_ms| SessionEntry { user: user.into(), epoch: 1, expires_ms };
        s.insert("a".into(), entry("ann", now_ms() + 60_000), 2);
        s.insert("b".into(), entry("bob", now_ms() + 1), 2);
        s.insert("c".into(), entry("ann", now_ms() + 120_000), 2);
        assert_eq!(s.len(), 2);
        assert!(s.get("b").is_none(), "the one expiring first went");
        s.insert("d".into(), entry("cy", 1), 10);
        assert!(s.get("d").is_none(), "expired");
        s.remove_user("ann");
        assert!(s.is_empty());
    }
}
