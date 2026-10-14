//! Compatibility fixtures of releases (step 17a): every released version
//! leaves `tests/fixtures/releases/<version>/`, written by that version,
//! and every later version must open it. The format fixtures next to it
//! (`data-dir-v*`, `backup-v*`, `archive-v*`, `auth-v*`, and `wal-v*` in
//! iwdb-storage) pin each format version on its own; a release fixture
//! pins what a release actually wrote, all formats together:
//!
//! - `store/`: a data directory with a checkpoint and WAL records after it
//!   in each namespace: `default` (keyed commits), `people` (a unique
//!   constraint, an index, keyed commits, marks), `scratch` (no
//!   checkpoint), `imported` (made by an LGF import), one dropped
//!   namespace, and the system namespace (users, grants, an API token);
//! - `backup/`: a backup of it, taken at the end;
//! - `archive/`: its WAL archive, from its first commit;
//! - `expected.txt`: the state, as `describe` and `describe_users` write it;
//! - `archive.txt`: the state the archive restores to. The archive gets a
//!   segment once it is sealed, so it ends before the store's last
//!   records.
//!
//! Each release fixture must open as a store with the state in
//! `expected.txt`, verify, restore from its backup to the same state and
//! from its archive to the state in `archive.txt`; and the store goes on
//! (a commit, a login, a clean close). The current tree writes a release
//! fixture to a temporary directory and checks it the same way, so the
//! generator keeps working between releases.
//!
//! Before tagging a release, write its fixture with the release commit and
//! commit it (`documentation/releasing.md`):
//!
//! ```text
//! cargo test -p iwdb --test release_fixture -- --ignored generate_fixture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::auth::HashParams;
use iwdb::{
    AttrPath, CatalogChange, CommitOptions, Constraint, ConstraintKind, Embedded, IdempotencyKey, IndexDef, Kind,
    Label, MarkName, MarkUpdate, Mutation, Namespace, QueryConfig, RestoreSources, RestoreTarget, Role, Secret, Store,
    StoreOptions, Value, verify,
};
use iwdb_query::exec::block_on;
use iwdb_query::{Accounts, Authenticate};
use support::{options, pad, reference, run, run_keyed, snapshot, workload};

/// Cheap password hashing for the fixture's users but `root`, which has
/// the defaults (as in `auth_fixture.rs`).
const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

fn releases() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/releases")
}

/// A namespace's state as text: the canonical graph, the catalog, the seq,
/// the idempotency keys (with commit times) and the marks.
fn describe(ns: &Namespace) -> String {
    let mut out = String::new();
    for line in iwdb_engine::testutil::canonical(ns.graph()) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("catalog {:?}\nseq {}\n", ns.catalog(), ns.seq()));
    for entry in ns.keys().entries() {
        out.push_str(&format!("key {:?}\n", entry));
    }
    for (name, entry) in ns.marks().iter() {
        out.push_str(&format!("mark {} {:?}\n", name, entry));
    }
    out
}

/// Every live namespace of the store, by name: a `namespace <name>` line,
/// then its description.
fn describe_store(store: &Store) -> String {
    let mut out = String::new();
    for info in store.namespaces() {
        let ns = store.namespace(info.name.as_str()).unwrap();
        out.push_str(&format!("namespace {}\n{}", info.name, ns.read(describe)));
    }
    out
}

/// The users, their grants and tokens.
fn describe_users(store: &Store) -> String {
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

/// Copy `from` to `to` as a git checkout has it: without empty
/// directories, which git doesn't keep. So a fixture just written (in a
/// temporary directory, or before it is committed) is checked as a clone
/// of the repository will see it.
fn copy_dir(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::create_dir_all(to).unwrap();
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Git keeps no empty directories: make every namespace's `checkpoints/`
/// and `wal/` again in a copied store or backup (`scratch` has no
/// checkpoint, and nothing after its backup's start; a placeholder file
/// would be a file the backup's manifest doesn't list).
fn make_empty_dirs(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir.join("ns")) {
        for ns in entries {
            let ns = ns.unwrap().path();
            fs::create_dir_all(ns.join("checkpoints")).unwrap();
            fs::create_dir_all(ns.join("wal")).unwrap();
        }
    }
}

/// Write a release fixture into `dir` (which must not exist).
fn write_release(dir: &Path) {
    let opts = StoreOptions { archive: Some(dir.join("archive")), ..options(2) };
    let store = Store::open(&dir.join("store"), opts).unwrap();
    let key = |k: &str| IdempotencyKey::new(k).unwrap();
    let keyed = |k: &str| CommitOptions { idempotency_key: Some(key(k)) };

    // `default`: commits, keyed commits, a checkpoint, more of both
    let mut reference = reference();
    let steps = workload(40, 17_000);
    run(&store, &mut reference, &steps[..25]);
    run_keyed(&store, &mut reference, &steps[25..32], "before-");
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[32..]);
    run_keyed(&store, &mut reference, &[pad(1000), pad(1001)], "after-");

    // `people`: a constraint, an index, keyed commits and marks, before and
    // after its checkpoint
    let person = |i: i64| Mutation::UpsertNode {
        id: format!("p{}", i),
        labels: vec!["Person".into()],
        attr: [("email".to_owned(), Value::from(format!("p{}@example.org", i)))].into(),
        meta: Default::default(),
        expected_version: None,
    };
    store.create_namespace("people", Some(&key("mk-people"))).unwrap();
    let people = store.namespace("people").unwrap();
    let path = AttrPath::new(["email"]).unwrap();
    let unique = Constraint { kind: ConstraintKind::Unique, label: Label::new("Person").unwrap(), path: path.clone() };
    people.commit_catalog(CatalogChange::AddConstraint(unique)).unwrap();
    people.commit_catalog(CatalogChange::CreateIndex(IndexDef { path })).unwrap();
    let mark = |name: &str, expected, position| MarkUpdate { name: MarkName::new(name).unwrap(), expected, position };
    let plain = CommitOptions::default();
    for i in 0..4 {
        people.commit_with(&[person(i)], &keyed(&format!("person-{}", i))).unwrap();
    }
    people.commit_marked(&[person(10)], &mark("feed", None, 10), &plain).unwrap();
    people.checkpoint().unwrap();
    for i in 4..7 {
        people.commit_with(&[person(i)], &keyed(&format!("person-{}", i))).unwrap();
    }
    people.commit_marked(&[], &mark("feed", Some(10), 12), &keyed("feed-12")).unwrap();

    // `scratch`: commits, no checkpoint; `gone`: filled and dropped
    store.create_namespace("scratch", None).unwrap();
    let scratch = store.namespace("scratch").unwrap();
    for i in 0..5 {
        scratch.commit(&[person(100 + i)]).unwrap();
    }
    store.create_namespace("gone", Some(&key("mk-gone"))).unwrap();
    store.namespace("gone").unwrap().commit(&[person(200)]).unwrap();
    store.drop_namespace("gone", Some(&key("rm-gone"))).unwrap();

    // `imported`: an import, then commits after its first checkpoint
    let lgf = "@nodes\nlabel name size\na Ann 1\nb \"Bo B\" 2.5\n@arcs\n\t\tw\na b 3\nb a 4\n";
    store.import_namespace("imported", iwdb::import::ImportFormat::Lgf, lgf.as_bytes(), None).unwrap();
    store.namespace("imported").unwrap().commit(&[person(300)]).unwrap();

    // The system namespace: users and grants, checkpointed, then a token
    // and more changes in its WAL
    store.users().create("root", &Secret::new("root-password"), true).unwrap();
    let users = store.users().with_params(FAST);
    users.create("ann", &Secret::new("ann-password"), false).unwrap();
    users.grant("ann", "people", Role::Write).unwrap();
    users.create("cy", &Secret::new("cy-password"), false).unwrap();
    store.checkpoint_all().unwrap();
    let token = users.create_token("ann", "ci", None).unwrap();
    users.grant("cy", "default", Role::Read).unwrap();

    // The backup covers everything; the WAL keeps records after each
    // namespace's last checkpoint
    store.backup(&dir.join("backup")).unwrap();
    let mut expected = format!("history {}\n", store.history());
    expected += &describe_store(&store);
    expected += &describe_users(&store);
    for (user, pw) in [("root", "root-password"), ("ann", "ann-password"), ("cy", "cy-password")] {
        expected += &format!("password {} {}\n", user, pw);
    }
    expected += &format!("token-secret {}\n", token.token.expose());
    // No close: it would checkpoint every namespace and empty the WAL
    drop(store);
    for lock in [dir.join("store/LOCK"), dir.join("archive/LOCK")] {
        let _ = fs::remove_file(lock);
    }
    fs::write(dir.join("expected.txt"), expected).unwrap();
    fs::write(dir.join("archive.txt"), restored(&RestoreSources { backup: None, archive: Some(dir.join("archive")) }))
        .unwrap();
}

/// The state restored from `sources`, as text: the namespaces and the users.
fn restored(sources: &RestoreSources) -> String {
    let work = tempfile::tempdir().unwrap();
    let dest = work.path().join("restored");
    iwdb::restore(&dest, sources, RestoreTarget::Latest).unwrap();
    let store = Store::open(&dest, StoreOptions::default()).unwrap();
    let text = describe_store(&store) + &describe_users(&store);
    store.close().unwrap();
    text
}

/// What `expected.txt` says.
struct Expected {
    history: String,
    namespaces: String,
    users: String,
    passwords: Vec<(String, String)>,
    token: String,
}

fn read_expected(dir: &Path) -> Expected {
    let text = fs::read_to_string(dir.join("expected.txt")).unwrap();
    let mut expected = Expected {
        history: String::new(),
        namespaces: String::new(),
        users: String::new(),
        passwords: Vec::new(),
        token: String::new(),
    };
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("history ") {
            expected.history = h.to_owned();
        } else if line.starts_with("user ") || line.starts_with("token ") {
            expected.users += &format!("{}\n", line);
        } else if let Some(rest) = line.strip_prefix("password ") {
            let (user, pw) = rest.split_once(' ').unwrap();
            expected.passwords.push((user.to_owned(), pw.to_owned()));
        } else if let Some(secret) = line.strip_prefix("token-secret ") {
            expected.token = secret.to_owned();
        } else {
            expected.namespaces += &format!("{}\n", line);
        }
    }
    expected
}

/// The users of `store` are the expected ones: the same records, every
/// password accepted, the token authenticates.
fn check_users(store: &Store, expected: &Expected, what: &str) {
    assert_eq!(describe_users(store), expected.users, "{}", what);
    for (user, pw) in &expected.passwords {
        assert!(store.users().verify(user, &Secret::new(pw.as_str())).unwrap(), "{}: {}", what, user);
    }
}

/// Check the release fixture in `dir` without changing it: the store
/// opens with every namespace, key, mark and user, and goes on; the backup
/// and the archive verify and restore to the same state.
fn check_release(dir: &Path) {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let expected = read_expected(dir);
    let before = snapshot(dir);

    // The data directory
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("store");
    copy_dir(&dir.join("store"), &path);
    make_empty_dirs(&path);
    let store = Store::open(&path, StoreOptions::default()).unwrap();
    assert_eq!(store.history().to_string(), expected.history, "{}", name);
    assert_eq!(describe_store(&store), expected.namespaces, "{}: the store", name);
    let report = store.recovery();
    assert!(report.skipped_checkpoints.is_empty() && report.torn_tail.is_none(), "{}: {:?}", name, report);
    check_users(&store, &expected, &format!("{}: the store", name));
    // It goes on: a retried keyed commit is answered from its key, a new
    // commit and a constraint work, the token authenticates, a login and a
    // grant commit, and it closes and verifies clean
    let people = store.namespace("people").unwrap();
    let seq = people.seq();
    let retry = people.commit_marked(
        &[],
        &MarkUpdate { name: MarkName::new("feed").unwrap(), expected: Some(10), position: 12 },
        &CommitOptions { idempotency_key: Some(IdempotencyKey::new("feed-12").unwrap()) },
    );
    assert!(retry.unwrap().deduplicated, "{}", name);
    assert_eq!(people.seq(), seq);
    let duplicate = Mutation::UpsertNode {
        id: "other".into(),
        labels: vec!["Person".into()],
        attr: [("email".to_owned(), Value::from("p1@example.org"))].into(),
        meta: Default::default(),
        expected_version: None,
    };
    assert!(people.commit(&[duplicate]).is_err(), "{}: the unique constraint holds", name);
    let support::Step::Tx(mutations) = pad(2000) else { panic!("a transaction") };
    store.commit(&mutations).unwrap();
    let db = Embedded::new(store, QueryConfig::default()).unwrap();
    let principal = block_on(db.authenticate(&Secret::new(expected.token.as_str()))).unwrap();
    assert_eq!((principal.user.as_str(), principal.role("people")), ("ann", Some(Role::Write)), "{}", name);
    block_on(db.login("root", Secret::new("root-password"), None)).unwrap();
    block_on(db.grant("cy", "scratch", Role::Read)).unwrap();
    db.close().unwrap();
    let report = verify(&path).unwrap();
    assert!(report.is_ok(), "{}: {:#?}", name, report.problems);

    // The backup verifies and restores the store's state, with a new
    // history and every password
    let backup = work.path().join("backup");
    copy_dir(&dir.join("backup"), &backup);
    make_empty_dirs(&backup);
    let report = verify(&backup).unwrap();
    assert!(report.is_ok() && report.kind == Kind::Backup, "{}: {:#?}", name, report.problems);
    let dest = work.path().join("restored");
    iwdb::restore(&dest, &RestoreSources { backup: Some(backup), archive: None }, RestoreTarget::Latest).unwrap();
    let store = Store::open(&dest, StoreOptions::default()).unwrap();
    assert_ne!(store.history().to_string(), expected.history);
    assert_eq!(describe_store(&store), expected.namespaces, "{}: restored from the backup", name);
    check_users(&store, &expected, &format!("{}: restored from the backup", name));
    store.close().unwrap();

    // The archive verifies and restores the state it reaches
    let archive = work.path().join("archive");
    copy_dir(&dir.join("archive"), &archive);
    let report = verify(&archive).unwrap();
    assert!(report.is_ok() && report.kind == Kind::Archive, "{}: {:#?}", name, report.problems);
    let reached = fs::read_to_string(dir.join("archive.txt")).unwrap();
    assert_eq!(restored(&RestoreSources { backup: None, archive: Some(archive) }), reached, "{}: the archive", name);
    assert_eq!(snapshot(dir), before, "{}: the fixture itself is unchanged", name);
}

/// Every release fixture opens, verifies and restores in this version.
#[test]
fn every_release_fixture_opens_in_this_version() {
    let Ok(entries) = fs::read_dir(releases()) else { return };
    let mut dirs: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        check_release(&dir);
    }
}

/// The current tree writes a release fixture that passes the same checks.
#[test]
fn this_version_writes_a_release_fixture_that_opens() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join(env!("CARGO_PKG_VERSION"));
    write_release(&dir);
    check_release(&dir);
}

/// Writes the fixture of this version (the workspace version) into
/// `tests/fixtures/releases/<version>/`. A release's fixture is written
/// once, by the release commit: it fails if the directory exists.
#[test]
#[ignore = "writes the fixture"]
fn generate_fixture() {
    let dir = releases().join(env!("CARGO_PKG_VERSION"));
    assert!(!dir.exists(), "{} exists: a release's fixture is written once", dir.display());
    write_release(&dir);
    check_release(&dir);
}
