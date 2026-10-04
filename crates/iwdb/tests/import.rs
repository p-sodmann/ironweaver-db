//! Bulk import and export (ADR 0033): a namespace made from one
//! checkpoint, all or nothing across crashes; exports that import back to
//! the same graph; files of the Ironweaver library; LGF; bad imports;
//! progress; backups of imported namespaces.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::assert_matches;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use common::{Action, Call, Rule, TestFs, When};
use ironweaver_core::format::{GraphWriter, RecordCodec};
use ironweaver_core::{Attrs, Graph, GraphError, Record, Value};
use iwdb::import::{ExportFormat, ImportFormat, Phase, Progress};
use iwdb::{
    AttrPath, BatchLimits, CatalogChange, Error, IndexDef, LogFs, Mutation, ReadOptions, RestoreSources, RestoreTarget,
    Store,
};
use iwdb_storage::checkpoint::list_checkpoints;
use support::options;

fn attrs(entries: &[(&str, Value)]) -> Attrs {
    entries.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}

fn node(id: &str, labels: &[&str], attr: Attrs) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        attr,
        meta: attrs(&[("source", Value::from("test"))]),
        expected_version: None,
    }
}

fn edge(from: &str, to: &str, ty: Option<&str>, attr: Attrs) -> Mutation {
    Mutation::AddEdge { from: from.into(), to: to.into(), ty: ty.map(str::to_owned), attr, meta: Attrs::new() }
}

/// Fill namespace `name` with a graph of every kind of value, labels,
/// typed and untyped edges, meta, two indexes, deleted nodes and edges
/// (holes in the slots and the edge ids), and versions above 1.
fn fill<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, name: &str, n: usize)
where
    F::File: Send,
{
    let ns = store.namespace(name).unwrap();
    for path in [vec!["age"], vec!["address", "city"]] {
        ns.commit_catalog(CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(path).unwrap() })).unwrap();
    }
    for chunk in (0..n).collect::<Vec<_>>().chunks(100) {
        let mut mutations = Vec::new();
        for &i in chunk {
            let address = Value::Dict(attrs(&[("city", Value::from(format!("c{}", i % 7)))]));
            let attr = attrs(&[
                ("age", Value::Int(i as i64 % 90)),
                ("score", Value::Float(i as f64 / 3.0)),
                ("odd", Value::Bool(i % 2 == 1)),
                ("raw", Value::Bytes(vec![i as u8, 0, 255])),
                ("tags", Value::List(vec![Value::from("t"), Value::Int(i as i64), Value::None])),
                ("address", address),
            ]);
            let labels: &[&str] = if i % 3 == 0 { &["Person", "Customer"] } else { &["Person"] };
            mutations.push(node(&format!("n{}", i), labels, attr));
            if i > 0 {
                mutations.push(edge(&format!("n{}", i), &format!("n{}", i / 2), Some("PARENT"), Attrs::new()));
            }
            if i > 3 && i % 5 == 0 {
                mutations.push(edge(&format!("n{}", i), "n1", None, attrs(&[("w", Value::Float(0.5))])));
            }
        }
        ns.commit(&mutations).unwrap();
    }
    // Holes, and versions above 1
    ns.commit(&[Mutation::DeleteNode { id: "n7".into(), expected_version: None }]).unwrap();
    ns.commit(&[node("n8", &["Person"], attrs(&[("age", Value::Int(1)), ("nan", Value::Float(f64::NAN))]))]).unwrap();
}

fn export<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, name: &str, format: ExportFormat) -> Vec<u8>
where
    F::File: Send,
{
    let mut out = Vec::new();
    store.namespace(name).unwrap().export(&mut out, format, None).unwrap();
    out
}

fn names<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>) -> Vec<String>
where
    F::File: Send,
{
    store.namespaces().into_iter().map(|n| n.name.to_string()).collect()
}

/// The entries of `ns/`: namespace directories and anything else.
fn ns_entries(root: &Path) -> BTreeSet<String> {
    fs::read_dir(root.join("ns")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/import").join(name)).unwrap()
}

/// The acceptance criterion: an import makes its namespace from one
/// checkpoint without WAL records, and an export imports back to the same
/// graph (byte for byte, re-exported).
#[test]
fn an_import_creates_its_namespace_from_one_checkpoint_and_an_export_imports_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    store.create_namespace("src", None).unwrap();
    fill(&store, "src", 1000);
    let binary = export(&store, "src", ExportFormat::Binary);
    let json = export(&store, "src", ExportFormat::Json);
    assert!(binary.starts_with(b"IRONWEAV") && json.starts_with(b"{"));

    for (name, format, bytes) in [("bin", ImportFormat::Binary, &binary), ("json", ImportFormat::Json, &json)] {
        let report = store.import_namespace(name, format, &bytes[..], None).unwrap();
        let src = store.namespace("src").unwrap();
        let (nodes, edges) = src.read(|n| (n.graph().node_count(), n.graph().edge_count()));
        assert_eq!((report.nodes, report.edges, report.seq), (nodes, edges, 1), "{}", name);
        assert_eq!(report.bytes_read, bytes.len() as u64);
        assert_eq!(report.indexes.iter().map(|p| p.to_string()).collect::<Vec<_>>(), ["address.city", "age"]);
        assert!(report.dropped.is_empty(), "{:?}", report.dropped);
        assert_eq!(export(&store, name, ExportFormat::Binary), binary, "{}: the same graph", name);

        let ns = store.namespace(name).unwrap();
        assert_eq!(ns.seq(), 1);
        ns.read(|n| {
            assert!(n.graph().nodes().all(|(_, node)| node.data.version == 1));
            assert!(n.graph().edges().all(|(_, edge)| edge.data.version == 1));
            assert!(iwdb_engine::invariants::check(n).is_empty());
            assert_eq!(n.catalog().indexes().count(), 2);
        });
        // One checkpoint, at seq 1, and no WAL record: the stream starts at 2
        let paths = ns.status();
        assert_eq!(paths.seq, 1);
        let ckpt_dir = dir.path().join("ns").join(format!("{:020}", report.event.id)).join("checkpoints");
        let checkpoints = list_checkpoints(&ckpt_dir).unwrap();
        assert_eq!(checkpoints.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [1]);
        assert_eq!(fs::metadata(&checkpoints[0].1).unwrap().len(), report.checkpoint_bytes);
        let limits = BatchLimits { max_records: 10, max_bytes: 1 << 20 };
        assert_matches!(
            ns.changes(1, limits, false, &ReadOptions::default()),
            Err(Error::NotRetained { first_seq: 2, .. })
        );
        assert!(ns.changes(2, limits, false, &ReadOptions::default()).unwrap().records.is_empty());
        // It takes commits like any namespace
        let result = ns.commit(&[node("new", &[], Attrs::new())]).unwrap();
        assert_eq!(result.seq, 2);
        assert_eq!(ns.changes(2, limits, false, &ReadOptions::default()).unwrap().records.len(), 1);
    }
    store.close().unwrap();

    // Recovered from its checkpoint and its WAL
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(names(&store), ["bin", "default", "json", "src"]);
    for name in ["bin", "json"] {
        let ns = store.namespace(name).unwrap();
        assert_eq!(ns.seq(), 2);
        assert!(ns.read(|n| n.graph().contains_node("new") && n.graph().contains_node("n999")));
    }
    assert_eq!(export(&store, "bin", ExportFormat::Json), export(&store, "json", ExportFormat::Json));
    store.close().unwrap();
    let report = iwdb::verify(dir.path()).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
}

/// After a failure at any file operation of an import: the next open
/// shows the namespace there with all its data, or not at all (no
/// directory, no temporary file), and the store verifies.
#[test]
fn every_file_operation_of_an_import_can_fail() {
    let source = {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), options(2)).unwrap();
        store.create_namespace("src", None).unwrap();
        fill(&store, "src", 50);
        export(&store, "src", ExportFormat::Binary)
    };
    let setup = |fs: &TestFs, dir: &Path| {
        let store = Store::open_with(fs.clone(), dir, options(2)).unwrap();
        store.create_namespace("keep", None).unwrap();
        store.close().unwrap();
    };
    let calls = {
        let dir = tempfile::tempdir().unwrap();
        let fs = TestFs::default();
        setup(&fs, dir.path());
        let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
        let before = fs.state().calls.len();
        store.import_namespace("fresh", ImportFormat::Binary, &source[..], None).unwrap();
        fs.state().calls[before..].to_vec()
    };
    for needed in [Call::Create, Call::Write, Call::Sync, Call::Rename, Call::CreateDir, Call::SyncDir] {
        assert!(calls.contains(&needed), "{:?} in {:?}", needed, calls);
    }
    let (mut seen_gone, mut seen_there, mut seen_finished) = (false, false, false);
    for &call in Call::ALL.iter() {
        let count = calls.iter().filter(|c| **c == call).count();
        for skip in 0..count {
            for when in [When::Before, When::After] {
                let dir = tempfile::tempdir().unwrap();
                let fs = TestFs::default();
                setup(&fs, dir.path());
                let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
                let rule = Rule::new(call, when, Action::Fail).skip(skip as u64);
                fs.add(rule.clone());
                let result = store.import_namespace("fresh", ImportFormat::Binary, &source[..], None);
                let fired = !fs.state().fired.is_empty();
                drop(store);
                if !fired {
                    assert!(result.is_ok(), "{}: {:?}", rule, result.err());
                    continue;
                }
                let store = Store::open(dir.path(), options(2)).unwrap_or_else(|e| panic!("{}: {}", rule, e));
                let entries = ns_entries(dir.path());
                assert!(entries.iter().all(|e| !e.ends_with(".tmp")), "{}: {:?}", rule, entries);
                if names(&store).contains(&"fresh".to_owned()) {
                    seen_there = true;
                    seen_finished |= store.recovery().namespace("fresh").unwrap().finished_import;
                    assert_eq!(entries.len(), 3, "{}: {:?}", rule, entries);
                    assert_eq!(export(&store, "fresh", ExportFormat::Binary), source, "{}: all its data", rule);
                    store.namespace("fresh").unwrap().commit(&[node("x", &[], Attrs::new())]).unwrap();
                } else {
                    seen_gone = true;
                    assert_eq!(entries.len(), 2, "{}: {:?}", rule, entries);
                    // The retry succeeds
                    store.import_namespace("fresh", ImportFormat::Binary, &source[..], None).unwrap();
                }
                assert_eq!(names(&store), ["default", "fresh", "keep"], "{}", rule);
                store.close().unwrap();
                let report = iwdb::verify(dir.path()).unwrap();
                assert!(report.is_ok(), "{}: {:#?}", rule, report.problems);
            }
        }
    }
    assert!(seen_gone && seen_there, "failures landed both before and after the create event");
    assert!(seen_finished, "a crash between the event and the rename is finished by recovery");
}

/// A graph as the Ironweaver library makes it: `Record` payloads, labels,
/// typed edges, graph meta, an index.
fn library_graph() -> (Graph<Record, Record>, Attrs) {
    let mut g = Graph::<Record, Record>::new();
    let rec = |attr: Attrs| Record { attr, meta: attrs(&[("m", Value::Int(1))]) };
    let a = g.add_node("a", rec(attrs(&[("name", Value::from("alpha"))]))).unwrap();
    let b = g.add_node("b", rec(attrs(&[("name", Value::from("beta"))]))).unwrap();
    g.add_label(a, "Person").unwrap();
    let e = g.add_edge(a, b, rec(attrs(&[("w", Value::Float(1.5))]))).unwrap();
    g.set_edge_type(e, Some("KNOWS")).unwrap();
    g.create_index::<GraphError>(&["name".to_owned()]).unwrap();
    (g, attrs(&[("title", Value::from("lib")), ("author", Value::from("me"))]))
}

#[test]
fn files_of_the_ironweaver_library_import() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let (g, meta) = library_graph();
    let codec = RecordCodec { meta: &meta, half: false };
    let json = GraphWriter::new(&g, &codec).to_json(true).unwrap();
    let mut binary = Vec::new();
    GraphWriter::new(&g, &codec).write_binary(&mut binary).unwrap();
    for (name, format, bytes) in [("lj", ImportFormat::Json, json), ("lb", ImportFormat::Binary, binary)] {
        assert_eq!(ImportFormat::detect(&bytes), Some(format));
        let report = store.import_namespace(name, format, &bytes[..], None).unwrap();
        assert_eq!(report.dropped, ["graph meta 'author'", "graph meta 'title'"]);
        assert_eq!(report.indexes, [AttrPath::new(["name"]).unwrap()]);
        let ns = store.namespace(name).unwrap();
        ns.read(|n| {
            let g = n.graph();
            let a = g.node_by_id("a").unwrap();
            assert_eq!(g.label_names(g.node_ix("a").unwrap()).unwrap(), ["Person"]);
            assert_eq!((a.data.attr["name"].clone(), a.data.meta["m"].clone()), (Value::from("alpha"), Value::Int(1)));
            let (ix, e) = g.edges().next().unwrap();
            assert_eq!((g.edge_type_name(ix), e.data.attr["w"].clone()), (Some("KNOWS"), Value::Float(1.5)));
        });
    }

    // The library's own files: format 2 (with half floats too), and
    // version 1 JSON, which the core migrates
    for (name, file) in
        [("v2j", "v2_graph.json"), ("v2b", "v2_graph.bin"), ("v2h", "v2_graph_f16.bin"), ("v1j", "legacy_graph.json")]
    {
        let bytes = fixture(file);
        let format = ImportFormat::detect(&bytes).unwrap();
        let report =
            store.import_namespace(name, format, &bytes[..], None).unwrap_or_else(|e| panic!("{}: {}", file, e));
        assert_eq!((report.nodes, report.edges), (3, 3), "{}", file);
        assert_eq!(report.dropped, ["graph meta 'title'"], "{}", file);
        let ns = store.namespace(name).unwrap();
        ns.read(|n| {
            let g = n.graph();
            let a = g.node_by_id("a").unwrap();
            assert_eq!(a.data.attr["name"], Value::from("alpha"), "{}", file);
            assert_eq!(a.data.meta["note"], Value::from("m"), "{}", file);
            // In each, a -> b has the type `knows` (in version 1, the edge
            // attribute `type`, which the core migrates)
            let ab = g.edges().find(|(_, e)| g.node(e.target()).unwrap().id() == "b").unwrap().0;
            assert_eq!(g.edge_type_name(ab), Some("knows"), "{}", file);
            assert!(g.edges().all(|(_, e)| !e.data.attr.contains_key("type")), "{}", file);
        });
    }
    // Version 1 binary files are refused, saying how to convert them
    let error =
        store.import_namespace("v1b", ImportFormat::Binary, &fixture("legacy_graph.bin")[..], None).unwrap_err();
    assert_matches!(&error, Error::InvalidImport { reason } if reason.contains("save_to_json"), "{}", error);
    store.close().unwrap();
    assert!(iwdb::verify(dir.path()).unwrap().is_ok());
}

/// Nothing is created by an import that fails, whatever the reason.
#[test]
fn bad_imports_create_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let reserved = |entity: &str| {
        let mut g = Graph::<Record, Record>::new();
        let mut record = Record { attr: Attrs::new(), meta: Attrs::new() };
        match entity {
            "attr" => record.attr.insert("iwdb.x".into(), Value::Int(1)),
            _ => record.meta.insert("iwdb.version".into(), Value::Int(1)),
        };
        g.add_node("a", record).unwrap();
        GraphWriter::new(&g, &RecordCodec { meta: &Attrs::new(), half: false }).to_json(false).unwrap()
    };
    // A checkpoint of the database is not a plain file
    let checkpoint = {
        let ns = iwdb::Namespace::new(iwdb::NamespaceName::new("x").unwrap());
        iwdb_engine::codec::to_binary(ns.graph(), &ns.graph_meta()).unwrap()
    };
    let cases: Vec<(&str, ImportFormat, Vec<u8>, &str)> = vec![
        ("ok", ImportFormat::Json, b"{not json".to_vec(), "invalid import"),
        ("ok", ImportFormat::Binary, b"IRONWEAV-truncated".to_vec(), "invalid import"),
        ("ok", ImportFormat::Json, reserved("attr"), "iwdb.x"),
        ("ok", ImportFormat::Json, reserved("meta"), "iwdb.version"),
        ("ok", ImportFormat::Binary, checkpoint, "iwdb."),
        ("ok", ImportFormat::Lgf, b"@nodes\nlabel\na\n@arcs\nw\na b 1\n".to_vec(), "line 6: no node 'b'"),
        ("default", ImportFormat::Lgf, b"@nodes\nlabel\na\n".to_vec(), "exists already"),
        ("bad name!", ImportFormat::Lgf, b"@nodes\nlabel\na\n".to_vec(), "namespace name"),
    ];
    for (name, format, bytes, message) in cases {
        let error = store.import_namespace(name, format, &bytes[..], None).unwrap_err();
        assert!(error.to_string().contains(message), "{} ({}): {}", name, format, error);
    }
    // An existing namespace is refused before the input is read
    struct Unreadable;
    impl std::io::Read for Unreadable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("read")
        }
    }
    assert_matches!(
        store.import_namespace("default", ImportFormat::Binary, Unreadable, None),
        Err(Error::NamespaceExists { .. })
    );
    assert_eq!(names(&store), ["default"]);
    assert_eq!(ns_entries(dir.path()).len(), 1, "{:?}", ns_entries(dir.path()));
    store.close().unwrap();
}

const LEMON: &str = r#"# The example of LEMON's documentation
@nodes
label   coordinates size    title
0       (10,20)     10      "First node"
1       (80,80)     8       "Second node"
2       (20,80)     10      "Third node"
@arcs
        capacity
0   1   10
1   2   20
2   0   8
@attributes
source 0
caption "LEMON test digraph"
"#;

#[test]
fn files_import_with_their_format_detected() {
    let dir = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let lgf = files.path().join("graph.lgf");
    fs::write(&lgf, LEMON).unwrap();
    let report = store.import_file("lemon", &lgf, None, None).unwrap();
    assert_eq!((report.format, report.nodes, report.edges), (ImportFormat::Lgf, 3, 3));
    assert_eq!(report.dropped, ["@attributes 'source'", "@attributes 'caption'"]);
    let ns = store.namespace("lemon").unwrap();
    assert_eq!(ns.read(|n| n.graph().node_by_id("1").unwrap().data.attr["size"].clone()), Value::Int(8));

    // Exports to files, by extension, read back by detection
    for file in ["out.json", "out.bin", "out"] {
        let path = files.path().join(file);
        let exported = ns.export_file(&path, None, None).unwrap();
        let expected = if file.ends_with(".json") { ExportFormat::Json } else { ExportFormat::Binary };
        assert_eq!((exported.format, exported.seq, exported.nodes), (expected, 1, 3));
        assert_eq!(fs::metadata(&path).unwrap().len(), exported.bytes);
        let name = format!("again_{}", file.replace('.', "_"));
        let again = store.import_file(&name, &path, None, None).unwrap();
        assert_eq!(again.format.name(), expected.name());
        assert_eq!(export(&store, &name, ExportFormat::Json), export(&store, "lemon", ExportFormat::Json));
    }
    // Only the exports are there: no temporary files
    assert_eq!(fs::read_dir(files.path()).unwrap().count(), 4);

    let unknown = files.path().join("notes.txt");
    fs::write(&unknown, "just some text\n").unwrap();
    let error = store.import_file("unknown", &unknown, None, None).unwrap_err();
    assert!(error.to_string().contains("can't tell the format"), "{}", error);
    assert_matches!(store.import_file("missing", &files.path().join("no"), None, None), Err(Error::Io { .. }));
    // A named format overrides detection
    let error = store.import_file("forced", &lgf, Some(ImportFormat::Json), None).unwrap_err();
    assert_matches!(error, Error::InvalidImport { .. });

    // A dropped namespace isn't exported
    let path = files.path().join("dropped.bin");
    let handle = store.namespace("again_out").unwrap();
    store.drop_namespace("again_out", None).unwrap();
    assert_matches!(handle.export_file(&path, None, None), Err(Error::NamespaceDropped { .. }));
    assert!(!path.exists());
    store.close().unwrap();
}

#[test]
fn progress_is_reported_while_reading_and_writing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    store.create_namespace("big", None).unwrap();
    // About 12 MiB of attributes
    let big = store.namespace("big").unwrap();
    for i in 0..24 {
        big.commit(&[node(&format!("n{}", i), &[], attrs(&[("blob", Value::Bytes(vec![i as u8; 512 << 10]))]))])
            .unwrap();
    }
    let mut seen: Vec<Progress> = Vec::new();
    let mut out = Vec::new();
    let exported = big.export(&mut out, ExportFormat::Binary, Some(&mut |p| seen.push(p))).unwrap();
    assert!(seen.len() >= 3, "{:?}", seen);
    assert!(seen.iter().all(|p| p.phase == Phase::Writing));
    assert!(seen.windows(2).all(|w| w[0].bytes < w[1].bytes), "{:?}", seen);
    assert_eq!(seen.last().unwrap().bytes, exported.bytes);
    assert_eq!(exported.bytes, out.len() as u64);

    seen.clear();
    let report = store.import_namespace("copy", ImportFormat::Binary, &out[..], Some(&mut |p| seen.push(p))).unwrap();
    let reading: Vec<u64> = seen.iter().filter(|p| p.phase == Phase::Reading).map(|p| p.bytes).collect();
    let writing: Vec<u64> = seen.iter().filter(|p| p.phase == Phase::Writing).map(|p| p.bytes).collect();
    assert!(reading.len() >= 3 && writing.len() >= 3, "{:?}", seen);
    // Reading first, then writing, each ending with its total
    assert!(seen.iter().position(|p| p.phase == Phase::Writing).unwrap() == reading.len());
    assert_eq!(*reading.last().unwrap(), out.len() as u64);
    assert_eq!(*writing.last().unwrap(), report.checkpoint_bytes);
    store.close().unwrap();
}

/// A backup taken after an import restores it. The WAL archive doesn't
/// have the import: a restore from the archive alone fails for the
/// namespace once the archive holds later segments of it.
#[test]
fn an_imported_namespace_is_backed_up_and_restored_but_not_from_the_archive_alone() {
    let dir = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let mut opts = options(1);
    opts.archive = Some(archive.path().to_path_buf());
    let store = Store::open(dir.path(), opts.clone()).unwrap();
    store.import_namespace("lemon", ImportFormat::Lgf, LEMON.as_bytes(), None).unwrap();
    let ns = store.namespace("lemon").unwrap();
    for i in 0..30 {
        ns.commit(&[node(&format!("x{}", i), &[], attrs(&[("pad", Value::from("p".repeat(100)))]))]).unwrap();
    }
    ns.checkpoint().unwrap();
    let expected = export(&store, "lemon", ExportFormat::Json);
    let backup = tempfile::tempdir().unwrap().path().join("backup");
    store.backup(&backup).unwrap();
    store.close().unwrap();

    let restored = tempfile::tempdir().unwrap().path().join("restored");
    let sources = RestoreSources { backup: Some(backup.clone()), archive: None };
    iwdb::restore(&restored, &sources, RestoreTarget::Latest).unwrap();
    let store = Store::open(&restored, options(1)).unwrap();
    assert_eq!(export(&store, "lemon", ExportFormat::Json), expected);
    store.close().unwrap();

    let from_archive = tempfile::tempdir().unwrap().path().join("from-archive");
    let sources = RestoreSources { backup: None, archive: Some(archive.path().to_path_buf()) };
    let error = iwdb::restore(&from_archive, &sources, RestoreTarget::Latest).unwrap_err();
    assert_matches!(error, Error::MissingRecords { from: 1, .. }, "{}", error);
}
