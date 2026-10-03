//! Format compatibility (design rule 4): a committed segment written in
//! each WAL format must keep reading as the same records (and, from format
//! 2 on, commit times), and writing the records must keep producing the
//! bytes of the current format's fixture. If that fails, the format
//! changed: bump `FORMAT_VERSION`, keep a reader for the previous version
//! and keep its fixture (see `documentation/formats/wal.md`).
//!
//! - `wal-v1/`: format 1 (step 4), frames without a commit time;
//! - `wal-v2/`: format 2 (step 7), with a commit time per frame;
//! - `wal-v3/`: format 3 (step 8), whose payloads start with the record's
//!   idempotency key and result (some records have one).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use ironweaver_core::{Attrs, Date, DateTime, EdgeId, Op, Value};
use iwdb_engine::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label};
use iwdb_engine::reserved::VERSION_KEY;
use iwdb_engine::{CatalogChange, Change, CommitRecord, DbRecord, IdempotencyKey, Keyed, Target};
use iwdb_storage::format::FORMAT_VERSION;
use iwdb_storage::{CommitTime, FsyncPolicy, Wal, WalOptions, WalReader, read_log};

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
    let mut records: Vec<CommitRecord> =
        changes.into_iter().zip(1..).map(|(change, seq)| CommitRecord::new(seq, change)).collect();
    // Format 3: a data and a catalog record with an idempotency key
    records[0].keyed = Some(Keyed {
        key: IdempotencyKey::new("req-1 ünïcode").unwrap(),
        fingerprint: 0xDEAD_BEEF,
        edge_ids: vec![EdgeId(0)],
        versions: vec![(Target::Node("a".into()), 1), (Target::Node("b".into()), 1), (Target::Edge(EdgeId(0)), 1)],
    });
    records[1].keyed =
        Some(Keyed { key: IdempotencyKey::new("k").unwrap(), fingerprint: 1, edge_ids: vec![], versions: vec![] });
    records
}

/// The records as a log of format `version` holds them: formats 1 and 2
/// have no idempotency keys.
fn records_in(version: u32) -> Vec<CommitRecord> {
    fixture_records().into_iter().map(|r| CommitRecord { keyed: r.keyed.filter(|_| version >= 3), ..r }).collect()
}

/// The commit times of the fixture records: fixed, and once going
/// backwards (times are written as given).
fn fixture_time(seq: u64) -> CommitTime {
    let base = 1_759_312_800_000_000; // 2025-10-01T10:00:00Z
    CommitTime(if seq == 5 { base } else { base + seq as i64 * 1_500_250 })
}

/// Write the fixture records with `always` into `dir`.
fn write(dir: &std::path::Path) {
    let options = WalOptions { fsync: FsyncPolicy::Always, ..WalOptions::default() };
    let mut wal = Wal::create(dir, options, 1).unwrap();
    for record in fixture_records() {
        let time = fixture_time(record.seq);
        wal.append_at(&record, time).unwrap();
    }
    wal.close().unwrap();
}

/// The records of the log in `dir`, with their commit times.
fn read_timed(dir: &std::path::Path) -> Vec<(CommitRecord, Option<CommitTime>)> {
    let mut reader = WalReader::open(dir, 1).unwrap();
    let mut out = Vec::new();
    while let Some(record) = reader.next() {
        out.push((record.unwrap(), reader.time()));
    }
    assert!(reader.end().unwrap().torn().is_none());
    out
}

#[test]
fn the_v1_fixture_reads_as_its_records_without_times() {
    let (records, end) = read_log(&fixture_dir(1), 1).unwrap();
    assert_eq!(records, records_in(1));
    assert_eq!(end.next_seq, fixture_records().len() as u64 + 1);
    assert!(end.torn().is_none());
    assert!(read_timed(&fixture_dir(1)).iter().all(|(_, time)| time.is_none()));
}

#[test]
fn the_v2_fixture_reads_as_its_records_and_times() {
    let expected: Vec<_> = records_in(2).into_iter().map(|r| (r.clone(), Some(fixture_time(r.seq)))).collect();
    assert_eq!(read_timed(&fixture_dir(2)), expected);
}

#[test]
fn the_v3_fixture_reads_as_its_records_keys_and_times() {
    let expected: Vec<_> = records_in(3).into_iter().map(|r| (r.clone(), Some(fixture_time(r.seq)))).collect();
    assert!(expected.iter().filter(|(r, _)| r.keyed.is_some()).count() == 2);
    assert_eq!(read_timed(&fixture_dir(3)), expected);
}

#[test]
fn writing_the_records_gives_the_current_fixture_bytes() {
    assert_eq!(FORMAT_VERSION, 3, "a new format version needs its own fixture next to wal-v1 to wal-v3");
    let dir = tempfile::tempdir().unwrap();
    write(dir.path());
    let name = "00000000000000000001.wal";
    let written = fs::read(dir.path().join(name)).unwrap();
    let fixture = fs::read(fixture_dir(FORMAT_VERSION).join(name)).unwrap();
    assert!(written == fixture, "the WAL format changed: bump FORMAT_VERSION and add a fixture");
}

/// A log written in format 1 continues in the current format: a new writer starts a
/// new segment in the current format, and the reader reads both as one log.
#[test]
fn a_v1_log_continues_in_the_current_format() {
    let dir = tempfile::tempdir().unwrap();
    let name = "00000000000000000001.wal";
    fs::copy(fixture_dir(1).join(name), dir.path().join(name)).unwrap();
    let next = fixture_records().len() as u64 + 1;
    let mut wal = Wal::create(dir.path(), WalOptions::default(), next).unwrap();
    let record = CommitRecord::new(next, Change::Data(vec![]));
    wal.append(&record).unwrap();
    wal.close().unwrap();
    let read = read_timed(dir.path());
    assert_eq!(read.len() as u64, next);
    assert!(read[..read.len() - 1].iter().all(|(_, time)| time.is_none()));
    let (last, time) = read.last().unwrap();
    assert_eq!(last, &record);
    assert!(time.is_some_and(|t| t > CommitTime(1_700_000_000_000_000)), "{:?}", time);
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

/// A log written in format 2 (step 7) continues in format 3: its records
/// read without keys, the new segment's with them.
#[test]
fn a_v2_log_continues_with_keyed_records() {
    let dir = tempfile::tempdir().unwrap();
    let name = "00000000000000000001.wal";
    fs::copy(fixture_dir(2).join(name), dir.path().join(name)).unwrap();
    let next = fixture_records().len() as u64 + 1;
    let mut wal = Wal::create(dir.path(), WalOptions::default(), next).unwrap();
    let mut record = CommitRecord::new(next, Change::Data(vec![]));
    record.keyed =
        Some(Keyed { key: IdempotencyKey::new("again").unwrap(), fingerprint: 9, edge_ids: vec![], versions: vec![] });
    let time = wal.append(&record).unwrap();
    wal.close().unwrap();
    let read = read_timed(dir.path());
    assert!(read[..read.len() - 1].iter().all(|(r, _)| r.keyed.is_none()));
    assert_eq!(read.last(), Some(&(record, Some(time))));
}
