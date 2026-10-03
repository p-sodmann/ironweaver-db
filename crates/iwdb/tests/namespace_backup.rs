//! Backup, archive, PITR and restore with several namespaces (step 9,
//! ADR 0017/0018): one namespace created and one dropped after the backup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::path::Path;

use iwdb::{
    Error, Mutation, NamespaceName, RestoreSources, RestoreTarget, Store, StoreOptions, Value, restore,
    restore_namespaces, verify,
};
use support::{options, state};

fn put(i: i64) -> Mutation {
    Mutation::UpsertNode {
        id: format!("n{}", i % 5),
        labels: vec!["L".into()],
        attr: [("v".to_owned(), Value::Int(i))].into(),
        meta: Default::default(),
        expected_version: None,
    }
}

/// The state of each namespace after each of its commits (index = seq).
#[derive(Default)]
struct Histories(BTreeMap<String, Vec<support::State>>);

impl Histories {
    fn create(&mut self, store: &Store, name: &str) {
        store.create_namespace(name, None).unwrap();
        self.0.insert(name.to_owned(), vec![store.namespace(name).unwrap().read(state)]);
    }

    fn commit(&mut self, store: &Store, name: &str, from: i64, to: i64) {
        let ns = store.namespace(name).unwrap();
        let states = self.0.entry(name.to_owned()).or_insert_with(|| vec![ns.read(state)]);
        for i in from..to {
            ns.commit(&[put(i)]).unwrap();
            states.push(ns.read(state));
        }
    }

    fn at(&self, name: &str, seq: u64) -> &support::State {
        &self.0[name][seq as usize]
    }
}

fn archived(dir: &Path) -> StoreOptions {
    StoreOptions { archive: Some(dir.join("archive")), ..options(1) }
}

fn name(n: &str) -> NamespaceName {
    NamespaceName::new(n).unwrap()
}

/// The namespaces of the restored store at `dest`, with their seqs, after
/// checking each state against the history at that seq.
fn check(dest: &Path, histories: &Histories) -> BTreeMap<String, u64> {
    assert!(verify(dest).unwrap().is_ok(), "{:#?}", verify(dest).unwrap());
    let store = Store::open(dest, options(1)).unwrap();
    let mut seqs = BTreeMap::new();
    for info in store.namespaces() {
        let ns = store.namespace(info.name.as_str()).unwrap();
        let seq = ns.seq();
        assert_eq!(&ns.read(state), histories.at(info.name.as_str(), seq), "{} at {}", info.name, seq);
        seqs.insert(info.name.to_string(), seq);
    }
    seqs
}

#[test]
fn backup_archive_and_restore_with_several_namespaces() {
    let work = tempfile::tempdir().unwrap();
    let data = work.path().join("data");
    let store = Store::open(&data, archived(work.path())).unwrap();
    let mut h = Histories::default();
    h.commit(&store, "default", 0, 12);
    for n in ["a", "b", "c"] {
        h.create(&store, n);
    }
    h.commit(&store, "a", 0, 20);
    h.commit(&store, "b", 0, 7);
    h.commit(&store, "c", 0, 15);
    store.checkpoint_all().unwrap();
    let backup = work.path().join("backup");
    let report = store.backup(&backup).unwrap();
    assert_eq!(report.namespaces.len(), 4);
    assert!(verify(&backup).unwrap().is_ok());

    // After the backup: one namespace created, one dropped, more commits
    h.create(&store, "late");
    h.commit(&store, "late", 0, 30);
    store.drop_namespace("b", None).unwrap();
    h.commit(&store, "a", 20, 80);
    h.commit(&store, "c", 15, 18);
    h.commit(&store, "default", 12, 40);
    store.checkpoint_all().unwrap();
    store.close().unwrap();
    let archive = work.path().join("archive");
    assert!(verify(&archive).unwrap().is_ok(), "{:#?}", verify(&archive).unwrap());

    // The backup alone: the namespaces it holds, at its seqs (b included, late absent)
    let dest = work.path().join("from-backup");
    let sources = RestoreSources { backup: Some(backup.clone()), archive: None };
    restore(&dest, &sources, RestoreTarget::Latest).unwrap();
    let seqs = check(&dest, &h);
    assert_eq!(seqs, BTreeMap::from([("a".into(), 20), ("b".into(), 7), ("c".into(), 15), ("default".into(), 12)]));

    // The archive alone: what existed at the end (late, not b), as far as
    // the archive reaches
    let dest = work.path().join("from-archive");
    let archive_only = RestoreSources { backup: None, archive: Some(archive.clone()) };
    restore(&dest, &archive_only, RestoreTarget::Latest).unwrap();
    let seqs = check(&dest, &h);
    assert_eq!(seqs.keys().collect::<Vec<_>>(), ["a", "c", "default", "late"]);

    // Both: at least as far as the backup, and further for the busy ones
    let dest = work.path().join("from-both");
    let both = RestoreSources { backup: Some(backup.clone()), archive: Some(archive.clone()) };
    restore(&dest, &both, RestoreTarget::Latest).unwrap();
    let seqs = check(&dest, &h);
    assert_eq!(seqs.keys().collect::<Vec<_>>(), ["a", "c", "default", "late"]);
    assert!(seqs["a"] > 20 && seqs["default"] > 12 && seqs["late"] > 0, "{:?}", seqs);

    // A seq means one namespace
    let dest = work.path().join("by-seq");
    match restore(&dest, &both, RestoreTarget::Seq(3)) {
        Err(Error::AmbiguousTarget { .. }) => {}
        other => panic!("{:?}", other.map(|r| r.path)),
    }
    let only = [name("a")];
    restore_namespaces(&dest, &both, RestoreTarget::Seq(25), Some(&only)).unwrap();
    // Opening adds the (empty) default namespace a store always has
    assert_eq!(check(&dest, &h), BTreeMap::from([("a".into(), 25), ("default".into(), 0)]));

    // Only some, to the latest
    let dest = work.path().join("only-c");
    let only = [name("c")];
    restore_namespaces(&dest, &both, RestoreTarget::Latest, Some(&only)).unwrap();
    assert_eq!(check(&dest, &h).keys().collect::<Vec<_>>(), ["c", "default"]);
    // A namespace that never existed
    let missing = [name("nope")];
    assert!(restore_namespaces(&work.path().join("x"), &both, RestoreTarget::Latest, Some(&missing)).is_err());
}
