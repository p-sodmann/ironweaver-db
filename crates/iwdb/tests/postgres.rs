//! The Postgres source of projection mode (ADR 0032), against a real
//! Postgres wire protocol: PGlite in development and CI
//! (`scripts/pglite.sh`). Runs when `IWDB_TEST_POSTGRES_URL` is set, and
//! is skipped (passing) otherwise. Each test uses tables of its own.

#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use iwdb::projection::postgres::{PostgresConfig, PostgresSource};
use iwdb::projection::{Projection, ProjectionOptions, Rules, Source};
use iwdb::{MarkName, Store, Value};
use support::options;

fn url() -> Option<String> {
    let url = std::env::var("IWDB_TEST_POSTGRES_URL").ok().filter(|u| !u.is_empty());
    if url.is_none() {
        eprintln!("IWDB_TEST_POSTGRES_URL is not set: skipped (start scripts/pglite.sh)");
    }
    url
}

fn client(url: &str) -> postgres::Client {
    postgres::Client::connect(url, postgres::NoTls).unwrap()
}

/// A fresh table `name`: `id bigserial`, and columns of every type the
/// source reads.
fn create(c: &mut postgres::Client, name: &str) {
    c.batch_execute(&format!(
        "DROP TABLE IF EXISTS {name};
         CREATE TABLE {name} (
           id bigserial PRIMARY KEY, kind text NOT NULL, small smallint, num integer, amount double precision,
           ratio real, flag boolean, payload jsonb, raw bytea, at timestamptz, local timestamp, code varchar(8)
         )"
    ))
    .unwrap();
}

#[test]
fn rows_become_events_with_typed_fields() {
    let Some(url) = url() else { return };
    let mut c = client(&url);
    create(&mut c, "pg_types");
    c.batch_execute(
        "INSERT INTO pg_types (kind, small, num, amount, ratio, flag, payload, raw, at, local, code) VALUES
           ('a', 1, -2, 2.5, 0.5, true, '{\"x\": [1, \"y\", null], \"n\": {\"m\": 1.5}}', '\\x00ff',
            '2025-10-01 10:00:00+02', '2025-10-01 10:00:00', 'abc'),
           ('b', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    )
    .unwrap();
    let mut source = PostgresSource::new(PostgresConfig::new(&url, "pg_types", "id")).unwrap();
    let events = source.read(0, 10).unwrap();
    assert_eq!(events.iter().map(|e| e.position).collect::<Vec<_>>(), [1, 2]);
    let f = &events[0].fields;
    assert_eq!(f["id"], Value::Int(1));
    assert_eq!(f["kind"], Value::String("a".into()));
    assert_eq!((f["small"].clone(), f["num"].clone()), (Value::Int(1), Value::Int(-2)));
    assert_eq!((f["amount"].clone(), f["ratio"].clone()), (Value::Float(2.5), Value::Float(0.5)));
    assert_eq!(f["flag"], Value::Bool(true));
    assert_eq!(f["raw"], Value::Bytes(vec![0, 255]));
    assert_eq!(f["code"], Value::String("abc".into()));
    let Value::Dict(payload) = &f["payload"] else { panic!("{:?}", f["payload"]) };
    assert_eq!(payload["x"], Value::List(vec![Value::Int(1), Value::String("y".into()), Value::None]));
    // 2025-10-01T08:00:00Z
    let micros = 1_759_305_600_000_000;
    assert_eq!(f["at"], Value::DateTime(ironweaver_core::DateTime { micros, offset: Some(0) }));
    assert!(matches!(f["local"], Value::DateTime(ironweaver_core::DateTime { offset: None, .. })));
    assert!(events[1].fields.iter().filter(|(k, _)| *k != "id" && *k != "kind").all(|(_, v)| *v == Value::None));

    // Resuming, limits and chosen columns
    assert_eq!(source.read(1, 10).unwrap().len(), 1);
    assert!(source.read(2, 10).unwrap().is_empty());
    assert_eq!(source.read(0, 1).unwrap().len(), 1);
    let mut config = PostgresConfig::new(&url, "public.pg_types", "id");
    config.columns = Some(vec!["kind".into()]);
    let narrow = PostgresSource::new(config).unwrap().read(0, 10).unwrap();
    assert_eq!(narrow[0].fields.len(), 2, "{:?}", narrow[0].fields);
}

#[test]
fn unreadable_types_and_missing_tables_are_errors_that_name_them() {
    let Some(url) = url() else { return };
    let mut c = client(&url);
    c.batch_execute(
        "DROP TABLE IF EXISTS pg_numeric;
         CREATE TABLE pg_numeric (id bigserial PRIMARY KEY, price numeric);
         INSERT INTO pg_numeric (price) VALUES (1.25)",
    )
    .unwrap();
    let mut source = PostgresSource::new(PostgresConfig::new(&url, "pg_numeric", "id")).unwrap();
    let error = source.read(0, 10).unwrap_err();
    assert!(error.message.contains("'price'") && error.message.contains("cast"), "{}", error);
    let mut missing = PostgresSource::new(PostgresConfig::new(&url, "pg_no_such_table", "id")).unwrap();
    assert!(missing.read(0, 10).unwrap_err().message.contains("pg_no_such_table"));
    // A bad URL fails the read (retried by the runner), not the source's creation
    let mut config = PostgresConfig::new("postgresql://nobody@127.0.0.1:1/x?connect_timeout=1", "t", "id");
    config.gap_timeout = Duration::ZERO;
    let mut unreachable = PostgresSource::new(config).unwrap();
    assert!(unreachable.read(0, 1).unwrap_err().message.contains("connect"));
}

/// A hole in the ids (a transaction that took an id and commits later) is
/// waited for, and filled; one that stays is skipped after the timeout.
#[test]
fn holes_in_the_positions_are_waited_for() {
    let Some(url) = url() else { return };
    let mut c = client(&url);
    create(&mut c, "pg_gaps");
    c.batch_execute("INSERT INTO pg_gaps (id, kind) VALUES (1, 'a'), (2, 'a'), (4, 'a'), (5, 'a')").unwrap();
    let mut config = PostgresConfig::new(&url, "pg_gaps", "id");
    config.gap_timeout = Duration::from_millis(300);
    let mut source = PostgresSource::new(config).unwrap();
    let positions = |events: Vec<iwdb::projection::SourceEvent>| events.iter().map(|e| e.position).collect::<Vec<_>>();
    assert_eq!(positions(source.read(0, 10).unwrap()), [1, 2]);
    assert!(source.read(2, 10).unwrap().is_empty());
    // The late transaction commits
    c.batch_execute("INSERT INTO pg_gaps (id, kind) VALUES (3, 'late'), (7, 'a')").unwrap();
    assert_eq!(positions(source.read(2, 10).unwrap()), [3, 4, 5]);
    // 6 never comes: skipped after the timeout
    assert!(source.read(5, 10).unwrap().is_empty());
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(positions(source.read(5, 10).unwrap()), [7]);
}

/// The whole path: a Postgres table, rules from TOML, the store's
/// projection thread; a restart resumes from the mark.
#[test]
fn a_table_is_projected_with_rules_and_resumed_after_a_restart() {
    let Some(url) = url() else { return };
    let mut c = client(&url);
    create(&mut c, "pg_orders");
    let insert = |c: &mut postgres::Client, kind: &str, payload: &str| {
        c.execute("INSERT INTO pg_orders (kind, payload) VALUES ($1, $2::text::jsonb)", &[&kind, &payload]).unwrap();
    };
    for i in 0..10 {
        insert(&mut c, "customer", &format!(r#"{{"id": {}, "name": "c{}"}}"#, i, i));
    }
    #[derive(serde::Deserialize)]
    struct Config {
        rule: Rules,
    }
    let rules: Config = toml::from_str(
        r#"
        [[rule]]
        when = { kind = "customer" }
        mutations = [{ upsert_node = { id = "c${payload.id}", labels = ["Customer"], attr = { name = "${payload.name}" } } }]

        [[rule]]
        when = { kind = "order" }
        mutations = [
          { upsert_node = { id = "o${id}", labels = ["Order"], attr = { total = "${payload.total}" } } },
          { upsert_edge = { from = "c${payload.customer}", to = "o${id}", type = "PLACED" } },
          { append_attr = { node = "c${payload.customer}", key = "orders", value = "${id}" } },
        ]
        "#,
    )
    .unwrap();
    let project = |store: &Store, rules: Rules| {
        let options = ProjectionOptions { batch: 7, poll: Duration::from_millis(10), ..Default::default() };
        let source = PostgresSource::new(PostgresConfig::new(&url, "pg_orders", "id")).unwrap();
        let projection = Projection::new(MarkName::new("orders").unwrap(), source, rules, options);
        store.project(iwdb::NAMESPACE, projection).unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let handle = project(&store, rules.rule.clone());
    let status = handle.wait_until(Duration::from_secs(20), |s| s.mark == Some(10));
    assert_eq!(status.mark, Some(10), "{:?}", status);
    for i in 0..5 {
        insert(&mut c, "order", &format!(r#"{{"customer": {}, "total": {}.5}}"#, i % 2, i));
    }
    insert(&mut c, "ignored", "{}");
    let status = handle.wait_until(Duration::from_secs(20), |s| s.mark == Some(16));
    assert_eq!((status.mark, status.applied), (Some(16), 16), "{:?}", status);
    // Stopped mid-stream: a crash of the process
    drop(handle);
    drop(store);

    insert(&mut c, "order", r#"{"customer": 3, "total": 9.0}"#);
    let store = Store::open(dir.path(), options(2)).unwrap();
    let handle = project(&store, rules.rule);
    let status = handle.wait_until(Duration::from_secs(20), |s| s.mark == Some(17));
    assert_eq!((status.mark, status.applied), (Some(17), 1), "{:?}", status);
    assert_eq!(
        store.node("c0").unwrap().attr.get("orders"),
        Some(&Value::List(vec![Value::Int(11), Value::Int(13), Value::Int(15)]))
    );
    assert_eq!(store.node("o17").unwrap().attr.get("total"), Some(&Value::Float(9.0)));
    assert_eq!(store.default_namespace().status().marks[0].position, 17);
    store.close().unwrap();
}
