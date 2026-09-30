//! Format compatibility (design rule 4): a committed segment written in
//! WAL format 1 must keep reading as the same records, and writing those
//! records must keep producing the same bytes. If either test fails, the
//! format changed: bump `FORMAT_VERSION`, keep a reader for version 1 and
//! keep this fixture (see `documentation/formats/wal.md`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use ironweaver_core::{Attrs, Date, DateTime, EdgeId, Op, Value};
use iwdb_engine::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label};
use iwdb_engine::reserved::VERSION_KEY;
use iwdb_engine::{CatalogChange, Change, CommitRecord, DbRecord};
use iwdb_storage::format::FORMAT_VERSION;
use iwdb_storage::{read_log, FsyncPolicy, Wal, WalOptions};

fn fixture_dir(version: u32) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/wal-v{}", version))
}

fn record(attr: Vec<(&str, Value)>, meta: Vec<(&str, Value)>, version: u64) -> DbRecord {
    let map = |kv: Vec<(&str, Value)>| kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect::<Attrs>();
    DbRecord { attr: map(attr), meta: map(meta), version }
}

fn version(v: i64) -> Option<Value> {
    Some(Value::Int(v))
}

/// The records of the fixture: every op kind, every value kind the core
/// exports a type for (not `Half`, whose `f16` the core doesn't re-export),
/// and every catalog change.
fn fixture_records() -> Vec<CommitRecord> {
    let dict: std::collections::HashMap<String, Value> =
        [("z".to_owned(), Value::Int(-1)), ("a".to_owned(), Value::List(vec![]))].into();
    let every_value = vec![
        ("s", Value::String("héllo".into())),
        ("i", Value::Int(i64::MIN)),
        ("f", Value::Float(-0.5)),
        ("b", Value::Bool(true)),
        ("n", Value::None),
        ("l", Value::List(vec![Value::Int(1), Value::String("x".into()), Value::List(vec![Value::Bool(false)])])),
        ("d", Value::Dict(dict)),
        ("y", Value::Bytes(vec![0, 1, 255])),
        ("t", Value::Date(Date(19_000))),
        ("u", Value::DateTime(DateTime { micros: 1_700_000_000_000_000, offset: Some(3600) })),
        ("w", Value::DateTime(DateTime { micros: -1, offset: None })),
    ];
    let path = |k: &str| AttrPath::new([k]).unwrap();
    let unique = Constraint { kind: ConstraintKind::Unique, label: Label::new("Person").unwrap(), path: path("email") };
    let required =
        Constraint { kind: ConstraintKind::Required, label: Label::new("Person").unwrap(), path: path("name") };
    let changes = vec![
        Change::Data(vec![
            Op::AddNode {
                id: "a".into(),
                labels: vec!["Person".into()],
                data: record(every_value, vec![("m", Value::Int(1))], 1),
            },
            Op::AddNode { id: "b".into(), labels: vec![], data: record(vec![], vec![], 1) },
            Op::AddEdge {
                id: EdgeId(0),
                from: "a".into(),
                to: "b".into(),
                ty: Some("KNOWS".into()),
                data: record(vec![("w", Value::Float(0.25))], vec![], 1),
            },
        ]),
        Change::Catalog(CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["address", "city"]).unwrap() })),
        Change::Catalog(CatalogChange::AddConstraint(unique.clone())),
        Change::Data(vec![
            Op::SetNodeAttr { id: "a".into(), key: "s".into(), value: Some(Value::String("new".into())) },
            Op::SetNodeAttr { id: "a".into(), key: "n".into(), value: None },
            Op::AddLabel { id: "b".into(), label: "Person".into() },
            Op::RemoveLabel { id: "a".into(), label: "Person".into() },
            Op::SetEdgeAttr { id: EdgeId(0), key: "w".into(), value: Some(Value::Float(1.0)) },
            Op::SetEdgeType { id: EdgeId(0), ty: None },
            Op::SetNodeAttr { id: "a".into(), key: VERSION_KEY.into(), value: version(2) },
            Op::SetNodeAttr { id: "b".into(), key: VERSION_KEY.into(), value: version(2) },
            Op::SetEdgeAttr { id: EdgeId(0), key: VERSION_KEY.into(), value: version(2) },
        ]),
        Change::Data(vec![
            Op::SetNode { id: "b".into(), data: record(vec![("email", Value::String("b@x".into()))], vec![], 3) },
            Op::SetEdge { id: EdgeId(0), data: record(vec![], vec![("m", Value::Bool(true))], 3) },
            Op::RenameNode { id: "a".into(), new_id: "c".into() },
            Op::RemoveEdge { id: EdgeId(0) },
            Op::RemoveNode { id: "c".into() },
        ]),
        Change::Catalog(CatalogChange::AddConstraint(required.clone())),
        Change::Catalog(CatalogChange::DropConstraint(required)),
        Change::Catalog(CatalogChange::DropConstraint(unique)),
        Change::Catalog(CatalogChange::DropIndex(IndexDef { path: AttrPath::new(["address", "city"]).unwrap() })),
        Change::Data(vec![]),
    ];
    changes.into_iter().zip(1..).map(|(change, seq)| CommitRecord { seq, change }).collect()
}

/// Write the fixture records with `always` into `dir`.
fn write(dir: &std::path::Path) {
    let options = WalOptions { fsync: FsyncPolicy::Always, ..WalOptions::default() };
    let mut wal = Wal::create(dir, options, 1).unwrap();
    for record in fixture_records() {
        wal.append(&record).unwrap();
    }
    wal.close().unwrap();
}

#[test]
fn the_v1_fixture_reads_as_its_records() {
    let (records, end) = read_log(&fixture_dir(1), 1).unwrap();
    assert_eq!(records, fixture_records());
    assert_eq!(end.next_seq, fixture_records().len() as u64 + 1);
    assert!(end.torn().is_none());
}

#[test]
fn writing_the_records_gives_the_v1_fixture_bytes() {
    assert_eq!(FORMAT_VERSION, 1, "a new format version needs its own fixture next to wal-v1");
    let dir = tempfile::tempdir().unwrap();
    write(dir.path());
    let name = "00000000000000000001.wal";
    let written = fs::read(dir.path().join(name)).unwrap();
    let fixture = fs::read(fixture_dir(1).join(name)).unwrap();
    assert!(written == fixture, "the WAL format changed: bump FORMAT_VERSION and add a fixture");
}

/// Writes the fixture of the current format version, if it doesn't exist
/// yet: `cargo test -p iwdb-storage --test wal_fixture -- --ignored`.
#[test]
#[ignore]
fn generate_fixture() {
    let dir = fixture_dir(FORMAT_VERSION);
    if dir.exists() {
        return;
    }
    fs::create_dir_all(&dir).unwrap();
    write(&dir);
}
