//! The memory limit through the store (step 16d, ADR 0054): writes above
//! the line are refused before they are logged, and a crash after a refusal
//! recovers exactly the accepted commits; namespace creation and imports
//! are refused too, drops and reads go on; the checkpointers' copies and
//! analytics projections are counted; the error is `resource_exhausted`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::assert_matches;
use std::path::Path;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::import::ImportFormat;
use iwdb::{
    AttrPath, CatalogChange, IndexDef, MemoryOptions, MemoryState, Mutation, ReadOptions, Store, StoreOptions, Value,
};
use iwdb_query::{Code, ProjectionSpec};
use support::{options, reference, state, store_state};

/// Room between what a store holds when it opens and its refusal line.
const HEADROOM: u64 = 200_000;

fn node(id: &str, bytes: usize) -> Vec<Mutation> {
    vec![Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["P".into()],
        attr: [("text".to_owned(), Value::String("x".repeat(bytes)))].into(),
        meta: Default::default(),
        expected_version: None,
    }]
}

/// A store in `dir` whose refusal line is [`HEADROOM`] above what it holds
/// when it opens (measured by opening it once without a limit).
fn limited(fs: &TestFs, dir: &Path) -> Store<TestFs> {
    let used = Store::open_with(fs.clone(), dir, options(2)).unwrap().memory().used();
    let refuse_at = used + HEADROOM;
    let memory = MemoryOptions { limit_bytes: Some(refuse_at * 10 / 9), ..MemoryOptions::default() };
    let store = Store::open_with(fs.clone(), dir, StoreOptions { memory, ..options(2) }).unwrap();
    assert_eq!(store.memory().state, MemoryState::Normal);
    store
}

#[test]
fn a_crash_after_a_refused_commit_recovers_exactly_the_accepted_ones() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let store = limited(&fs, dir.path());
    let mut expected = reference();
    store.create_namespace("other", None).unwrap();

    // Accepted below the line; the second carries the memory over it
    for (id, bytes) in [("small", 1_000), ("crossing", HEADROOM as usize)] {
        assert!(store.memory().used() < store.memory().limit.unwrap().refuse_writes);
        store.commit(&node(id, bytes)).unwrap();
        expected.commit(&node(id, bytes)).unwrap();
    }
    let memory = store.memory();
    assert_eq!(memory.state, MemoryState::RefusingWrites, "{:?}", memory);
    assert!(memory.payload > HEADROOM, "{:?}", memory);

    // Any WAL or namespace-log write from here on fails, so a refused
    // write that reached the log would show as `Io` and a read-only store
    let writes = fs.count(Call::Write);
    fs.add(Rule::new(Call::Write, When::Before, Action::Fail));
    let refused = store.commit(&node("refused", 10));
    assert_matches!(refused, Err(iwdb::Error::MemoryLimit { .. }));
    assert_eq!(iwdb_query::Error::from(refused.unwrap_err()).code(), Code::ResourceExhausted);
    let index = CatalogChange::CreateIndex(IndexDef { path: AttrPath::new(["text"]).unwrap() });
    assert_matches!(store.commit_catalog(index), Err(iwdb::Error::MemoryLimit { .. }));
    assert_matches!(store.create_namespace("new", None), Err(iwdb::Error::MemoryLimit { .. }));
    let file = br#"{"format_version": 2, "nodes": [], "edges": []}"#;
    let import = store.import_namespace("imported", ImportFormat::Json, &file[..], None);
    assert_matches!(import, Err(iwdb::Error::MemoryLimit { .. }));
    assert_eq!(fs.count(Call::Write), writes, "nothing was written");
    assert_eq!(store.read_only(), None);
    fs.clear();

    // Reads go on, and so does a drop
    assert_eq!(store_state(&store), state(&expected));
    store.drop_namespace("other", None).unwrap();

    // A crash, and recovery without a limit: the accepted commits, nothing else
    let seq = store.seq();
    drop(store);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store.seq(), seq);
    assert_eq!(store_state(&store), state(&expected));
    assert!(store.namespace("new").is_err() && store.namespace("imported").is_err());
}

#[test]
fn deleting_brings_the_store_back_below_the_line() {
    let dir = tempfile::tempdir().unwrap();
    let store = limited(&TestFs::default(), dir.path());
    store.commit(&node("big", HEADROOM as usize)).unwrap();
    assert_eq!(store.memory().state, MemoryState::RefusingWrites);
    assert_matches!(store.commit(&node("next", 10)), Err(iwdb::Error::MemoryLimit { .. }));

    store.commit(&[Mutation::DeleteNode { id: "big".into(), expected_version: None }]).unwrap();
    assert_eq!(store.memory().state, MemoryState::Normal);
    store.commit(&node("next", 10)).unwrap();
}

#[test]
fn checkpointer_copies_and_analytics_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    for i in 0..100 {
        store.commit(&node(&format!("n{}", i), 100)).unwrap();
    }
    assert_eq!(store.memory().checkpoint, 0, "no copy before the first checkpoint");
    store.checkpoint().unwrap();
    // The copy replayed the same commits: the live namespace's estimate
    let memory = store.memory();
    assert_eq!(memory.checkpoint, store.read(|ns| ns.memory_bytes() as u64));
    assert!(memory.limit.is_none() && memory.state == MemoryState::Normal);

    let working = store
        .analyze(&ProjectionSpec::default(), &ReadOptions::default(), |_| Ok(store.memory().working))
        .unwrap()
        .value;
    assert!(working > 0);
    assert_eq!(store.memory().working, 0, "released when the job ends");
}

/// The acceptance criterion of step 16d: under a growing write load the
/// store refuses writes at its line, below the limit, and takes them again
/// once deletes have brought memory down.
#[test]
fn a_growing_write_load_is_refused_below_the_limit_and_resumes_when_memory_falls() {
    let dir = tempfile::tempdir().unwrap();
    let limit = 8 << 20;
    let memory = MemoryOptions { limit_bytes: Some(limit), ..MemoryOptions::default() };
    let store = Store::open(dir.path(), StoreOptions { memory, ..options(2) }).unwrap();
    let mut written = 0;
    let refused = loop {
        match store.commit(&node(&format!("n{}", written), 20_000)) {
            Ok(_) => written += 1,
            Err(e) => break e,
        }
        assert!(written < 1000, "never refused");
    };
    assert_matches!(refused, iwdb::Error::MemoryLimit { .. });
    let m = store.memory();
    // Refused from the line on; one commit (20 KB and its node) past it at most
    assert!(
        m.used() >= m.limit.unwrap().refuse_writes && m.used() < m.limit.unwrap().refuse_writes + 100_000,
        "{:?}",
        m
    );
    assert!(m.used() < limit);

    // The application deletes a quarter of what it wrote: writes resume
    for i in 0..written / 4 {
        store.commit(&[Mutation::DeleteNode { id: format!("n{}", i), expected_version: None }]).unwrap();
    }
    // From about 90 % to below 75 %: past both bands
    assert_eq!(store.memory().state, MemoryState::Normal);
    store.commit(&node("again", 20_000)).unwrap();
}

/// Managed jobs (ADR 0056): a running job's projection is in `working`
/// and leaves it when the job ends; its stored result stays there until
/// it expires. Jobs start while writes are refused, as reads do.
#[test]
fn a_job_is_counted_while_it_runs_and_starts_while_writes_are_refused() {
    use ironweaver_core::algo::PageRank;
    use iwdb::{Embedded, QueryConfig};
    use iwdb_query::exec::block_on;
    use iwdb_query::jobs::JobsConfig;
    use iwdb_query::{Admin, AnalyticsRequest, Database, Job, JobState, QueryOptions};
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path(), options(2)).unwrap();
        let mut m: Vec<Mutation> = (0..500).flat_map(|i| node(&format!("n{}", i), 10)).collect();
        m.extend((0..500).map(|i| Mutation::AddEdge {
            from: format!("n{}", i),
            to: format!("n{}", (i + 1) % 500),
            ty: None,
            attr: Default::default(),
            meta: Default::default(),
        }));
        store.commit(&m).unwrap();
        store.close().unwrap();
    }
    // A limit of 1 byte: refusing writes from the start
    let memory = MemoryOptions { limit_bytes: Some(1), ..MemoryOptions::default() };
    let store = Store::open(dir.path(), StoreOptions { memory, ..options(2) }).unwrap();
    let jobs = JobsConfig { retention: Duration::from_millis(300), ..JobsConfig::default() };
    let db = Embedded::new(store, QueryConfig { jobs, ..QueryConfig::default() }).unwrap();
    assert_eq!(db.store().memory().state, MemoryState::RefusingWrites);
    assert_eq!(
        block_on(db.commit("default", node("new", 1), Default::default())).unwrap_err().code(),
        Code::ResourceExhausted
    );
    let working = || db.store().memory().working;
    assert_eq!(working(), 0);

    let endless = Job::PageRank(PageRank { tol: 0.0, max_iter: 1 << 40, ..PageRank::default() });
    let request = |job| AnalyticsRequest { projection: ProjectionSpec::default(), job };
    let job = block_on(db.start_job("default".into(), request(endless), QueryOptions::default(), None)).unwrap();
    let start = Instant::now();
    while block_on(db.job(job.id, None)).unwrap().state != JobState::Running {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(2));
    }
    let projection = working();
    assert!(projection > 0, "the running job's projection is counted");
    block_on(db.cancel_job(job.id, None)).unwrap();
    let start = Instant::now();
    while working() != 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "the projection was never released");
        std::thread::sleep(Duration::from_millis(2));
    }

    // A done job's result stays counted until it expires
    let quick = request(Job::Degree { incoming: false });
    let job = block_on(db.start_job("default".into(), quick, QueryOptions::default(), None)).unwrap();
    let done = loop {
        let j = block_on(db.job(job.id, None)).unwrap();
        if j.state.ended() {
            break j;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(done.state, JobState::Done);
    assert_eq!(working(), done.result_bytes);
    assert_eq!(
        db.store().memory().used(),
        db.store().memory().graph + db.store().memory().payload + db.store().memory().checkpoint + done.result_bytes
    );
    std::thread::sleep(Duration::from_millis(350));
    // Expiry is checked when the registry is used
    assert_eq!(block_on(db.job(job.id, None)).unwrap_err().code(), Code::NotFound);
    assert_eq!(working(), 0);
    db.close().unwrap();
}
