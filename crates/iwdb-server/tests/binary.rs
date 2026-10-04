//! The `iwdb-server` binary: it serves the data directory of its config
//! file or environment, shuts down gracefully on SIGTERM (checkpointing, so
//! the next open replays nothing), refuses a bad command line or
//! configuration with every problem, logs JSON lines, and becomes ready
//! only once recovery has finished.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use iwdb::{Mutation, Store};
use iwdb_query::exec::block_on;
use iwdb_query::{CommitOptions, Database};
use iwdb_server::client::Remote;

const BIN: &str = env!("CARGO_BIN_EXE_iwdb-server");

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
fn serves_its_data_directory_and_shuts_down_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    std::fs::write(&config, "data_dir = \"data\"\nlisten = \"127.0.0.1:0\"\n[server]\ndrain_timeout_secs = 5\n")
        .unwrap();
    let (mut child, lines, reader) = spawn(Command::new(BIN).arg("--config").arg(&config));
    let mut seen = Vec::new();
    let address = ready_address(&lines, &mut seen);
    // Listening (recovering) comes first, then ready
    assert!(event(&seen[0])["message"].as_str().unwrap().starts_with("listening"), "{:?}", seen);
    assert!(seen.iter().any(|l| event(l)["message"] == "recovery finished"), "{:?}", seen);

    let remote = Remote::connect(&format!("http://{}", address)).unwrap();
    let seq = block_on(remote.commit("default", vec![node("a")], CommitOptions::default())).unwrap().seq;
    assert_eq!(seq, 1);
    iwdb_server::health::probe(&address, Duration::from_secs(5)).unwrap();
    let probe = Command::new(BIN).arg("--probe").arg(&address).output().unwrap();
    assert!(probe.status.success(), "{:?}", probe);

    let killed = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status().unwrap();
    assert!(killed.success());
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
        let (mut child, lines, reader) = spawn(Command::new(BIN).arg("--config").arg(&config));
        let address = ready_address(&lines, &mut Vec::new());
        let remote = Remote::connect(&format!("http://{}", address)).unwrap();
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
        Command::new("kill").arg("-TERM").arg(child.id().to_string()).status().unwrap();
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

fn sigterm(child: &std::process::Child) {
    let killed = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status().unwrap();
    assert!(killed.success());
}

/// No file: `IWDB_DATA_DIR` and the other variables are the whole
/// configuration (ADR 0039), as in a container.
#[test]
fn runs_from_the_environment_alone() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let (mut child, lines, reader) = spawn(
        Command::new(BIN)
            .env("IWDB_DATA_DIR", &data)
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5")
            .env("IWDB_STORE_FSYNC", "group")
            .env("IWDB_LOG_LEVEL", "debug"),
    );
    let address = ready_address(&lines, &mut Vec::new());
    let remote = Remote::connect(&format!("http://{}", address)).unwrap();
    block_on(remote.commit("default", vec![node("e")], CommitOptions::default())).unwrap();
    sigterm(&child);
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
    std::fs::write(&config, "data_dir = \"/var/lib/iwdb\"\n[store]\nretain_records = 7\n").unwrap();
    let out = Command::new(BIN)
        .arg("--check-config")
        .arg("--config")
        .arg(&config)
        .env("IWDB_STORE_FSYNC", "off")
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    let text = String::from_utf8(out.stdout).unwrap();
    let line = |key: &str| text.lines().find(|l| l.starts_with(&format!("{} =", key))).unwrap().to_owned();
    assert!(line("fsync").contains("\"off\"") && line("fsync").ends_with("# IWDB_STORE_FSYNC"), "{}", text);
    assert!(line("retain_records").ends_with("# file"), "{}", text);
    assert!(line("listen").ends_with("# default"), "{}", text);
    let back = iwdb_server::config::Config::parse(&text).unwrap();
    assert_eq!(back.store.fsync, iwdb_server::config::Fsync::Off);
    assert_eq!(back.store.retain_records, 7);
    assert_eq!(back.data_dir, std::path::PathBuf::from("/var/lib/iwdb"));
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
        Command::new(BIN)
            .env("IWDB_DATA_DIR", &data)
            .env("IWDB_LISTEN", "127.0.0.1:0")
            .env("IWDB_SERVER_DRAIN_TIMEOUT_SECS", "5"),
    );
    let listening = event(&lines.recv_timeout(Duration::from_secs(30)).unwrap());
    assert!(listening["message"].as_str().unwrap().starts_with("listening"), "{}", listening);
    let address = listening["address"].as_str().unwrap().to_owned();
    let mut not_ready = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while iwdb_server::health::probe(&address, Duration::from_secs(5)).is_err() {
        not_ready += 1;
        assert!(std::time::Instant::now() < deadline, "never ready");
    }
    let remote = Remote::connect(&format!("http://{}", address)).unwrap();
    let status = block_on(remote.namespace_status("default")).unwrap();
    assert_eq!(status.nodes, BATCHES * BATCH);
    assert_eq!(status.recovery.replayed, BATCHES as u64);
    assert!(not_ready > 0, "recovery was too quick to see it unready");
    let mut seen = Vec::new();
    ready_address(&lines, &mut seen);
    let recovered = seen.iter().map(|l| event(l)).find(|e| e["message"] == "recovered").unwrap();
    assert_eq!(recovered["replayed"], BATCHES as u64);
    sigterm(&child);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
}
