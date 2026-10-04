//! `iwctl shell` (step 14a) against a server on an ephemeral port: a
//! session piped through stdin, in tables and in JSON, and its exit codes;
//! over TLS with a client certificate (step 15b).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use iwdb::auth::{AuthSettings, HashParams};
use iwdb::{Embedded, QueryConfig, Secret, Store, StoreOptions};
use iwdb_server::Server;
use iwdb_server::auth::AuthMode;
use tokio::runtime::Runtime;

/// A server of a new store in a temporary directory, until dropped.
struct Running {
    endpoint: String,
    _runtime: Runtime,
    _dir: tempfile::TempDir,
}

fn serve() -> Running {
    serve_with(false)
}

/// Cheap hashes for the test users.
const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

/// With `auth`: authentication on, users `root` (admin, password
/// `root-password`) and `ann` (read on default, `ann-password`).
fn serve_with(auth: bool) -> Running {
    serve_on(auth, false)
}

/// A test certificate or key (test-only, public).
fn fixture(name: &str) -> String {
    format!("{}/../../tests/fixtures/tls/{}", env!("CARGO_MANIFEST_DIR"), name)
}

/// With `tls`: over TLS, with client certificates of the test CA optional.
fn serve_on(auth: bool, tls: bool) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let mut options = StoreOptions::default();
    options.wal.fsync = iwdb::FsyncPolicy::Off;
    options.checkpoint.background = false;
    let store = Store::open(dir.path(), options).unwrap();
    if auth {
        let users = store.users().with_params(FAST);
        users.create("root", &Secret::new("root-password"), true).unwrap();
        users.create("ann", &Secret::new("ann-password"), false).unwrap();
        users.grant("ann", "default", iwdb::Role::Read).unwrap();
    }
    let settings = AuthSettings { hash: FAST, ..AuthSettings::default() };
    let db = Embedded::new(store, QueryConfig::default()).unwrap().with_auth(settings);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let scheme = if tls { "https" } else { "http" };
    let endpoint = format!("{}://{}", scheme, listener.local_addr().unwrap());
    let tls = tls.then(|| {
        let files = iwdb_server::tls::TlsFiles {
            cert: fixture("server.pem").into(),
            key: fixture("server.key").into(),
            client_ca: Some(fixture("ca.pem").into()),
            client_auth: iwdb_server::config::ClientAuth::Optional,
        };
        Arc::new(iwdb_server::tls::ServerTls::load(files).unwrap())
    });
    let server = Server::new(Arc::new(db)).auth(AuthMode { enabled: auth }).tls(tls);
    runtime.spawn(async move {
        let never = std::future::pending::<()>();
        let _ = server.serve(listener, never, || async { tokio::time::sleep(Duration::ZERO).await }).await;
    });
    Running { endpoint, _runtime: runtime, _dir: dir }
}

fn shell(args: &[&str], script: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_iwctl"))
        .arg("shell")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const DATA: &str = r#"
-- a namespace with three people
create-namespace social
use social
create-index age
upsert-node alice :Person {"age": 36, "name": "Alice"}
upsert-node bob :Person {"age": 41}
upsert-node carol :Person :Admin {"age": 29}
add-edge alice bob :KNOWS {"since": 2020}
add-edge bob carol :KNOWS
"#;

#[test]
fn a_session_creates_commits_matches_and_pages() {
    let server = serve();
    let script = format!(
        "{}\nmatch (a:Person)-[k:KNOWS]->(b)\n\\limit 2\nfind {{\"Label\": \"Person\"}}\n\\next\n\\next\nnode alice nobody\nedge 0\nindexes\nexplain {{\"Compare\": {{\"path\": [\"age\"], \"op\": \"Eq\", \"value\": {{\"Int\": 36}}}}}}\nnamespaces\n\\quit\nnode never-read\n",
        DATA
    );
    let o = shell(&[&server.endpoint], &script);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(4), "the second \\next fails: {}\n{}", out, stderr(&o));
    assert!(out.contains("created namespace 'social'"), "{}", out);
    assert!(out.contains("create-index age at seq 1"), "{}", out);
    assert!(out.contains("committed at seq 5 (edge 0)"), "{}", out);
    // The match: columns a, b, then the edge variable
    assert!(
        out.contains("a     | b     | k\n------+-------+--\nalice | bob   | 0\nbob   | carol | 1\n(2 rows)"),
        "{}",
        out
    );
    // Two pages of find, then no more
    // Each page is aligned on its own
    assert!(
        out.contains("alice | Person | {\"age\":36,\"name\":\"Alice\"} | 1\nbob   | Person | {\"age\":41}"),
        "{}",
        out
    );
    assert!(out.contains("(2 rows)\n(more: \\next)"), "{}", out);
    assert!(out.contains("carol | Admin,Person | {\"age\":29} | 1\n(1 row)"), "{}", out);
    assert!(stderr(&o).contains("error (usage): no more pages"), "{}", stderr(&o));
    assert!(out.contains("not found: nobody"), "{}", out);
    assert!(out.contains("0  | alice | bob | KNOWS | {\"since\":2020} | 1"), "{}", out);
    assert!(out.contains("index age ready (declared)"), "{}", out);
    assert!(out.contains("candidates: 1 estimated, 1 exact, of 3 nodes"), "{}", out);
    assert!(out.contains("social "), "{}", out);
    assert!(!out.contains("never-read"), "\\quit ends the session: {}", out);
}

#[test]
fn json_mode_prints_one_object_per_answer() {
    let server = serve();
    let script = format!("{}\nmatch (a)-[:KNOWS]->(b {{age: 41}})\nfind {{\"Label\": \"Nope\"}}\nnode bob\n", DATA);
    let o = shell(&["--json", "-n", "social", &server.endpoint], &script.replacen("use social\n", "", 1));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let lines: Vec<serde_json::Value> = stdout(&o).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let m = lines.iter().find(|v| v.get("rows").is_some()).unwrap();
    // The anonymous edge gets a column of its own
    assert_eq!(m["columns"], serde_json::json!(["a", "b", "_e0"]));
    assert_eq!(m["rows"], serde_json::json!([{"nodes": ["alice", "bob"], "edges": [[0]]}]));
    assert_eq!(m["cursor"], serde_json::Value::Null);
    let find = lines.iter().find(|v| v.get("nodes").is_some_and(|n| n.as_array().unwrap().is_empty())).unwrap();
    assert_eq!(find["truncated"], false);
    let node = lines.last().unwrap();
    assert_eq!(node["nodes"][0]["attr"]["age"], 41);
}

#[test]
fn errors_print_their_code_and_the_shell_goes_on() {
    let server = serve();
    let script = "use nowhere\nupsert-node a\nadd-edge a ghost\nfind {\"Bogus\": 1}\nmatch (a\nfrobnicate\nnode a\n";
    let o = shell(&[&server.endpoint], script);
    let (out, err) = (stdout(&o), stderr(&o));
    assert_eq!(o.status.code(), Some(4), "{}", out);
    assert!(err.contains("error (not_found):"), "{}", err);
    assert!(err.contains("invalid filter:"), "{}", err);
    assert!(err.contains("invalid pattern:"), "{}", err);
    assert!(err.contains("unknown command 'frobnicate'"), "{}", err);
    // Still in "default", and still running after the errors
    assert!(out.contains("committed at seq 1"), "{}", out);
    assert!(out.ends_with("a  |        | {}   | 1\n(1 row)\n"), "{}", out);
}

#[test]
fn no_server_and_bad_endpoints() {
    let o = shell(&["http://127.0.0.1:1"], "node a\n");
    assert_eq!(o.status.code(), Some(4));
    assert!(stderr(&o).contains("error (unavailable)"), "{}", stderr(&o));
    let o = shell(&["not a url"], "");
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let o = Command::new(env!("CARGO_BIN_EXE_iwctl")).arg("shell").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
}

/// Against a server with authentication on (step 15a): without credentials
/// every command fails with `unauthenticated`; `\login` (its password the
/// next line), `--user` with the password on stdin, and `--token` work, and
/// roles apply.
#[test]
fn the_shell_logs_in() {
    let server = serve_with(true);
    let o = shell(&[&server.endpoint], "namespaces\n");
    assert_eq!(o.status.code(), Some(4));
    assert!(stderr(&o).contains("error (unauthenticated)"), "{}", stderr(&o));
    // \login, then roles: ann reads default but can't commit
    let o = shell(&[&server.endpoint], "\\login ann\nann-password\n\\whoami\nnode x\nupsert-node x\n");
    let (out, err) = (stdout(&o), stderr(&o));
    assert!(out.contains("logged in as ann") && out.contains("ann: default=read"), "{}\n{}", out, err);
    assert!(err.contains("error (permission_denied)"), "{}", err);
    assert!(!out.contains("ann-password") && !err.contains("ann-password"));
    // --user: the password is the first line of input
    let o =
        shell(&[&server.endpoint, "--user", "root"], "root-password\ncreate-namespace other\n\\logout\nnamespaces\n");
    let (out, err) = (stdout(&o), stderr(&o));
    assert!(out.contains("created namespace 'other'") && out.contains("logged out"), "{}\n{}", out, err);
    assert!(err.contains("error (unauthenticated)"), "after logout: {}", err);
    // A wrong password at the start: exit 4 before any command
    let o = shell(&[&server.endpoint, "--user", "root"], "wrong-password\nnamespaces\n");
    assert_eq!(o.status.code(), Some(4));
    assert!(stderr(&o).contains("unauthenticated"), "{}", stderr(&o));
}

/// `iwctl user` and `iwctl token` against a server (`--server`): logged in
/// with `--user`, then with the API token that made, then with
/// `IWDB_TOKEN`; roles apply.
#[test]
fn users_and_tokens_on_a_server() {
    let server = serve_with(true);
    let iwctl = |args: &[&str], input: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_iwctl"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let ep = server.endpoint.as_str();
    // Log in with --user, make a token for root
    let o = iwctl(&["--json", "token", "create", "--server", ep, "--user", "root", "root", "cli"], "root-password\n");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let token: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let token = token["token"].as_str().unwrap().to_owned();
    let o = iwctl(&["user", "create", "--server", ep, "--token", &token, "bob"], "bob-password\n");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o = iwctl(&["--json", "user", "list", "--server", ep, "--token", &token], "");
    let users: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(users["users"].as_array().unwrap().len(), 3);
    // ann may not list users
    let o = iwctl(&["user", "list", "--server", ep, "--user", "ann"], "ann-password\n");
    assert_eq!(o.status.code(), Some(4));
    assert!(stderr(&o).contains("permission_denied"), "{}", stderr(&o));
    // IWDB_TOKEN works too
    let o = Command::new(env!("CARGO_BIN_EXE_iwctl"))
        .args(["user", "grant", "--server", ep, "bob", "default", "admin"])
        .env("IWDB_TOKEN", &token)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("bob: default=admin"), "{}", stdout(&o));
}

/// Over TLS (step 15b): `--tls-ca` trusts the test CA; a client
/// certificate (`--tls-cert`, `--tls-key`) logs in as the user it names,
/// with that user's roles; another CA is refused.
#[test]
fn the_shell_speaks_tls_and_a_certificate_logs_in() {
    let server = serve_on(true, true);
    let (ca, cert, key) = (fixture("ca.pem"), fixture("client-ann.pem"), fixture("client-ann.key"));
    let tls = ["--tls-ca", ca.as_str(), "--tls-cert", cert.as_str(), "--tls-key", key.as_str()];
    let mut args = vec![server.endpoint.as_str()];
    args.extend(tls);
    let o = shell(&args, "\\whoami\nnode x\nupsert-node x\n");
    let (out, err) = (stdout(&o), stderr(&o));
    assert!(out.contains("ann: default=read"), "{}\n{}", out, err);
    assert!(err.contains("error (permission_denied)"), "{}", err);
    // A login over TLS, trusting the CA only
    let o = shell(&[&server.endpoint, "--tls-ca", &ca, "--user", "root"], "root-password\n\\whoami\n");
    assert!(stdout(&o).contains("root (admin)"), "{}\n{}", stdout(&o), stderr(&o));
    // User commands with --server, by certificate
    let mut args = vec!["user", "list", "--server", server.endpoint.as_str()];
    args.extend(tls);
    let o = Command::new(env!("CARGO_BIN_EXE_iwctl")).args(&args).output().unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", stdout(&o));
    assert!(stderr(&o).contains("permission_denied") && stderr(&o).contains("'ann'"), "{}", stderr(&o));
    // Another CA: no answer from the server
    let other = fixture("other-ca.pem");
    let o = shell(&[&server.endpoint, "--tls-ca", &other], "namespaces\n");
    assert_eq!(o.status.code(), Some(4));
    assert!(stderr(&o).contains("UnknownIssuer"), "{}", stderr(&o));
    // TLS settings with http:// are a usage error
    let plain = server.endpoint.replace("https://", "http://");
    let o = shell(&[&plain, "--tls-ca", &ca], "namespaces\n");
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
}
