//! The `iwdb-server` binary: it serves the data directory of its config
//! file, shuts down gracefully on SIGTERM (checkpointing, so the next open
//! replays nothing), and refuses a bad command line or config file.

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

#[test]
fn serves_its_data_directory_and_shuts_down_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    std::fs::write(&config, "data_dir = \"data\"\nlisten = \"127.0.0.1:0\"\n[server]\ndrain_timeout_secs = 5\n")
        .unwrap();
    let mut child = Command::new(BIN).arg("--config").arg(&config).stderr(Stdio::piped()).spawn().unwrap();
    // The address it listens on, from its first line
    let (lines_tx, lines) = mpsc::channel::<String>();
    let stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap();
            let _ = lines_tx.send(line);
        }
    });
    let first = lines.recv_timeout(Duration::from_secs(30)).expect("the server started");
    let address = first.rsplit(" on ").next().unwrap().to_owned();
    assert!(first.contains("serving"), "{}", first);

    let remote = Remote::connect(&format!("http://{}", address)).unwrap();
    let node = Mutation::UpsertNode {
        id: "a".into(),
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    };
    let seq = block_on(remote.commit("default", vec![node], CommitOptions::default())).unwrap().seq;
    assert_eq!(seq, 1);

    let killed = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status().unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let output: Vec<String> = lines.try_iter().collect();
    assert!(status.success(), "{:?}: {:?}", status, output);
    assert!(output.iter().any(|l| l.contains("closed")), "{:?}", output);

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
        let mut child = Command::new(BIN).arg("--config").arg(&config).stderr(Stdio::piped()).spawn().unwrap();
        let (lines_tx, lines) = mpsc::channel::<String>();
        let stderr = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let _ = lines_tx.send(line.unwrap());
            }
        });
        let serving = loop {
            let line = lines.recv_timeout(Duration::from_secs(30)).expect("the server started");
            if line.contains("serving") {
                break line;
            }
        };
        let address = serving.rsplit(" on ").next().unwrap().to_owned();
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
