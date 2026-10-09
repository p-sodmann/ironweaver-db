//! The `iwdb-server` binary: it serves the data directory of its config
//! file or environment, shuts down gracefully on SIGTERM (checkpointing, so
//! the next open replays nothing), refuses a bad command line or
//! configuration with every problem, logs JSON lines, and becomes ready
//! only once recovery has finished. Authentication is on (the default): the
//! tests bootstrap an admin and log in; one checks that no secret (nor a
//! private key, nor a value of the data) reaches the logs or the audit
//! file (step 15c). TLS is on (the default, step 15b) with
//! the test certificate of `tests/fixtures/tls`; SIGHUP reloads it.
//!
//! On Windows (ADR 0058) the server runs in a process group of its own and
//! shuts down on Ctrl-Break, which [`shut_down`] sends; SIGHUP has no
//! counterpart there, so the reload tests are Unix only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use iwdb::{Mutation, Store};
use iwdb_query::exec::block_on;
use iwdb_query::{Accounts, Admin, Code, CommitOptions, Database, Secret};
use iwdb_server::client::{ClientTls, Remote};

const BIN: &str = env!("CARGO_BIN_EXE_iwdb-server");

/// A test certificate or key (test-only, public).
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tls").join(name)
}

/// The server's certificate and key: the test ones.
fn tls(command: &mut Command) -> &mut Command {
    command.env("IWDB_TLS_CERT", fixture("server.pem")).env("IWDB_TLS_KEY", fixture("server.key"))
}

/// A client of the server at `address` over TLS, trusting the test CA.
fn client(address: &str) -> Remote {
    let tls = ClientTls { ca: Some(fixture("ca.pem")), ..ClientTls::default() };
    Remote::connect_tls(&format!("https://{}", address), &tls).unwrap()
}
/// The bootstrap admin's password of these tests.
const ADMIN_PASSWORD: &str = "admin-password-for-tests";

/// The command with the first admin from the bootstrap variable, over
/// TLS.
fn bin() -> Command {
    let mut command = Command::new(BIN);
    command.env("IWDB_AUTH_BOOTSTRAP_PASSWORD", ADMIN_PASSWORD);
    tls(&mut command);
    command
}

/// A client of `address`, logged in as the bootstrap admin.
fn admin(address: &str) -> Remote {
    let remote = client(address);
    block_on(remote.login("admin", Secret::new(ADMIN_PASSWORD))).unwrap();
    remote
}

/// A log line's JSON object (stderr isn't a terminal: JSON by default).
fn event(line: &str) -> serde_json::Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not a JSON log line ({}): {}", e, line))
}

/// The address of a `serving` event: the server is ready.
fn serving(line: &str) -> Option<String> {
    let e = event(line);
    e["message"].as_str()?.starts_with("serving").then(|| e["address"].as_str().unwrap().to_owned())
}

/// Start `command` and read its stderr on a thread: the lines arrive on the
/// receiver.
fn spawn(command: &mut Command) -> (std::process::Child, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
    // A process group of its own, so that Ctrl-Break reaches the server
    // alone (`shut_down`)
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(command, CREATE_NEW_PROCESS_GROUP);
    let mut child = command.stderr(Stdio::piped()).spawn().unwrap();
    let (lines_tx, lines) = mpsc::channel::<String>();
    let stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let _ = lines_tx.send(line.unwrap());
        }
    });
    (child, lines, reader)
}

/// Wait for the `serving` event; the lines before it are kept in `seen`.
fn ready_address(lines: &mpsc::Receiver<String>, seen: &mut Vec<String>) -> String {
    loop {
        let line = lines.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| panic!("not ready: {:?}", seen));
        let address = serving(&line);
        seen.push(line);
        if let Some(address) = address {
            return address;
        }
    }
}

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    }
}

#[test]
fn serves_its_data_directory_and_shuts_down_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    std::fs::write(&config, "data_dir = \"data\"\nlisten = \"127.0.0.1:0\"\n[server]\ndrain_timeout_secs = 5\n")
        .unwrap();
    let (mut child, lines, reader) = spawn(bin().arg("--config").arg(&config));
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    assert!(seen.iter().any(|l| event(l)["message"].as_str().unwrap().starts_with("created the first admin")));
    // Listening (recovering) comes first, then ready
    assert!(event(&seen[0])["message"].as_str().unwrap().starts_with("listening"), "{:?}", seen);
    assert!(seen.iter().any(|l| event(l)["message"] == "recovery finished"), "{:?}", seen);

    let anonymous = client(&address);
    let e = block_on(anonymous.commit("default", vec![node("a")], CommitOptions::default())).unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    let remote = admin(&address);
    let seq = block_on(remote.commit("default", vec![node("a")], CommitOptions::default())).unwrap().seq;
    assert_eq!(seq, 1);
    iwdb_server::health::probe(&address, true, Duration::from_secs(5)).unwrap();
    let probe = Command::new(BIN).arg("--probe").arg(format!("https://{}", address)).output().unwrap();
    assert!(probe.status.success(), "{:?}", probe);
    // The configuration's server: TLS as configured
    let probe = tls(Command::new(BIN).arg("--probe").env("IWDB_DATA_DIR", "unused").env("IWDB_LISTEN", &address))
        .output()
        .unwrap();
    assert!(probe.status.success(), "{:?}", probe);
    let plain = Command::new(BIN).arg("--probe").arg(format!("http://{}", address)).output().unwrap();
    assert_eq!(plain.status.code(), Some(1), "{:?}", plain);
    let bare = Command::new(BIN).arg("--probe").arg(&address).output().unwrap();
    assert_eq!(bare.status.code(), Some(2), "{:?}", bare);

    shut_down(&child);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let output: Vec<String> = lines.try_iter().collect();
    assert!(status.success(), "{:?}: {:?}", status, output);
    assert!(output.iter().any(|l| event(l)["message"].as_str().unwrap().starts_with("closed")), "{:?}", output);
    // Every line is JSON with the fields a collector needs
    for line in &output {
        let e = event(line);
        assert!(e["timestamp"].is_string() && e["level"].is_string() && e["target"].is_string(), "{}", line);
    }

    // The data is there, and the checkpoint at shutdown leaves nothing to replay
    let store = Store::open(&dir.path().join("data"), Default::default()).unwrap();
    assert!(store.node("a").is_some());
    let status = store.namespace("default").unwrap().status();
    assert_eq!((status.recovery.checkpoint, status.recovery.replayed), (Some(1), 0));
    store.close().unwrap();
}

#[test]
fn a_bad_command_line_or_config_exits_with_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let config = dir.path().join("bad.toml");
    std::fs::write(&config, "data_dir = \"d\"\nfsync = \"always\"\n").unwrap();
    let out = Command::new(BIN).arg("--config").arg(&config).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("fsync"), "{}", String::from_utf8_lossy(&out.stderr));
    let help = Command::new(BIN).arg("--help").output().unwrap();
    assert!(help.status.success() && String::from_utf8_lossy(&help.stdout).contains("--config"));
}

/// A projection from the config file (step 13, ADR 0032): the server
/// follows a Postgres table (PGlite, `scripts/pglite.sh`; skipped without
/// `IWDB_TEST_POSTGRES_URL`), its mark shows in the namespace status, and
/// after a restart it goes on from the mark.
#[cfg(feature = "postgres")]
#[test]
fn runs_the_projections_of_its_config() {
    let Some(url) = std::env::var("IWDB_TEST_POSTGRES_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("IWDB_TEST_POSTGRES_URL is not set: skipped (start scripts/pglite.sh)");
        return;
    };
    let mut pg = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    pg.batch_execute(
        "DROP TABLE IF EXISTS server_events;
         CREATE TABLE server_events (id bigserial PRIMARY KEY, kind text NOT NULL, who text);
         INSERT INTO server_events (kind, who) VALUES ('hello', 'ann'), ('hello', 'bob'), ('noise', NULL)",
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    std::fs::write(
        &config,
        r#"data_dir = "data"
listen = "127.0.0.1:0"
[server]
drain_timeout_secs = 5

[[projection]]
name = "greetings"
poll_ms = 20
[projection.source]
kind = "postgres"
url_env = "IWDB_TEST_POSTGRES_URL"
table = "server_events"
position = "id"
[[projection.rule]]
when = { kind = "hello" }
mutations = [{ upsert_node = { id = "${who}", labels = ["Person"], attr = { greeted_at = "${id}" } } }]
"#,
    )
    .unwrap();
    let run = |expected_mark: u64| {
        let (mut child, lines, reader) = spawn(bin().arg("--config").arg(&config));
        let address = ready_address(&lines, &mut Vec::new());
        let remote = admin(&address);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let marks = loop {
            let status = block_on(remote.namespace_status("default")).unwrap();
            if status.marks.first().is_some_and(|m| m.position == expected_mark) || std::time::Instant::now() > deadline
            {
                break status.marks;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(marks.len(), 1, "{:?}", marks);
        assert_eq!((marks[0].name.as_str(), marks[0].position), ("greetings", expected_mark));
        shut_down(&child);
        let status = child.wait().unwrap();
        reader.join().unwrap();
        assert!(status.success(), "{:?}: {:?}", status, lines.try_iter().collect::<Vec<_>>());
    };
    run(3);
    pg.batch_execute("INSERT INTO server_events (kind, who) VALUES ('hello', 'ann')").unwrap();
    run(4);
    let store = Store::open(&dir.path().join("data"), Default::default()).unwrap();
    assert_eq!(store.node("ann").unwrap().attr.get("greeted_at"), Some(&iwdb::Value::Int(4)));
    assert_eq!(store.node("bob").unwrap().attr.get("greeted_at"), Some(&iwdb::Value::Int(2)));
    assert_eq!(store.default_namespace().status().nodes, 2);
    store.close().unwrap();
}

/// `--version` names what the build has (ADR 0034); a build without
/// `postgres` refuses a Postgres projection when it reads its config.
#[test]
fn the_version_lists_the_features() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && text.contains("(grpc"), "{}", text);
    assert_eq!(text.contains("rest"), cfg!(feature = "rest"), "{}", text);
    assert_eq!(text.contains("postgres"), cfg!(feature = "postgres"), "{}", text);
    assert_eq!(text.contains("otel"), cfg!(feature = "otel"), "{}", text);
    // A build without traces refuses a config that turns them on (ADR 0057)
    #[cfg(not(feature = "otel"))]
    {
        let dir = tempfile::tempdir().unwrap();
        let out =
            bin().env("IWDB_DATA_DIR", dir.path().join("d")).env("IWDB_TRACING_ENABLED", "true").output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("[tracing] enabled (from IWDB_TRACING_ENABLED)") && stderr.contains("otel feature"),
            "{}",
            stderr
        );
        assert!(!dir.path().join("d").exists(), "the store was opened");
    }
    #[cfg(not(feature = "postgres"))]
    {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("server.toml");
        let text = "data_dir = \"d\"\n[[projection]]\nname = \"p\"\n[projection.source]\nkind = \"postgres\"\nurl = \"host=x\"\ntable = \"t\"\nposition = \"id\"\n";
        std::fs::write(&config, text).unwrap();
        let out = Command::new(BIN).arg("--config").arg(&config).output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("postgres feature"));
        assert!(!dir.path().join("d").exists(), "the store was opened");
    }
}

/// `CREATE_NEW_PROCESS_GROUP` (winbase.h).
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Ask the server to shut down gracefully: SIGTERM on Unix; on Windows
/// Ctrl-Break to its process group (`spawn` made one), sent by PowerShell
/// with `GenerateConsoleCtrlEvent` (no unsafe code here), which works where
/// the tests and the server share a console.
fn shut_down(child: &std::process::Child) {
    #[cfg(unix)]
    let sent = Command::new("kill").arg("-TERM").arg(child.id().to_string()).output().unwrap();
    #[cfg(windows)]
    let sent = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "$k = Add-Type -Name Console -Namespace Iwdb -PassThru -MemberDefinition '[DllImport(\"kernel32.dll\", \
             SetLastError = true)] public static extern bool GenerateConsoleCtrlEvent(uint e, uint group);'; \
             if (-not $k::GenerateConsoleCtrlEvent(1, {})) {{ \
             Write-Error ('GenerateConsoleCtrlEvent: ' + [Runtime.InteropServices.Marshal]::GetLastWin32Error()); \
             exit 1 }}",
            child.id()
        ))
        .output()
        .unwrap();
    assert!(sent.status.success(), "{:?}", sent);
}

/// No file: `IWDB_DATA_DIR` and the other variables are the whole
/// configuration (ADR 0039), as in a container.
#[test]
fn runs_from_the_environment_alone() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let (mut child, lines, reader) = spawn(
        bin()
            .env("IWDB_DATA_DIR", &data)
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5")
            .env("IWDB_STORE_FSYNC", "group")
            .env("IWDB_LOG_LEVEL", "debug"),
    );
    let address = ready_address(&lines, &mut Vec::new());
    let remote = admin(&address);
    block_on(remote.commit("default", vec![node("e")], CommitOptions::default())).unwrap();
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    let store = Store::open(&data, Default::default()).unwrap();
    assert!(store.node("e").is_some());
    store.close().unwrap();
}

/// Every problem at once, each with where it came from, before the store
/// opens (exit 2).
#[test]
fn a_bad_configuration_lists_every_problem() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    std::fs::write(&config, "data_dir = \"d\"\n[store]\nfsync = \"group\"\ngroup_max_batch = 0\n").unwrap();
    let out = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .env("IWDB_STORE_FSINC", "off")
        .env("IWDB_LIMITS_MAX_TIMEOUT_MS", "soon")
        .env("IWDB_SERVER_MAX_MESSAGE_BYTES", "10")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8_lossy(&out.stderr);
    for expected in [
        "IWDB_STORE_FSINC: no such setting",
        "IWDB_LIMITS_MAX_TIMEOUT_MS: expected a whole number",
        "[store] group_max_batch must be at least 1",
        "[server] max_message_bytes (from IWDB_SERVER_MAX_MESSAGE_BYTES) must be at least 1024",
        "server.toml",
    ] {
        assert!(text.contains(expected), "{:?} in {}", expected, text);
    }
    assert!(!dir.path().join("d").exists(), "the store was opened");
    // No data directory anywhere
    let out = Command::new(BIN).env_remove("IWDB_DATA_DIR").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("data_dir is required"));
}

/// `--check-config` prints the effective settings with their sources, as
/// TOML that reads back to the same configuration.
#[test]
fn check_config_prints_the_effective_settings() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    // Absolute on the platform (Windows needs a drive)
    let data = if cfg!(windows) { "C:/var/lib/iwdb" } else { "/var/lib/iwdb" };
    std::fs::write(&config, format!("data_dir = \"{}\"\n[store]\nretain_records = 7\n", data)).unwrap();
    let out = tls(Command::new(BIN).arg("--check-config").arg("--config").arg(&config).env("IWDB_STORE_FSYNC", "off"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    let text = String::from_utf8(out.stdout).unwrap();
    let line = |key: &str| text.lines().find(|l| l.starts_with(&format!("{} =", key))).unwrap().to_owned();
    assert!(line("fsync").contains("\"off\"") && line("fsync").ends_with("# IWDB_STORE_FSYNC"), "{}", text);
    assert!(line("retain_records").ends_with("# file"), "{}", text);
    assert!(line("listen").ends_with("# default"), "{}", text);
    assert!(line("cert").contains("server.pem") && line("cert").ends_with("# IWDB_TLS_CERT"), "{}", text);
    let back = iwdb_server::config::Config::parse(&text).unwrap();
    assert_eq!(back.store.fsync, iwdb_server::config::Fsync::Off);
    assert_eq!(back.store.retain_records, 7);
    assert_eq!(back.data_dir, std::path::PathBuf::from(data));
}

/// Readiness against a real recovery (design rule 3): a WAL large enough
/// that replaying it takes a while. The port answers at once, not ready;
/// the first ready answer comes with every committed node visible.
#[test]
fn ready_only_after_a_large_recovery() {
    const BATCHES: usize = 100;
    const BATCH: usize = 1000;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut options = iwdb::StoreOptions::default();
        options.wal.fsync = iwdb::FsyncPolicy::Off;
        options.checkpoint.on_close = false;
        options.checkpoint.wal_size = None;
        options.checkpoint.interval = None;
        let store = Store::open(&data, options).unwrap();
        for b in 0..BATCHES {
            let batch: Vec<Mutation> = (0..BATCH).map(|i| node(&format!("n{}-{}", b, i))).collect();
            store.commit(&batch).unwrap();
        }
        store.close().unwrap();
    }
    let (mut child, lines, reader) = spawn(
        bin().env("IWDB_DATA_DIR", &data).env("IWDB_LISTEN", "127.0.0.1:0").env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5"),
    );
    let listening = event(&lines.recv_timeout(Duration::from_secs(30)).unwrap());
    assert!(listening["message"].as_str().unwrap().starts_with("listening"), "{}", listening);
    let address = listening["address"].as_str().unwrap().to_owned();
    let mut not_ready = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while iwdb_server::health::probe(&address, true, Duration::from_secs(5)).is_err() {
        not_ready += 1;
        assert!(std::time::Instant::now() < deadline, "never ready");
    }
    let remote = admin(&address);
    let status = block_on(remote.namespace_status("default")).unwrap();
    assert_eq!(status.nodes, BATCHES * BATCH);
    assert_eq!(status.recovery.replayed, BATCHES as u64);
    assert!(not_ready > 0, "recovery was too quick to see it unready");
    let mut seen = Vec::new();
    ready_address(&lines, &mut seen);
    let recovered = seen.iter().map(|l| event(l)).find(|e| e["message"] == "recovered").unwrap();
    assert_eq!(recovered["replayed"], BATCHES as u64);
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
}

/// With authentication on, a store without users refuses to start, and the
/// message says how to make the first admin (ADR 0047). A bootstrap
/// variable on a store with users is ignored with a warning.
#[test]
fn a_store_without_users_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = tls(Command::new(BIN).env("IWDB_DATA_DIR", &data).env("IWDB_LISTEN", "127.0.0.1:0")).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("has no users") && text.contains("iwctl user create"), "{}", text);
    assert!(text.contains("IWDB_AUTH_BOOTSTRAP_PASSWORD"), "{}", text);
    // A user from elsewhere (iwctl, offline): it starts, and ignores the bootstrap
    let store = Store::open(&data, Default::default()).unwrap();
    store.users().create("root", &Secret::new("root-password"), true).unwrap();
    store.close().unwrap();
    let (mut child, lines, reader) = spawn(
        bin().env("IWDB_DATA_DIR", &data).env("IWDB_LISTEN", "127.0.0.1:0").env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5"),
    );
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    assert!(
        seen.iter().any(|l| event(l)["level"] == "WARN" && l.contains("IWDB_AUTH_BOOTSTRAP_PASSWORD")),
        "{:?}",
        seen
    );
    let remote = client(&address);
    assert_eq!(block_on(remote.login("admin", Secret::new(ADMIN_PASSWORD))).unwrap_err().code(), Code::Unauthenticated);
    block_on(remote.login("root", Secret::new("root-password"))).unwrap();
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    // Off, no users are needed (and in plaintext on loopback, which needs
    // one flag)
    let empty = dir.path().join("empty");
    let (mut child, lines, reader) = spawn(
        Command::new(BIN)
            .env("IWDB_DATA_DIR", &empty)
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_AUTH_ENABLED", "false")
            .env("IWDB_TLS_ENABLED", "false")
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5"),
    );
    let address = ready_address(&lines, &mut Vec::new());
    let remote = Remote::connect(&format!("http://{}", address)).unwrap();
    block_on(remote.commit("default", vec![node("x")], CommitOptions::default())).unwrap();
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
}

/// Send SIGHUP to the server, and wait for its log line that contains
/// `expected`.
#[cfg(unix)]
fn hangup(child: &std::process::Child, lines: &mpsc::Receiver<String>, seen: &mut Vec<String>, expected: &str) {
    let sent = Command::new("kill").arg("-HUP").arg(child.id().to_string()).status().unwrap();
    assert!(sent.success());
    loop {
        let line =
            lines.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("no {:?}: {:?}", expected, seen));
        let found = line.contains(expected);
        seen.push(line);
        if found {
            return;
        }
    }
}

/// No password, token or private key reaches the logs (steps 15a and
/// 15b): the server logs at `debug` through logins, failed logins, a token,
/// a password change, a failed password change, and reloads of a key that
/// fail (another certificate's key, a cut-off one), and no line holds any
/// of the secrets.
#[test]
fn no_secret_reaches_the_logs() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::copy(fixture("server.pem"), &cert).unwrap();
    std::fs::copy(fixture("server.key"), &key).unwrap();
    let (mut child, lines, reader) = spawn(
        bin()
            .env("IWDB_DATA_DIR", dir.path().join("data"))
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_LOG_LEVEL", "debug")
            .env("IWDB_TLS_CERT", &cert)
            .env("IWDB_TLS_KEY", &key)
            .env("IWDB_AUDIT_DIR", dir.path().join("audit"))
            .env("IWDB_LOG_TAIL_EVENTS", "100000")
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5"),
    );
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    let remote = admin(&address);
    // Data and a catalog change: their values must not reach the audit log
    // (nor any other line)
    let data = "data-value-not-for-the-log";
    let mut valued = node("n1");
    if let Mutation::UpsertNode { attr, .. } = &mut valued {
        attr.insert("note".into(), iwdb::Value::String(data.into()));
    }
    block_on(remote.commit("default", vec![valued], CommitOptions::default())).unwrap();
    let path = iwdb_engine::catalog::AttrPath::new(["note"]).unwrap();
    let index = iwdb_engine::CatalogChange::CreateIndex(iwdb_engine::catalog::IndexDef { path });
    block_on(remote.commit_catalog("default", index, CommitOptions::default())).unwrap();
    let mut secrets = vec![ADMIN_PASSWORD.to_owned(), remote.token().unwrap().expose().to_owned(), data.to_owned()];
    block_on(remote.create_user("ann", Secret::new("ann-secret-password"), false)).unwrap();
    secrets.push("ann-secret-password".into());
    let token = block_on(remote.create_token("ann", "ci", None)).unwrap();
    secrets.push(token.token.expose().to_owned());
    let ann = client(&address);
    for wrong in ["wrong-password-one", "wrong-password-two"] {
        assert_eq!(block_on(ann.login("ann", Secret::new(wrong))).unwrap_err().code(), Code::Unauthenticated);
        secrets.push(wrong.into());
    }
    let session = block_on(ann.login("ann", Secret::new("ann-secret-password"))).unwrap();
    secrets.push(session.token.expose().to_owned());
    let e = block_on(ann.set_password("ann", Secret::new("new-ann-password"), Some(Secret::new("not-the-current"))))
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    secrets.extend(["new-ann-password".to_owned(), "not-the-current".to_owned()]);
    block_on(ann.set_password("ann", Secret::new("new-ann-password"), Some(Secret::new("ann-secret-password"))))
        .unwrap();
    // A garbage bearer token and the right one, over REST too
    let bad = client(&address).with_token(Secret::new("iwdb_not-a-real-token"));
    assert_eq!(block_on(bad.namespaces()).unwrap_err().code(), Code::Unauthenticated);
    secrets.push("iwdb_not-a-real-token".into());
    #[cfg(feature = "rest")]
    {
        let tls = ClientTls { ca: Some(fixture("ca.pem")), ..ClientTls::default() };
        let rest = iwdb_server::client::RestRemote::connect_tls(&format!("https://{}", address), &tls).unwrap();
        assert!(block_on(rest.login("ann", Secret::new("wrong-password-rest"))).is_err());
        secrets.push("wrong-password-rest".into());
    }
    // Private keys: reloads that fail, then one that works (SIGHUP: Unix only)
    let (own, other) =
        (std::fs::read_to_string(&key).unwrap(), std::fs::read_to_string(fixture("client-ann.key")).unwrap());
    #[cfg(unix)]
    {
        std::fs::write(&key, &other).unwrap();
        hangup(&child, &lines, &mut seen, "reloading TLS failed");
        std::fs::write(&key, &own[..own.len() / 2]).unwrap();
        hangup(&child, &lines, &mut seen, "reloading TLS failed");
        std::fs::write(&key, &own).unwrap();
        hangup(&child, &lines, &mut seen, "reloaded the TLS certificate");
    }
    for pem in [&own, &other] {
        // The base64 lines of the keys (long enough not to match by chance)
        secrets.extend(pem.lines().filter(|l| !l.starts_with("-----") && l.len() >= 16).map(str::to_owned));
    }
    // A failed TLS handshake (a client of another CA) is logged at debug
    let other_ca = ClientTls { ca: Some(fixture("other-ca.pem")), ..ClientTls::default() };
    let stranger = Remote::connect_tls(&format!("https://{}", address), &other_ca).unwrap();
    assert_eq!(block_on(stranger.namespaces()).unwrap_err().code(), Code::Unavailable);
    // The log tail (step 16c): every event so far, page by page
    let mut tail = Vec::new();
    loop {
        let page = block_on(remote.log(tail.last().map_or(0, |e: &iwdb_query::log::LogEvent| e.seq), None)).unwrap();
        let more = page.missed && !page.events.is_empty();
        tail.extend(page.events);
        if !more {
            break;
        }
    }
    let tail_text: Vec<String> = tail.iter().map(|e| format!("{} {} {:?}", e.target, e.message, e.fields)).collect();
    assert!(tail.len() > 20 && tail.first().map(|e| e.seq) == Some(1), "the tail holds every event: {:?}", tail_text);
    assert!(tail_text.iter().any(|l| l.contains("failed login") && l.contains("ann")), "{:?}", tail_text);
    assert!(
        tail.iter().any(|e| e.target == "iwdb::audit" && e.fields.contains(&("operation".into(), "CreateUser".into())))
    );
    #[cfg(unix)]
    assert!(tail_text.iter().any(|l| l.contains("reloading TLS failed")), "{:?}", tail_text);
    for line in &tail_text {
        for secret in &secrets {
            assert!(!line.contains(secret.as_str()), "a secret in the log tail: {}", line);
        }
    }
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    seen.extend(lines.try_iter());
    assert!(seen.iter().any(|l| l.contains("failed login") && l.contains("ann")), "failed logins are logged");
    assert!(seen.iter().any(|l| l.contains("TLS handshake failed")), "failed handshakes are logged");
    // The audit entries (step 15c): in the log, and the same in the audit
    // file
    let audit: Vec<serde_json::Value> =
        seen.iter().map(|l| event(l)).filter(|e| e["target"] == "iwdb::audit").collect();
    let has = |operation: &str, outcome: &str, check: &dyn Fn(&serde_json::Value) -> bool| {
        audit.iter().any(|e| e["operation"] == operation && e["outcome"] == outcome && check(e))
    };
    let local = |e: &serde_json::Value| e["client"] == "127.0.0.1";
    assert!(has("Login", "success", &|e| e["user"] == "admin" && e["auth"] == "session" && local(e)), "{:?}", audit);
    assert!(
        has("Login", "failure", &|e| e["user"] == "ann" && e["code"] == "unauthenticated" && local(e)),
        "failed logins are audited with the client: {:?}",
        audit
    );
    assert!(has("CreateUser", "success", &|e| e["subject"] == "ann" && e["user"] == "admin" && e["admin"] == false));
    assert!(has("CreateToken", "success", &|e| e["subject"] == "ann" && e["token_name"] == "ci"));
    assert!(has("SetPassword", "failure", &|e| e["user"] == "ann" && e["code"] == "unauthenticated"));
    assert!(has("SetPassword", "success", &|e| e["user"] == "ann" && e["subject"] == "ann"));
    assert!(has("CommitCatalog", "success", &|e| e["namespace"] == "default" && e["seq"].is_u64()));
    assert!(
        has("ListNamespaces", "failure", &|e| e["code"] == "unauthenticated" && e["user"].is_null() && local(e)),
        "refusals are audited with the client: {:?}",
        audit
    );
    assert!(!audit.iter().any(|e| e["operation"] == "Commit"), "data commits aren't audited");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir.path().join("audit")).unwrap() {
        files.extend(std::fs::read_to_string(entry.unwrap().path()).unwrap().lines().map(str::to_owned));
    }
    assert_eq!(files.len(), audit.len(), "the audit file holds the log's audit entries");
    for line in &files {
        assert_eq!(event(line)["target"], "iwdb::audit", "{}", line);
    }
    for line in seen.iter().chain(&files) {
        for secret in &secrets {
            assert!(!line.contains(secret.as_str()), "a secret in the log: {}", line);
        }
    }
}

/// The certificate a TLS server at `address` presents.
#[cfg(unix)]
fn presented(address: &str) -> Vec<u8> {
    let tls = ClientTls { ca: Some(fixture("ca.pem")), ..ClientTls::default() };
    let config = std::sync::Arc::new(tls.rustls_config(&[b"h2"]).unwrap());
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let mut connection = rustls::ClientConnection::new(config, name).unwrap();
    let mut tcp = std::net::TcpStream::connect(address).unwrap();
    while connection.is_handshaking() {
        connection.complete_io(&mut tcp).unwrap();
    }
    connection.peer_certificates().unwrap()[0].to_vec()
}

#[cfg(unix)]
fn der(name: &str) -> Vec<u8> {
    use rustls_pki_types::pem::PemObject;
    rustls_pki_types::CertificateDer::from_pem_file(fixture(name)).unwrap().to_vec()
}

/// SIGHUP reloads the certificate (ADR 0048): new connections get the new
/// one, a reload that fails keeps it, and the server goes on serving.
/// Unix only: Windows has no SIGHUP, and rotating the certificate there
/// means a restart (ADR 0058).
#[cfg(unix)]
#[test]
fn sighup_reloads_the_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::copy(fixture("server.pem"), &cert).unwrap();
    std::fs::copy(fixture("server.key"), &key).unwrap();
    let (mut child, lines, reader) = spawn(
        bin()
            .env("IWDB_DATA_DIR", dir.path().join("data"))
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_TLS_CERT", &cert)
            .env("IWDB_TLS_KEY", &key)
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5"),
    );
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    assert!(seen.iter().any(|l| event(l)["scheme"] == "TLS"), "{:?}", seen);
    assert_eq!(presented(&address), der("server.pem"));
    let remote = admin(&address);
    std::fs::copy(fixture("server-renewed.pem"), &cert).unwrap();
    std::fs::copy(fixture("server-renewed.key"), &key).unwrap();
    hangup(&child, &lines, &mut seen, "reloaded the TLS certificate");
    assert_eq!(presented(&address), der("server-renewed.pem"));
    std::fs::write(&cert, "garbage").unwrap();
    hangup(&child, &lines, &mut seen, "reloading TLS failed");
    assert_eq!(presented(&address), der("server-renewed.pem"));
    block_on(remote.commit("default", vec![node("after")], CommitOptions::default())).unwrap();
    admin(&address);
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
}

/// TLS is the default: without a certificate the server doesn't start;
/// plaintext needs `[tls] enabled = false`, and on a non-loopback address
/// also `[server] plaintext_public` (ADR 0048). A certificate that can't
/// be used stops it before the store opens.
#[test]
fn plaintext_needs_explicit_flags_and_tls_needs_a_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let run = |vars: &[(&str, &str)]| {
        let mut command = Command::new(BIN);
        command.env("IWDB_DATA_DIR", &data).env("IWDB_AUTH_BOOTSTRAP_PASSWORD", ADMIN_PASSWORD);
        for (k, v) in vars {
            command.env(k, v);
        }
        let out = command.output().unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (code, text) = run(&[("IWDB_LISTEN", "127.0.0.1:0")]);
    assert_eq!(code, Some(2), "{}", text);
    assert!(text.contains("[tls] cert and [tls] key are not set, and TLS is on"), "{}", text);
    for public in ["0.0.0.0:0", "[::]:0"] {
        let (code, text) = run(&[("IWDB_LISTEN", public), ("IWDB_TLS_ENABLED", "false")]);
        assert_eq!(code, Some(2), "{}", text);
        assert!(text.contains("not a loopback address, and TLS is off"), "{}", text);
        assert!(text.contains("IWDB_SERVER_PLAINTEXT_PUBLIC"), "{}", text);
        // The second flag alone isn't enough
        let (code, text) = run(&[("IWDB_LISTEN", public), ("IWDB_SERVER_PLAINTEXT_PUBLIC", "true")]);
        assert_eq!(code, Some(2), "{}", text);
        assert!(text.contains("TLS is on"), "{}", text);
    }
    let missing = dir.path().join("missing.pem");
    let (code, text) = run(&[
        ("IWDB_LISTEN", "127.0.0.1:0"),
        ("IWDB_TLS_CERT", missing.to_str().unwrap()),
        ("IWDB_TLS_KEY", fixture("server.key").to_str().unwrap()),
    ]);
    assert_eq!(code, Some(2), "{}", text);
    assert!(text.contains("can't read the TLS certificate") && text.contains("missing.pem"), "{}", text);
    assert!(!data.exists(), "the store was opened");
}

/// The process's internet sockets, as `lsof` lists them: `(protocol,
/// name)`, the name `local` or `local->remote`. `None` without `lsof`.
#[cfg(not(windows))]
fn sockets(pid: u32) -> Option<Vec<(String, String)>> {
    let output = Command::new("lsof").args(["-a", "-n", "-P", "-i", "-F", "Pn", "-p"]).arg(pid.to_string()).output();
    let output = output.ok()?;
    let mut sockets = Vec::new();
    let mut protocol = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(p) = line.strip_prefix('P') {
            protocol = p.to_owned();
        } else if let Some(name) = line.strip_prefix('n') {
            sockets.push((protocol.clone(), name.to_owned()));
        }
    }
    Some(sockets)
}

/// The same from `netstat -ano` on Windows (ADR 0058): a listener's name is
/// its local address, a connection's `local->remote`.
#[cfg(windows)]
fn sockets(pid: u32) -> Option<Vec<(String, String)>> {
    let output = Command::new("netstat").arg("-ano").output().ok()?;
    let pid = pid.to_string();
    let mut sockets = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["TCP", local, _, "LISTENING", p] if *p == pid => sockets.push(("TCP".into(), (*local).to_owned())),
            ["TCP", local, remote, _, p] if *p == pid => sockets.push(("TCP".into(), format!("{}->{}", local, remote))),
            ["UDP", local, _, p] if *p == pid => sockets.push(("UDP".into(), (*local).to_owned())),
            _ => {}
        }
    }
    Some(sockets)
}

/// SECURITY.md's promise (step 15c): without projections, the server opens
/// no connection of its own. Through logins, user, token, namespace and
/// catalog changes, commits and refusals, its only internet sockets are
/// its listener and the connections to it: TCP, local port the listen
/// port. No UDP either (no DNS lookups, no telemetry).
#[test]
fn opens_no_connection_it_was_not_configured_for() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, lines, reader) = spawn(
        bin()
            .env("IWDB_DATA_DIR", dir.path().join("data"))
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5"),
    );
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    let port = address.rsplit(':').next().unwrap().to_owned();
    let remote = admin(&address);
    block_on(remote.create_user("ann", Secret::new("ann-password-1"), false)).unwrap();
    block_on(remote.create_token("ann", "ci", None)).unwrap();
    block_on(remote.create_namespace("other", None)).unwrap();
    block_on(remote.grant("ann", "other", iwdb_query::Role::Read)).unwrap();
    block_on(remote.commit("other", vec![node("a")], CommitOptions::default())).unwrap();
    let path = iwdb_engine::catalog::AttrPath::new(["x"]).unwrap();
    let index = iwdb_engine::CatalogChange::CreateIndex(iwdb_engine::catalog::IndexDef { path });
    block_on(remote.commit_catalog("other", index, CommitOptions::default())).unwrap();
    assert!(block_on(client(&address).login("ann", Secret::new("wrong-password"))).is_err());
    assert!(block_on(client(&address).namespaces()).is_err());
    block_on(remote.drop_namespace("other", None)).unwrap();
    // Give anything started in the background a moment
    std::thread::sleep(Duration::from_millis(300));
    let listed = sockets(child.id());
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    let Some(listed) = listed else {
        assert!(std::env::var_os("CI").is_none(), "lsof (netstat on Windows) is needed on CI");
        eprintln!("skipped: no lsof");
        return;
    };
    assert!(!listed.is_empty(), "lsof (netstat) listed nothing: is it working?");
    let local_port = |name: &str| name.split("->").next().unwrap().rsplit(':').next().unwrap().to_owned();
    for (protocol, name) in &listed {
        assert_eq!(protocol, "TCP", "{}", name);
        assert_eq!(local_port(name), port, "a socket not of the listener: {} ({:?})", name, listed);
    }
}

/// `GET path` over HTTP/1.1 and TLS, with `token` as a bearer token: the
/// status, the content type and the body.
fn https_get(address: &str, path: &str, token: Option<&str>) -> (u16, String, String) {
    use std::io::{Read, Write};
    let tls = ClientTls { ca: Some(fixture("ca.pem")), ..ClientTls::default() };
    let config = std::sync::Arc::new(tls.rustls_config(&[b"http/1.1"]).unwrap());
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let connection = rustls::ClientConnection::new(config, name).unwrap();
    let mut stream = rustls::StreamOwned::new(connection, std::net::TcpStream::connect(address).unwrap());
    let auth = token.map(|t| format!("authorization: Bearer {}\r\n", t)).unwrap_or_default();
    write!(stream, "GET {} HTTP/1.1\r\nhost: localhost\r\n{}connection: close\r\n\r\n", path, auth).unwrap();
    let mut answer = Vec::new();
    // A server that closes without TLS's close_notify ends the read with an error
    let _ = stream.read_to_end(&mut answer);
    let answer = String::from_utf8(answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let media = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-type:").map(|v| v.trim().to_owned()))
        .unwrap_or_default();
    (status, media, body.to_owned())
}

/// The binary serves `GET /metrics` to a caller with a token, in
/// Prometheus' text format, with every metric of metrics.md; without a
/// token it is refused (step 16c, ADR 0050).
#[test]
fn serves_the_metrics_to_prometheus() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, lines, reader) =
        spawn(bin().env("IWDB_DATA_DIR", dir.path().join("data")).env("IWDB_LISTEN", "127.0.0.1:0"));
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    let remote = admin(&address);
    block_on(remote.commit("default", vec![node("n1")], CommitOptions::default())).unwrap();
    let token = block_on(remote.create_token("admin", "prometheus", None)).unwrap().token;

    let (status, media, body) = https_get(&address, "/metrics", None);
    assert_eq!((status, media.as_str()), (401, "application/json"), "{}", body);
    assert!(body.contains("\"code\":\"unauthenticated\""), "{}", body);

    let (status, media, text) = https_get(&address, "/metrics", Some(token.expose()));
    assert_eq!(status, 200, "{}", text);
    assert_eq!(media, "text/plain; version=0.0.4; charset=utf-8");
    let names: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("# TYPE ")?.split(' ').next()).collect();
    let documented: Vec<&str> = iwdb_query::metrics::METRICS.iter().map(|d| d.name).collect();
    assert_eq!(names, documented, "every documented metric is exported, and only those");
    // Every sample belongs to one of them
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let series = line.split(['{', ' ']).next().unwrap();
        let family = ["_bucket", "_sum", "_count"].iter().fold(series, |s, suffix| s.strip_suffix(suffix).unwrap_or(s));
        assert!(names.contains(&series) || names.contains(&family), "{}", line);
    }
    assert!(text.contains("iwdb_namespace_nodes{namespace=\"default\"} 1\n"), "{}", text);
    assert!(text.contains("iwdb_requests_total{operation=\"Commit\",code=\"ok\"} 1\n"), "{}", text);
    assert!(text.contains("iwdb_ready 1\n"));
    assert!(!text.contains(token.expose()) && !text.contains(ADMIN_PASSWORD));
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
}

/// With traces sent to a collector that isn't there, requests are as fast
/// as ever and none fails; the drops are counted in the metrics and said
/// once in the log (step 16g, ADR 0057).
#[cfg(feature = "otel")]
#[test]
fn a_collector_that_is_down_slows_and_fails_no_request() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let (mut child, lines, reader) = spawn(
        bin()
            .env("IWDB_DATA_DIR", dir.path().join("data"))
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "3")
            .env("IWDB_TRACING_ENABLED", "true")
            .env("IWDB_TRACING_ENDPOINT", format!("http://127.0.0.1:{}", port)),
    );
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    let remote = admin(&address);
    let start = std::time::Instant::now();
    for i in 0..200 {
        block_on(remote.commit("default", vec![node(&format!("n{}", i))], CommitOptions::default())).unwrap();
        block_on(remote.get_nodes("default", vec![format!("n{}", i)], Default::default())).unwrap();
    }
    assert!(start.elapsed() < Duration::from_secs(20), "400 requests took {:?}", start.elapsed());
    // The batch is sent after its delay (5 s) and fails
    let token = block_on(remote.create_token("admin", "prometheus", None)).unwrap().token;
    let failed = |text: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix("iwdb_trace_spans_dropped_total{reason=\"export_failed\"} "))
            .map_or(0, |n| n.parse::<u64>().unwrap())
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (status, _, text) = https_get(&address, "/metrics", Some(token.expose()));
        assert_eq!(status, 200);
        if failed(&text) > 0 {
            assert!(text.contains("iwdb_trace_spans_exported_total 0\n"), "{}", text);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no failed export counted: {}", text);
        std::thread::sleep(Duration::from_millis(200));
    }
    let start = std::time::Instant::now();
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    assert!(start.elapsed() < Duration::from_secs(15), "shutdown took {:?}", start.elapsed());
    reader.join().unwrap();
    seen.extend(lines.try_iter());
    let warned = seen.iter().filter(|l| l.contains("trace export failed")).count();
    assert_eq!(warned, 1, "{:#?}", seen);
}

/// A collector of OTLP over HTTP: each request's `x-collector-key` header
/// and its body, decoded.
#[cfg(feature = "otel")]
type Received = (Option<String>, opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest);

#[cfg(feature = "otel")]
fn collector() -> (u16, mpsc::Receiver<Received>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                loop {
                    let mut length = 0;
                    let mut key = None;
                    let mut line = String::new();
                    let mut first = true;
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if first {
                            assert!(line.starts_with("POST /v1/traces "), "{}", line);
                            first = false;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap();
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("x-collector-key:") {
                            key = Some(v.trim().to_owned());
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let request = prost::Message::decode(body.as_slice()).unwrap();
                    let _ = tx.send((key, request));
                    writer.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
                }
            });
        }
    });
    (port, rx)
}

/// Spans reach a collector over OTLP/HTTP, with the service's name; a
/// shutdown sends what is queued before the batch's delay (ADR 0057).
#[cfg(feature = "otel")]
#[test]
fn the_shutdown_sends_the_queued_spans_to_the_collector() {
    let (port, received) = collector();
    let dir = tempfile::tempdir().unwrap();
    let (mut child, lines, reader) = spawn(
        bin()
            .env("IWDB_DATA_DIR", dir.path().join("data"))
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_TRACING_ENABLED", "true")
            .env("IWDB_TRACING_PROTOCOL", "http/protobuf")
            .env("IWDB_TRACING_ENDPOINT", format!("http://127.0.0.1:{}", port))
            .env("IWDB_TRACING_SERVICE_NAME", "iwdb-binary-test")
            .env("IWDB_TRACING_HEADERS", "x-collector-key=k1"),
    );
    let address = ready_address(&lines, &mut Vec::new());
    let remote = admin(&address);
    block_on(remote.commit("default", vec![node("n1")], CommitOptions::default())).unwrap();
    shut_down(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    let mut names = Vec::new();
    while let Ok((key, request)) = received.recv_timeout(Duration::from_secs(5)) {
        assert_eq!(key.as_deref(), Some("k1"));
        for resource in request.resource_spans {
            let attrs = resource.resource.unwrap().attributes;
            let service = attrs.iter().find(|kv| kv.key == "service.name").unwrap();
            assert!(format!("{:?}", service.value).contains("iwdb-binary-test"), "{:?}", service);
            for scope in resource.scope_spans {
                names.extend(scope.spans.into_iter().map(|s| s.name));
            }
        }
        if names.iter().any(|n| n == "iwdb.checkpoint") {
            break;
        }
    }
    for expected in
        ["Login", "Commit", "iwdb.queue", "iwdb.execute", "iwdb.commit", "iwdb.wal.fsync", "iwdb.checkpoint"]
    {
        assert!(names.iter().any(|n| n == expected), "{} not in {:?}", expected, names);
    }
}
