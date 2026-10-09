//! Users, grants and API tokens in the system namespace (step 15a, ADR
//! 0043), through `Store::users` and `Embedded`'s `Accounts` and
//! `Authenticate`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use iwdb::auth::{AuthSettings, HashParams};
use iwdb::{Embedded, QueryConfig, RestoreSources, RestoreTarget, Role, Secret, Store, StoreOptions};
use iwdb_query::exec::block_on;
use iwdb_query::{Accounts, Authenticate, Code, Database};

const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

fn open(dir: &std::path::Path) -> Store {
    Store::open(dir, StoreOptions::default()).unwrap()
}

fn pw(s: &str) -> Secret {
    Secret::new(s)
}

#[test]
fn a_store_without_users_has_no_system_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    assert!(!store.users().exist().unwrap());
    assert!(store.users().list().unwrap().is_empty());
    assert_eq!(store.namespaces().len(), 1);
    store.close().unwrap();
    assert!(iwdb::verify(dir.path()).unwrap().namespaces.iter().all(|n| n.name.as_str() != "_system"));
}

#[test]
fn users_are_durable_and_their_namespace_is_hidden_and_reserved() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let users = store.users().with_params(FAST);
    users.create("root", &pw("root-password"), true).unwrap();
    store.create_namespace("social", None).unwrap();
    users.create("ann", &pw("ann-password"), false).unwrap();
    let ann = users.grant("ann", "social", Role::Write).unwrap();
    assert_eq!(ann.grants.get("social"), Some(&Role::Write));
    // Hidden from the namespaces, the status, and every public method
    let names: Vec<String> = store.namespaces().into_iter().map(|n| n.name.to_string()).collect();
    assert_eq!(names, ["default", "social"]);
    assert!(store.status().namespaces.iter().all(|n| n.name != "_system"));
    assert!(store.namespace("_system").is_err());
    for e in [store.create_namespace("_system", None).unwrap_err(), store.drop_namespace("_system", None).unwrap_err()]
    {
        assert!(e.to_string().contains("reserved"), "{}", e);
    }
    store.close().unwrap();

    let store = open(dir.path());
    let users = store.users();
    assert_eq!(users.list().unwrap().iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["ann", "root"]);
    assert!(users.verify("ann", &pw("ann-password")).unwrap());
    assert!(!users.verify("ann", &pw("ann-password!")).unwrap());
    assert!(!users.verify("nobody", &pw("ann-password")).unwrap());
    assert_eq!(users.get("ann").unwrap().unwrap().grants.get("social"), Some(&Role::Write));
    // The hash, not the password, is stored
    let files = walk(dir.path());
    assert!(files.iter().all(|bytes| !contains(bytes, b"ann-password")));
    store.close().unwrap();
    assert!(iwdb::verify(dir.path()).unwrap().is_ok());
}

#[test]
fn grants_end_with_their_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let users = store.users().with_params(FAST);
    users.create("ann", &pw("ann-password"), true).unwrap();
    store.create_namespace("social", None).unwrap();
    users.grant("ann", "social", Role::Read).unwrap();
    store.drop_namespace("social", None).unwrap();
    store.create_namespace("social", None).unwrap();
    assert!(users.get("ann").unwrap().unwrap().grants.is_empty(), "a new namespace of the same name");
    assert_eq!(users.grant("ann", "nope", Role::Read).unwrap_err().code(), Code::NotFound);
}

#[test]
fn invalid_requests_and_the_last_admin() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let users = store.users().with_params(FAST);
    assert_eq!(users.create("ann", &pw("short"), false).unwrap_err().code(), Code::InvalidArgument);
    for bad in ["", "_x", "a b", "ä"] {
        assert_eq!(users.create(bad, &pw("long enough"), false).unwrap_err().code(), Code::InvalidArgument);
    }
    users.create("root", &pw("root-password"), true).unwrap();
    assert_eq!(users.create("root", &pw("root-password"), true).unwrap_err().code(), Code::Conflict);
    assert_eq!(users.set_admin("root", false).unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(users.delete("root").unwrap_err().code(), Code::InvalidArgument);
    users.create("ann", &pw("ann-password"), true).unwrap();
    users.set_admin("root", false).unwrap();
    users.delete("root").unwrap();
    assert_eq!(users.delete("root").unwrap_err().code(), Code::NotFound);
    let e = users.set_password("ann", &pw("new-password"), Some(&pw("wrong-password"))).unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    assert!(!e.message().contains("wrong-password"));
    users.set_password("ann", &pw("new-password"), Some(&pw("ann-password"))).unwrap();
    assert!(users.verify("ann", &pw("new-password")).unwrap());
}

#[test]
fn backups_and_restores_carry_the_users() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.users().with_params(FAST).create("ann", &pw("ann-password"), true).unwrap();
    let backup = dir.path().with_extension("backup");
    store.backup(&backup).unwrap();
    store.close().unwrap();
    let restored = dir.path().with_extension("restored");
    let sources = RestoreSources { backup: Some(backup.clone()), archive: None };
    iwdb::restore(&restored, &sources, RestoreTarget::Latest).unwrap();
    let store = open(&restored);
    assert!(store.users().verify("ann", &pw("ann-password")).unwrap());
    store.close().unwrap();
    for d in [backup, restored] {
        std::fs::remove_dir_all(d).unwrap();
    }
}

fn embedded(dir: &std::path::Path, settings: AuthSettings) -> Embedded {
    Embedded::new(open(dir), QueryConfig::default()).unwrap().with_auth(settings)
}

fn settings() -> AuthSettings {
    AuthSettings { hash: FAST, ..AuthSettings::default() }
}

#[test]
fn logins_give_sessions_that_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = embedded(dir.path(), AuthSettings { session_lifetime: Duration::from_millis(300), ..settings() });
    block_on(db.create_user("ann", pw("ann-password"), false)).unwrap();
    block_on(db.grant("ann", "default", Role::Read)).unwrap();
    let session = block_on(db.login("ann", pw("ann-password"), None)).unwrap();
    assert!(session.token.expose().starts_with("iwdb_"));
    let principal = block_on(db.authenticate(&session.token)).unwrap();
    assert_eq!((principal.user.as_str(), principal.admin), ("ann", false));
    assert_eq!(principal.role("default"), Some(Role::Read));
    // Logout
    block_on(db.logout(&session.token)).unwrap();
    assert_eq!(block_on(db.authenticate(&session.token)).unwrap_err().code(), Code::Unauthenticated);
    // Expiry
    let session = block_on(db.login("ann", pw("ann-password"), None)).unwrap();
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(block_on(db.authenticate(&session.token)).unwrap_err().code(), Code::Unauthenticated);
    // A password change ends sessions, also those another process made
    let session = block_on(db.login("ann", pw("ann-password"), None)).unwrap();
    db.store().users().with_params(FAST).set_password("ann", &pw("new-password"), None).unwrap();
    assert_eq!(block_on(db.authenticate(&session.token)).unwrap_err().code(), Code::Unauthenticated);
    // Deleting the user does too
    let session = block_on(db.login("ann", pw("new-password"), None)).unwrap();
    block_on(db.create_user("root", pw("root-password"), true)).unwrap();
    block_on(db.delete_user("ann")).unwrap();
    assert_eq!(block_on(db.authenticate(&session.token)).unwrap_err().code(), Code::Unauthenticated);
    assert_eq!(block_on(db.authenticate(&pw("iwdb_nonsense"))).unwrap_err().code(), Code::Unauthenticated);
}

#[test]
fn failed_logins_are_slowed_down_per_user_and_address() {
    let dir = tempfile::tempdir().unwrap();
    let db = embedded(
        dir.path(),
        AuthSettings { max_failures: 3, failure_window: Duration::from_millis(500), ..settings() },
    );
    block_on(db.create_user("ann", pw("ann-password"), false)).unwrap();
    let here = Some("10.0.0.1".parse().unwrap());
    for _ in 0..3 {
        let e = block_on(db.login("ann", pw("wrong-password"), here)).unwrap_err();
        assert_eq!((e.code(), e.message()), (Code::Unauthenticated, "wrong user or password"));
    }
    // Locked: even the right password is refused, and so are other users
    // from the same address
    let e = block_on(db.login("ann", pw("ann-password"), Some("10.0.0.2".parse().unwrap()))).unwrap_err();
    assert!(e.message().contains("too many failed logins"), "{}", e);
    block_on(db.create_user("bob", pw("bob-password"), false)).unwrap();
    assert!(block_on(db.login("bob", pw("bob-password"), here)).is_err());
    assert!(block_on(db.login("bob", pw("bob-password"), None)).is_ok());
    // Unknown users fail the same way
    let e = block_on(db.login("nobody", pw("whatever-pw"), None)).unwrap_err();
    assert_eq!(e.message(), "wrong user or password");
    std::thread::sleep(Duration::from_millis(550));
    assert!(block_on(db.login("ann", pw("ann-password"), here)).is_ok());
}

#[test]
fn api_tokens_authenticate_until_revoked_or_expired() {
    let dir = tempfile::tempdir().unwrap();
    let db = embedded(dir.path(), settings());
    block_on(db.create_user("ann", pw("ann-password"), true)).unwrap();
    let token = block_on(db.create_token("ann", "ci", None)).unwrap();
    assert_eq!(token.info.expires_ms, None);
    assert_eq!(block_on(db.authenticate(&token.token)).unwrap().user, "ann");
    assert_eq!(block_on(db.create_token("ann", "ci", None)).unwrap_err().code(), Code::Conflict);
    let short = block_on(db.create_token("ann", "short", Some(Duration::from_millis(100)))).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(block_on(db.authenticate(&short.token)).unwrap_err().code(), Code::Unauthenticated);
    let names: Vec<String> = block_on(db.tokens("ann")).unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(names, ["ci", "short"]);
    // Survives a password change and a restart
    block_on(db.set_password("ann", pw("new-password"), None)).unwrap();
    db.close().unwrap();
    let db = embedded(dir.path(), settings());
    assert_eq!(block_on(db.authenticate(&token.token)).unwrap().user, "ann");
    block_on(db.revoke_token("ann", "ci")).unwrap();
    assert_eq!(block_on(db.authenticate(&token.token)).unwrap_err().code(), Code::Unauthenticated);
    // The namespace isn't reachable through the trait either
    assert_eq!(block_on(db.namespace_status("_system")).unwrap_err().code(), Code::NotFound);
}

fn walk(dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else if path.file_name().is_some_and(|name| name == "LOCK") {
            // Locked while a store is open: unreadable on Windows (ADR 0058)
        } else {
            out.push(std::fs::read(&path).unwrap());
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
