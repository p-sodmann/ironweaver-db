//! Compatibility fixtures of the users' records (steps 15a and 15d, design
//! rule 4, `documentation/formats/auth.md`): `tests/fixtures/auth-v<N>/` is
//! a store whose system namespace holds auth format N, changed before and
//! after a checkpoint of it, so the records are in the checkpoint and in
//! the WAL. Every later version must read each: the users, their admin
//! flags, grants and limits, the namespaces' limits, which password each
//! hash accepts, and the API token. `generate_fixture` writes the current
//! format's:
//!
//! ```text
//! cargo test -p iwdb --test auth_fixture -- --ignored generate_fixture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::auth::{AUTH_FORMAT, HashParams};
use iwdb::{Embedded, QueryConfig, Role, Secret, Store, StoreOptions};
use iwdb_query::exec::block_on;
use iwdb_query::{Accounts, Authenticate, ClientLimits, NamespaceLimits};

/// Cheap parameters for most users; `root` has the defaults, so the fixture
/// holds both.
const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

fn fixture(format: i64) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/auth-v{}", format))
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
    // Git keeps no empty directories
    if to.file_name().is_some_and(|n| n.len() == 20) {
        fs::create_dir_all(to.join("checkpoints")).unwrap();
        fs::create_dir_all(to.join("wal")).unwrap();
    }
}

/// What a store's users look like, as text (limits only where set, so
/// format 1's description has none).
fn describe(store: &Store) -> String {
    let users = store.users();
    let mut out = String::new();
    for user in users.list().unwrap() {
        out.push_str(&format!("user {} admin={} grants={:?}", user.name, user.admin, user.grants));
        if !user.limits.is_unset() {
            out.push_str(&format!(" limits={:?}", user.limits.named()));
        }
        out.push('\n');
        for token in users.tokens(&user.name).unwrap() {
            out.push_str(&format!("token {} {} expires={:?}\n", token.user, token.name, token.expires_ms.is_some()));
        }
    }
    for ns in users.namespace_limits().unwrap() {
        out.push_str(&format!("limits {} {:?}\n", ns.namespace, ns.limits.named()));
    }
    out
}

/// Writes the current format's fixture, if it doesn't exist yet: `root`
/// (admin), `ann` (write on `social`, an API token `ci`, limits), `bob`
/// (made, then deleted) and `cy` (read on default, then revoked, then its
/// password changed, then limits), limits on `social` (set, then changed)
/// and on `gone` (dropped since), with a checkpoint of the system namespace
/// in the middle. `expected.txt` is the description, the passwords as
/// `password <user> <password>` and the token as `token-secret <secret>`.
#[test]
#[ignore = "writes the fixture"]
fn generate_fixture() {
    let dir = fixture(AUTH_FORMAT);
    if dir.exists() {
        panic!("{} exists; remove it to write it again", dir.display());
    }
    let path = dir.join("store");
    // No checkpoint on close: the last changes stay in the WAL
    let mut options = StoreOptions::default();
    options.checkpoint.on_close = false;
    options.checkpoint.background = false;
    let store = Store::open(&path, options).unwrap();
    store.create_namespace("social", None).unwrap();
    store.create_namespace("gone", None).unwrap();
    store.users().create("root", &Secret::new("root-password"), true).unwrap();
    let users = store.users().with_params(FAST);
    users.create("ann", &Secret::new("ann-password"), false).unwrap();
    users.grant("ann", "social", Role::Write).unwrap();
    let ann = ClientLimits { requests_per_second: Some(50), max_running: Some(4), ..ClientLimits::default() };
    users.set_limits("ann", ann).unwrap();
    users.create("bob", &Secret::new("bob-password"), false).unwrap();
    users.create("cy", &Secret::new("cy-password-1"), false).unwrap();
    users.grant("cy", "default", Role::Read).unwrap();
    users.set_namespace_limits("social", NamespaceLimits { max_running: Some(8), max_memory_bytes: None }).unwrap();
    users.set_namespace_limits("gone", NamespaceLimits { max_running: Some(1), max_memory_bytes: None }).unwrap();
    // The records so far into a checkpoint of the system namespace
    store.checkpoint_all().unwrap();
    let token = users.create_token("ann", "ci", None).unwrap();
    users.delete("bob").unwrap();
    users.revoke("cy", "default").unwrap();
    users.set_password("cy", &Secret::new("cy-password-2"), None).unwrap();
    let cy =
        ClientLimits { max_visited: Some(1000), timeout_ms: Some(5000), burst: Some(0), ..ClientLimits::default() };
    users.set_limits("cy", cy).unwrap();
    store.drop_namespace("gone", None).unwrap();
    users
        .set_namespace_limits("social", NamespaceLimits { max_running: Some(16), max_memory_bytes: Some(1 << 30) })
        .unwrap();
    let mut expected = describe(&store);
    for (user, pw) in [("root", "root-password"), ("ann", "ann-password"), ("cy", "cy-password-2")] {
        expected.push_str(&format!("password {} {}\n", user, pw));
    }
    expected.push_str(&format!("token-secret {}\n", token.token.expose()));
    store.close().unwrap();
    let _ = fs::remove_file(path.join("LOCK"));
    fs::write(dir.join("expected.txt"), expected).unwrap();
}

/// Open a copy of format `format`'s fixture in `work`, check it against its
/// `expected.txt`, and return it with that file.
fn open_fixture(format: i64, work: &Path) -> (Store, String) {
    let expected = fs::read_to_string(fixture(format).join("expected.txt")).unwrap();
    copy_dir(&fixture(format).join("store"), work);
    let store = Store::open(work, StoreOptions::default()).unwrap();
    assert_eq!(store.users().format().unwrap(), format);
    let description: String = expected
        .lines()
        .filter(|l| l.starts_with("user ") || l.starts_with("token ") || l.starts_with("limits "))
        .map(|l| format!("{}\n", l))
        .collect();
    assert_eq!(describe(&store), description);
    for line in expected.lines().filter_map(|l| l.strip_prefix("password ")) {
        let (user, pw) = line.split_once(' ').unwrap();
        assert!(store.users().verify(user, &Secret::new(pw)).unwrap(), "{}", user);
    }
    assert!(!store.users().verify("cy", &Secret::new("cy-password-1")).unwrap());
    assert!(store.users().exist().unwrap());
    (store, expected)
}

/// The fixture's API token, as `ann`'s principal.
fn ann(db: &Embedded, expected: &str) -> iwdb_query::Principal {
    let secret = expected.lines().find_map(|l| l.strip_prefix("token-secret ")).unwrap().to_owned();
    let principal = block_on(db.authenticate(&Secret::new(secret))).unwrap();
    assert_eq!((principal.user.as_str(), principal.role("social")), ("ann", Some(Role::Write)));
    principal
}

#[test]
fn reads_auth_format_1() {
    let work = tempfile::tempdir().unwrap();
    let (store, expected) = open_fixture(1, work.path());
    // Format 1 has no limits
    assert!(store.users().list().unwrap().iter().all(|u| u.limits.is_unset()));
    assert!(store.users().namespace_limits().unwrap().is_empty());
    let db = Embedded::new(store, QueryConfig::default()).unwrap();
    assert!(ann(&db, &expected).limits.is_unset());
    // And it goes on: a login (which changes nothing), then a change, which
    // moves the records to the current format in the same commit
    block_on(db.login("root", Secret::new("root-password"), None)).unwrap();
    assert_eq!(db.store().users().format().unwrap(), 1);
    block_on(db.grant("cy", "social", Role::Admin)).unwrap();
    assert_eq!(db.store().users().format().unwrap(), AUTH_FORMAT);
    let limits = ClientLimits { max_running: Some(2), ..ClientLimits::default() };
    block_on(db.set_user_limits("cy", limits)).unwrap();
    db.close().unwrap();
    assert!(iwdb::verify(work.path()).unwrap().is_ok());
    let store = Store::open(work.path(), StoreOptions::default()).unwrap();
    assert_eq!(store.users().format().unwrap(), AUTH_FORMAT);
    assert_eq!(store.users().get("cy").unwrap().unwrap().limits, limits);
}

#[test]
fn reads_auth_format_2() {
    let work = tempfile::tempdir().unwrap();
    let (store, expected) = open_fixture(2, work.path());
    let ann_limits = ClientLimits { requests_per_second: Some(50), max_running: Some(4), ..ClientLimits::default() };
    assert_eq!(store.users().get("ann").unwrap().unwrap().limits, ann_limits);
    // The namespace's limits are on its open state, its memory limit too;
    // those of the dropped namespace are gone
    let social = NamespaceLimits { max_running: Some(16), max_memory_bytes: Some(1 << 30) };
    assert_eq!(store.namespace("social").unwrap().limits(), social);
    assert_eq!(store.users().namespace_limits().unwrap().len(), 1);
    let db = Embedded::new(store, QueryConfig::default()).unwrap();
    assert_eq!(ann(&db, &expected).limits, ann_limits);
    block_on(db.login("root", Secret::new("root-password"), None)).unwrap();
    block_on(db.set_namespace_limits("social", NamespaceLimits::default())).unwrap();
    assert!(block_on(db.limits()).unwrap().namespaces.is_empty());
    db.close().unwrap();
    assert!(iwdb::verify(work.path()).unwrap().is_ok());
}
