//! `iwctl --server <endpoint>` (step 16e, ADR 0055): every admin command
//! against a server the test starts, with authentication on, a WAL archive
//! and a backup directory; text and `--json` output and exit codes. Step
//! 16e's acceptance criterion: a test per command.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iwdb::auth::{AuthSettings, HashParams};
use iwdb::{Embedded, Mutation, QueryConfig, Secret, Store, StoreOptions};
use iwdb_query::exec::block_on;
use iwdb_query::{ChangesRequest, Code, Database, QueryOptions};
use iwdb_server::Server;
use iwdb_server::auth::AuthMode;
use iwdb_server::client::Remote;
use serde_json::Value;
use tokio::runtime::Runtime;

/// Cheap hashes for the test users.
const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

/// A server of a store with commits and no checkpoint after them, until
/// dropped: `root` (a server admin) and `ann` (`read` on `default`), each
/// with an API token.
struct Running {
    endpoint: String,
    root: String,
    ann: String,
    backups: PathBuf,
    _runtime: Runtime,
    _dir: tempfile::TempDir,
}

fn node(id: &str, pad: usize) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["Person".into()],
        attr: [("pad".to_owned(), iwdb::Value::String("x".repeat(pad)))].into(),
        meta: Default::default(),
        expected_version: None,
    }
}

fn serve() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let mut options = StoreOptions { archive: Some(dir.path().join("archive")), ..StoreOptions::default() };
    options.wal.segment_size = iwdb_storage::MIN_SEGMENT_SIZE;
    options.checkpoint.background = false;
    options.checkpoint.keep = 1;
    let store = Store::open(&dir.path().join("data"), options).unwrap();
    let users = store.users().with_params(FAST);
    users.create("root", &Secret::new("root-password"), true).unwrap();
    users.create("ann", &Secret::new("ann-password"), false).unwrap();
    users.grant("ann", "default", iwdb::Role::Read).unwrap();
    let root = users.create_token("root", "cli", None).unwrap().token.expose().to_owned();
    let ann = users.create_token("ann", "cli", None).unwrap().token.expose().to_owned();
    // Several checkpoints, so that segments reach the archive, then more
    for round in 0..4 {
        for i in 0..10 {
            store.commit(&[node(&format!("n{}-{}", round, i), 300)]).unwrap();
        }
        store.checkpoint().unwrap();
    }
    store.commit(&[node("last", 10)]).unwrap();
    let backups = dir.path().join("backups");
    std::fs::create_dir(&backups).unwrap();
    let settings = AuthSettings { hash: FAST, ..AuthSettings::default() };
    let db =
        Embedded::new(store, QueryConfig::default()).unwrap().with_auth(settings).with_backup_dir(&backups).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = Server::new(Arc::new(db)).auth(AuthMode { enabled: true });
    runtime.spawn(async move {
        let never = std::future::pending::<()>();
        let _ = server.serve(listener, never, || async { tokio::time::sleep(Duration::ZERO).await }).await;
    });
    Running { endpoint, root, ann, backups, _runtime: runtime, _dir: dir }
}

impl Running {
    /// `iwctl --server <endpoint> <args>` as root (its token in
    /// `IWDB_TOKEN`).
    fn iwctl(&self, args: &[&str]) -> Output {
        self.iwctl_as(&self.root, args)
    }

    fn iwctl_as(&self, token: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_iwctl"))
            .args(["--server", &self.endpoint])
            .args(args)
            .env("IWDB_TOKEN", token)
            .output()
            .unwrap()
    }

    /// The same with `--json`: its answers, one JSON object per line.
    fn json(&self, args: &[&str]) -> Vec<Value> {
        let mut all = vec!["--json"];
        all.extend(args);
        let o = self.iwctl(&all);
        assert_eq!(o.status.code(), Some(0), "{:?}: {}", args, stderr(&o));
        stdout(&o).lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{}: {}", e, l))).collect()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn ok(o: &Output) -> String {
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(o), stderr(o));
    stdout(o)
}

fn fails(o: &Output, code: i32, text: &str) {
    assert_eq!(o.status.code(), Some(code), "{}{}", stdout(o), stderr(o));
    assert!(stderr(o).contains(text), "{}", stderr(o));
}

#[test]
fn status() {
    let s = serve();
    let text = ok(&s.iwctl(&["status"]));
    assert!(text.contains(&format!("server  {}", s.endpoint)), "{}", text);
    assert!(text.contains("namespace default (id 1): seq 41"), "{}", text);
    let [v] = s.json(&["status"]).try_into().unwrap();
    assert_eq!((v["ready"].as_bool(), v["fsync"].as_str()), (Some(true), Some("always")));
    assert_eq!(v["namespaces"][0]["name"], "default");
    // A reader sees the status too, narrowed to its namespaces
    assert!(ok(&s.iwctl_as(&s.ann, &["status"])).contains("namespace default"));
}

#[test]
fn checkpoint() {
    let s = serve();
    let all = s.json(&["checkpoint"]);
    let default = all.iter().find(|v| v["namespace"] == "default").unwrap();
    assert_eq!((default["seq"].as_u64(), default["written"].as_bool()), (Some(41), Some(true)));
    assert_eq!(default["path"], s.endpoint.as_str());
    let text = ok(&s.iwctl(&["checkpoint", "-n", "default"]));
    assert!(text.contains("checkpoint at seq 41 (already current, nothing written)"), "{}", text);
    fails(&s.iwctl(&["checkpoint", "-n", "nope"]), 4, "not_found");
    // Only a server admin
    fails(&s.iwctl_as(&s.ann, &["checkpoint"]), 4, "permission_denied");
}

#[test]
fn backup() {
    let s = serve();
    let [backup, verify] = s.json(&["backup", "nightly"]).try_into().unwrap();
    assert_eq!(backup["backup"]["namespaces"][0]["seq"], 41);
    assert_eq!((verify["verify"]["ok"].as_bool(), verify["verify"]["kind"].as_str()), (Some(true), Some("backup")));
    assert!(s.backups.join("nightly").join("IWDB").is_file());
    // Never over anything; names stay in the backup directory
    fails(&s.iwctl(&["backup", "nightly"]), 4, "conflict");
    fails(&s.iwctl(&["backup", "../escape"]), 4, "invalid_argument");
    assert!(!s.backups.parent().unwrap().join("escape").exists());
    // Throttled, without verifying
    let text = ok(&s.iwctl(&["backup", "slow", "--max-bytes-per-second", "100000000", "--no-verify"]));
    assert!(text.contains("backed up") && !text.contains("verify"), "{}", text);
}

#[test]
fn verify() {
    let s = serve();
    let text = ok(&s.iwctl(&["verify"]));
    assert!(text.contains("(data directory): ok"), "{}", text);
    let [v] = s.json(&["verify", "archive"]).try_into().unwrap();
    assert_eq!((v["verify"]["kind"].as_str(), v["verify"]["ok"].as_bool()), (Some("archive"), Some(true)));
    ok(&s.iwctl(&["backup", "b", "--no-verify"]));
    let [v] = s.json(&["verify", "backup", "b"]).try_into().unwrap();
    assert_eq!(v["verify"]["kind"], "backup");
    fails(&s.iwctl(&["verify", "backup", "missing"]), 4, "not_found");
}

#[test]
fn archive_prune() {
    let s = serve();
    ok(&s.iwctl(&["backup", "kept", "--no-verify"]));
    let [dry] = s.json(&["archive", "prune", "--before", "kept", "--dry-run"]).try_into().unwrap();
    assert_eq!(dry["prune"]["dry_run"], true);
    let segments = dry["prune"]["namespaces"][0]["removed_segments"].as_array().unwrap().len();
    assert!(segments > 0, "{}", dry);
    let text = ok(&s.iwctl(&["archive", "prune", "--before", "kept"]));
    assert!(text.contains(&format!("removed {} archived segments", segments)), "{}", text);
    let text = ok(&s.iwctl(&["archive", "prune", "--before", "kept"]));
    assert!(text.contains("removed 0 archived segments"), "{}", text);
    assert!(ok(&s.iwctl(&["verify", "archive"])).contains(": ok"));
    fails(&s.iwctl(&["archive", "prune", "--before", "missing"]), 4, "not_found");
}

#[test]
fn namespaces_created_and_dropped() {
    let s = serve();
    let text = ok(&s.iwctl(&["create-namespace", "social", "--key", "k1"]));
    assert!(text.contains("created namespace 'social'"), "{}", text);
    let text = ok(&s.iwctl(&["create-namespace", "social", "--key", "k1"]));
    assert!(text.contains("a retry: nothing changed"), "{}", text);
    let [v] = s.json(&["namespaces"]).try_into().unwrap();
    let names: Vec<&str> = v["namespaces"].as_array().unwrap().iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(names.contains(&"social") && names.contains(&"default"), "{:?}", names);
    let text = ok(&s.iwctl(&["drop-namespace", "social"]));
    assert!(text.contains("dropped namespace 'social'"), "{}", text);
    assert!(!ok(&s.iwctl(&["namespaces"])).contains("social"));
}

#[test]
fn indexes_and_constraints() {
    let s = serve();
    assert!(ok(&s.iwctl(&["create-index", "address.city"])).contains("created index at seq 42"));
    assert!(ok(&s.iwctl(&["add-constraint", "unique", "Person", "email"])).contains("added constraint at seq 43"));
    let [v] = s.json(&["indexes"]).try_into().unwrap();
    let paths: Vec<&str> = v["indexes"].as_array().unwrap().iter().filter_map(|i| i["path"].as_str()).collect();
    assert!(paths.contains(&"address.city") && paths.contains(&"email"), "{:?}", paths);
    assert_eq!(v["constraints"].as_array().unwrap().len(), 1, "{}", v);
    assert!(ok(&s.iwctl(&["drop-constraint", "unique", "Person", "email"])).contains("dropped constraint"));
    assert!(ok(&s.iwctl(&["drop-index", "address.city", "-n", "default"])).contains("dropped index"));
    let text = ok(&s.iwctl(&["indexes"]));
    assert!(!text.contains("address.city") && !text.contains("email"), "{}", text);
    // A reader may list, not change
    ok(&s.iwctl_as(&s.ann, &["indexes"]));
    fails(&s.iwctl_as(&s.ann, &["create-index", "x"]), 4, "permission_denied");
}

#[test]
fn requests_and_cancel() {
    let s = serve();
    // The request list holds the call that lists it
    let [v] = s.json(&["requests"]).try_into().unwrap();
    assert!(v["requests"].as_array().unwrap().iter().any(|r| r["operation"] == "ListRequests"), "{}", v);
    // A long poll of ann's, cancelled by root
    let remote = Remote::connect(&s.endpoint).unwrap();
    remote.set_token(Some(Secret::new(s.ann.clone())));
    let poll = std::thread::spawn(move || {
        let options = QueryOptions { timeout: Some(Duration::from_secs(30)), ..QueryOptions::default() };
        block_on(remote.changes("default", ChangesRequest { from_seq: 1_000_000, wait: true }, options))
    });
    let start = Instant::now();
    let id = loop {
        let [v] = s.json(&["requests", "ann"]).try_into().unwrap();
        if let Some(r) = v["requests"].as_array().unwrap().iter().find(|r| r["operation"] == "GetChanges") {
            break r["id"].as_u64().unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(10), "the long poll was never listed");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(ok(&s.iwctl(&["requests"])).contains(&format!("request {}: GetChanges on default by ann", id)));
    let text = ok(&s.iwctl(&["cancel", &id.to_string()]));
    assert!(text.contains(&format!("cancelled request {}", id)), "{}", text);
    assert_eq!(poll.join().unwrap().unwrap_err().code(), Code::Cancelled);
    fails(&s.iwctl(&["cancel", &id.to_string()]), 4, "not_found");
}

#[test]
fn restore_is_offline() {
    let s = serve();
    let o = s.iwctl(&["restore", "/tmp/r", "--backup", "b"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("offline"), "{}", stderr(&o));
    for args in [["import", "x", "f"].as_slice(), ["export", "f"].as_slice()] {
        assert_eq!(s.iwctl(args).status.code(), Some(2), "{:?}", args);
    }
}

#[test]
fn without_credentials_or_with_a_wrong_server() {
    let s = serve();
    let o = Command::new(env!("CARGO_BIN_EXE_iwctl")).args(["--server", &s.endpoint, "status"]).output().unwrap();
    fails(&o, 4, "unauthenticated");
    let o =
        Command::new(env!("CARGO_BIN_EXE_iwctl")).args(["--server", "http://127.0.0.1:1", "status"]).output().unwrap();
    assert_eq!(o.status.code(), Some(3), "a server that doesn't answer is unavailable: {}", stderr(&o));
}
