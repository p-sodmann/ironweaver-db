//! The memory limit in the commit pipeline (step 16d, ADR 0054): a refused
//! commit never reaches the WAL (failpoints on every write prove that none
//! is attempted), a commit accepted just below the line is durable, and the
//! writes that free memory, repeat a keyed commit or go to the system
//! namespace pass while writes are refused. A charge from outside the
//! namespace sets the memory exactly where a test needs it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::assert_matches;
use std::path::Path;
use std::sync::Arc;

use common::{Action, Call, Rule, TestFs, When, namespace, upsert};
use ironweaver_core::Value;
use iwdb_engine::catalog::{AttrPath, IndexDef, NamespaceName};
use iwdb_engine::{CatalogChange, IdempotencyKey, Mutation, Namespace, Target};
use iwdb_storage::memory::{Charge, Memory, MemoryOptions, MemoryState, Part};
use iwdb_storage::{Error, FsyncPolicy, LoggedNamespace, Wal, WalOptions, read_log};

/// Lines at 800 000 (warn) and 900 000 (refuse), a band of 50 000.
const LIMIT: u64 = 1_000_000;
const REFUSE: u64 = 900_000;
const BAND: u64 = 50_000;

fn memory() -> Arc<Memory> {
    Memory::new(&MemoryOptions { limit_bytes: Some(LIMIT), ..MemoryOptions::default() })
}

fn logged(fs: &TestFs, dir: &Path, ns: Namespace, memory: &Arc<Memory>) -> LoggedNamespace<TestFs> {
    let options = WalOptions { fsync: FsyncPolicy::Always, ..WalOptions::default() };
    let wal = Wal::create_with(fs.clone(), dir, options, ns.seq() + 1).unwrap();
    LoggedNamespace::new(ns, wal).unwrap().with_memory(memory)
}

/// Set `other` so that the memory in use is `used`.
fn use_exactly(memory: &Memory, other: &Charge, used: u64) {
    other.set(used - (memory.used() - other.bytes()));
    assert_eq!(memory.used(), used);
}

fn index(key: &str) -> CatalogChange {
    CatalogChange::CreateIndex(IndexDef { path: AttrPath::new([key]).unwrap() })
}

#[test]
fn a_refused_commit_is_never_logged_and_one_below_the_line_is_durable() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let memory = memory();
    let other = memory.charge(Part::Working);
    let ns = logged(&fs, dir.path(), namespace(), &memory);
    assert!(memory.part(Part::Graph) > 0, "the namespace charges its graph");

    // One byte below the line: accepted, and it carries the memory over
    use_exactly(&memory, &other, REFUSE - 1);
    assert_eq!(memory.state(), MemoryState::Warn);
    let below = ns.commit(&[upsert("below", Value::String("x".repeat(100)))]).unwrap();
    assert_eq!(memory.state(), MemoryState::RefusingWrites);

    // Every write and fsync fails from here on: a refused commit that tried
    // to log would make the namespace read-only
    let (writes, syncs) = (fs.count(Call::Write), fs.count(Call::Sync));
    fs.add(Rule::new(Call::Write, When::Before, Action::Fail));
    fs.add(Rule::new(Call::Sync, When::Before, Action::Fail));
    let refused = ns.commit(&[upsert("refused", Value::Int(1))]);
    assert_matches!(refused, Err(Error::MemoryLimit { refuse_at: REFUSE, limit: LIMIT, .. }));
    assert_matches!(ns.commit_catalog(index("x")), Err(Error::MemoryLimit { .. }));
    assert!(ns.builds().is_empty(), "no index build started");
    let set = Mutation::SetAttr {
        target: Target::Node("below".into()),
        key: "y".into(),
        value: Value::Int(2),
        expected_version: None,
    };
    assert_matches!(ns.commit(&[set]), Err(Error::MemoryLimit { .. }));
    assert_eq!((fs.count(Call::Write), fs.count(Call::Sync)), (writes, syncs), "nothing was written");
    assert_eq!(ns.read_only(), None);
    assert_eq!(ns.seq(), below.seq);
    // Reads go on
    assert!(ns.read(|n| n.graph().contains_node("below")));
    fs.clear();

    // A crash: the log holds the accepted commit and nothing after it
    drop(ns);
    let (records, _) = read_log(dir.path(), 1).unwrap();
    assert_eq!(records.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![below.seq]);
    let recovered = common::replay(records);
    assert!(recovered.graph().contains_node("below"));
    assert!(!recovered.graph().contains_node("refused"));
    assert_eq!(recovered.seq(), below.seq);
}

#[test]
fn removals_repeats_and_the_system_namespace_pass_while_writes_are_refused() {
    let (dir, system_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let fs = TestFs::default();
    let memory = memory();
    let other = memory.charge(Part::Working);
    let ns = logged(&fs, dir.path(), namespace(), &memory);
    let key = IdempotencyKey::new("k-1").unwrap();
    let a = [upsert("a", Value::String("x".repeat(100)))];
    let keyed = ns.commit_keyed(&a, Some(&key)).unwrap();
    ns.commit(&[upsert("b", Value::Int(1))]).unwrap();
    ns.commit_catalog(index("x")).unwrap();

    use_exactly(&memory, &other, REFUSE);
    assert_eq!(memory.state(), MemoryState::RefusingWrites);
    assert_matches!(ns.commit(&[upsert("c", Value::Int(1))]), Err(Error::MemoryLimit { .. }));

    // The keyed commit's repeat answers its result
    let again = ns.commit_keyed(&a, Some(&key)).unwrap();
    assert!(again.deduplicated);
    assert_eq!(again.seq, keyed.seq);
    // Removing an attribute, a node, an index
    let remove = Mutation::RemoveAttr { target: Target::Node("a".into()), key: "x".into(), expected_version: None };
    ns.commit(&[remove]).unwrap();
    ns.commit(&[Mutation::DeleteNode { id: "b".into(), expected_version: None }]).unwrap();
    let path = AttrPath::new(["x"]).unwrap();
    ns.commit_catalog(CatalogChange::DropIndex(IndexDef { path })).unwrap();
    assert_eq!(ns.read(|n| n.graph().node_count()), 1);

    // The system namespace (users, grants, session tokens) isn't limited
    let system = logged(&fs, system_dir.path(), Namespace::new(NamespaceName::system()), &memory);
    system.commit(&[upsert("admin", Value::Int(1))]).unwrap();
    assert_eq!(memory.state(), MemoryState::RefusingWrites);
}

#[test]
fn writes_resume_only_once_memory_is_below_the_band() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let memory = memory();
    let other = memory.charge(Part::Working);
    let ns = logged(&fs, dir.path(), namespace(), &memory);
    let mut i = 0;
    let mut commit = || {
        i += 1;
        // Values the same size as before: the payload doesn't grow
        ns.commit(&[upsert("n", Value::Int(i))])
    };
    commit().unwrap();

    use_exactly(&memory, &other, REFUSE);
    assert_matches!(commit(), Err(Error::MemoryLimit { .. }));
    // Below the line, inside the band: still refused
    use_exactly(&memory, &other, REFUSE - 1);
    assert_matches!(commit(), Err(Error::MemoryLimit { .. }));
    use_exactly(&memory, &other, REFUSE - BAND);
    assert_matches!(commit(), Err(Error::MemoryLimit { .. }));
    // Below the band: accepted, warning
    use_exactly(&memory, &other, REFUSE - BAND - 1);
    assert_eq!(memory.state(), MemoryState::Warn);
    commit().unwrap();
    // Up to the line again: refused at once
    use_exactly(&memory, &other, REFUSE);
    assert_matches!(commit(), Err(Error::MemoryLimit { .. }));
    drop(other);
    assert_eq!(memory.state(), MemoryState::Normal);
    commit().unwrap();
}

#[test]
fn a_namespace_charges_its_graph_with_its_payloads_until_it_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let memory = Memory::unlimited();
    let ns = logged(&TestFs::default(), dir.path(), namespace(), &memory);
    ns.commit(&[upsert("a", Value::String("x".repeat(10_000)))]).unwrap();
    let graph = ns.read(|n| n.memory_bytes() as u64);
    assert!(graph > 10_000, "the payload counts (upstream #61): {}", graph);
    assert_eq!(memory.part(Part::Graph), graph);
    // Without a limit nothing is refused
    assert_eq!(memory.state(), MemoryState::Normal);
    drop(ns);
    assert_eq!(memory.used(), 0);
}
