//! Compatibility fixture of the users' records (step 15a, design rule 4,
//! `documentation/formats/auth.md`): `tests/fixtures/auth-v1/` is a store
//! whose system namespace holds auth format 1, changed before and after a
//! checkpoint of it, so the records are in the checkpoint and in the WAL.
//! Every later version must read it: the users, their admin flags and
//! grants, which password each hash accepts, and the API token.
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
use iwdb_query::{Accounts, Authenticate};

/// Cheap parameters for most users; `root` has the defaults, so the fixture
/// holds both.
const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/auth-v{}", AUTH_FORMAT))
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

/// What a store's users look like, as text.
fn describe(store: &Store) -> String {
    let users = store.users();
    let mut out = String::new();
    for user in users.list().unwrap() {
        out.push_str(&format!("user {} admin={} grants={:?}\n", user.name, user.admin, user.grants));
        for token in users.tokens(&user.name).unwrap() {
            out.push_str(&format!("token {} {} expires={:?}\n", token.user, token.name, token.expires_ms.is_some()));
        }
    }
    out
}

/// Writes the fixture, if it doesn't exist yet: `root` (admin), `ann`
/// (write on `social`, an API token `ci`), `bob` (made, then deleted) and
/// `cy` (read on default, then revoked, then its password changed), with a
/// checkpoint of the system namespace in the middle. `expected.txt` is the
/// description, the passwords as `password <user> <password>` and the
/// token as `token-secret <secret>`.
#[test]
#[ignore = "writes the fixture"]
fn generate_fixture() {
    let dir = fixture();
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
    store.users().create("root", &Secret::new("root-password"), true).unwrap();
    let users = store.users().with_params(FAST);
    users.create("ann", &Secret::new("ann-password"), false).unwrap();
    users.grant("ann", "social", Role::Write).unwrap();
    users.create("bob", &Secret::new("bob-password"), false).unwrap();
    users.create("cy", &Secret::new("cy-password-1"), false).unwrap();
    users.grant("cy", "default", Role::Read).unwrap();
    // The records so far into a checkpoint of the system namespace
    store.checkpoint_all().unwrap();
    let token = users.create_token("ann", "ci", None).unwrap();
    users.delete("bob").unwrap();
    users.revoke("cy", "default").unwrap();
    users.set_password("cy", &Secret::new("cy-password-2"), None).unwrap();
    let mut expected = describe(&store);
    for (user, pw) in [("root", "root-password"), ("ann", "ann-password"), ("cy", "cy-password-2")] {
        expected.push_str(&format!("password {} {}\n", user, pw));
    }
    expected.push_str(&format!("token-secret {}\n", token.token.expose()));
    store.close().unwrap();
    let _ = fs::remove_file(path.join("LOCK"));
    fs::write(dir.join("expected.txt"), expected).unwrap();
}

#[test]
fn reads_auth_format_1() {
    let expected = fs::read_to_string(fixture().join("expected.txt")).unwrap();
    let work = tempfile::tempdir().unwrap();
    copy_dir(&fixture().join("store"), work.path());
    let store = Store::open(work.path(), StoreOptions::default()).unwrap();
    let description: String = expected
        .lines()
        .filter(|l| l.starts_with("user ") || l.starts_with("token "))
        .map(|l| format!("{}\n", l))
        .collect();
    assert_eq!(describe(&store), description);
    for line in expected.lines().filter_map(|l| l.strip_prefix("password ")) {
        let (user, pw) = line.split_once(' ').unwrap();
        assert!(store.users().verify(user, &Secret::new(pw)).unwrap(), "{}", user);
    }
    assert!(!store.users().verify("cy", &Secret::new("cy-password-1")).unwrap());
    assert!(store.users().exist().unwrap());
    let secret = expected.lines().find_map(|l| l.strip_prefix("token-secret ")).unwrap().to_owned();
    let db = Embedded::new(store, QueryConfig::default()).unwrap();
    let principal = block_on(db.authenticate(&Secret::new(secret))).unwrap();
    assert_eq!((principal.user.as_str(), principal.role("social")), ("ann", Some(Role::Write)));
    // And it goes on: a login, a change
    block_on(db.login("root", Secret::new("root-password"), None)).unwrap();
    block_on(db.grant("cy", "social", Role::Admin)).unwrap();
    db.close().unwrap();
    assert!(iwdb::verify(work.path()).unwrap().is_ok());
}
