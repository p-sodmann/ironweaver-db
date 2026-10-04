//! Projection mode (ADR 0032): a projection's mark moves in the commit that
//! applies its events, so crashes never make it apply an event twice. The
//! mapping here is not idempotent (it appends the event's position to a
//! list), so an event applied twice shows up as a duplicate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::projection::{
    Mapping, MappingError, OnError, Projection, ProjectionError, ProjectionOptions, ProjectionState, Source,
    SourceError, SourceEvent,
};
use iwdb::{Error, FsyncPolicy, MarkName, Mutation, Store, Target, Value};
use iwdb_storage::failpoint::FailFs;
use std::assert_matches;
use support::options;

const NODES: u64 = 7;

/// An in-memory event log the test can append to; it can fail its next
/// reads.
#[derive(Clone, Default)]
struct Log {
    events: Arc<Mutex<Vec<SourceEvent>>>,
    failures: Arc<Mutex<usize>>,
}

impl Log {
    fn with(n: u64) -> Self {
        let log = Log::default();
        log.append(n);
        log
    }

    /// Append events up to position `n`: event `i` is for node `n{i % 7}`,
    /// or for a node that doesn't exist when `i` is in `bad`.
    fn append(&self, n: u64) {
        self.append_bad(n, &[]);
    }

    fn append_bad(&self, n: u64, bad: &[u64]) {
        let mut events = self.events.lock().unwrap();
        let from = events.last().map_or(1, |e| e.position + 1);
        for i in from..=n {
            let node = if bad.contains(&i) { "missing".to_owned() } else { format!("n{}", i % NODES) };
            let fields = [("node".to_owned(), Value::String(node)), ("i".to_owned(), Value::Int(i as i64))].into();
            events.push(SourceEvent { position: i, fields });
        }
    }

    fn fail_next(&self, reads: usize) {
        *self.failures.lock().unwrap() = reads;
    }
}

impl Source for Log {
    fn read(&mut self, after: u64, limit: usize) -> Result<Vec<SourceEvent>, SourceError> {
        let mut failures = self.failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(SourceError::new("the log is unavailable"));
        }
        let events = self.events.lock().unwrap();
        Ok(events.iter().filter(|e| e.position > after).take(limit).cloned().collect())
    }
}

/// Append the event's position to its node's `seen` list: applying an
/// event twice leaves a duplicate. An event without a node is a mapping
/// error.
fn append(event: &SourceEvent) -> Result<Vec<Mutation>, MappingError> {
    let Some(Value::String(node)) = event.fields.get("node") else {
        return Err(MappingError::new("no node"));
    };
    Ok(vec![Mutation::AppendAttr {
        target: Target::Node(node.clone()),
        key: "seen".into(),
        value: Value::Int(event.position as i64),
        expected_version: None,
    }])
}

fn name() -> MarkName {
    MarkName::new("feed").unwrap()
}

fn projection(log: &Log, batch: usize, on_error: OnError) -> Projection {
    let options =
        ProjectionOptions { batch, poll: Duration::from_millis(5), on_error, max_backoff: Duration::from_millis(10) };
    Projection::new(name(), log.clone(), append, options)
}

fn create_nodes<F: iwdb::LogFs + Clone + Send + Sync + 'static>(store: &Store<F>)
where
    F::File: Send,
{
    let nodes = (0..NODES).map(|k| Mutation::UpsertNode {
        id: format!("n{}", k),
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    });
    store.commit(&nodes.collect::<Vec<_>>()).unwrap();
}

/// Step until the source has nothing more, or an error.
fn drain<F: iwdb::LogFs + Clone + Send + Sync + 'static>(
    projection: &mut Projection,
    store: &Store<F>,
) -> Result<(), ProjectionError>
where
    F::File: Send,
{
    let ns = store.default_namespace();
    while projection.step(&ns)?.read > 0 {}
    Ok(())
}

/// What each node's `seen` list holds.
fn seen<F: iwdb::LogFs + Clone + Send + Sync + 'static>(store: &Store<F>) -> HashMap<String, Vec<i64>>
where
    F::File: Send,
{
    (0..NODES)
        .map(|k| {
            let id = format!("n{}", k);
            let list = match store.node(&id).unwrap().attr.get("seen") {
                Some(Value::List(items)) => {
                    items.iter().map(|v| if let Value::Int(i) = v { *i } else { panic!("{:?}", v) }).collect()
                }
                None => vec![],
                other => panic!("{:?}", other),
            };
            (id, list)
        })
        .collect()
}

/// Every event up to `n`, each once and in order, except `skipped`.
fn assert_exactly_once<F: iwdb::LogFs + Clone + Send + Sync + 'static>(
    store: &Store<F>,
    n: u64,
    skipped: &[u64],
    what: &str,
) where
    F::File: Send,
{
    let seen = seen(store);
    for k in 0..NODES {
        let expected: Vec<i64> = (1..=n).filter(|i| i % NODES == k && !skipped.contains(i)).map(|i| i as i64).collect();
        assert_eq!(seen[&format!("n{}", k)], expected, "node n{}: {}", k, what);
    }
    assert_eq!(store.default_namespace().mark(&name()), Some(n), "{}", what);
}

/// The acceptance criterion: a failure at each point of the commit path
/// (the WAL write before, halfway and after, the fsync before and after)
/// stops the projection with the commit's outcome unknown. After a restart
/// it finishes, and every event is applied exactly once: a commit that
/// reached the log moved the mark with it, one that didn't moved neither.
#[test]
fn a_projection_survives_crashes_without_applying_an_event_twice() {
    let failpoints = [
        Rule::new(Call::Write, When::Before, Action::Fail),
        Rule::new(Call::Write, When::Midway, Action::NoSpace),
        Rule::new(Call::Write, When::After, Action::Fail),
        Rule::new(Call::Sync, When::Before, Action::Fail),
        Rule::new(Call::Sync, When::After, Action::Fail),
    ];
    let n = 60;
    for rule in failpoints {
        for skip in [0, 2, 7] {
            for batch in [1, 4] {
                let what = format!("{:?} after {} writes, batch {}", rule, skip, batch);
                let dir = tempfile::tempdir().unwrap();
                let log = Log::with(n);
                let fs = TestFs::default();
                let store = Store::open_with(fs.clone(), dir.path(), options(2)).unwrap();
                create_nodes(&store);
                let mut first = projection(&log, batch, OnError::Stop);
                fs.add(rule.clone().skip(skip));
                let error = drain(&mut first, &store).unwrap_err();
                assert_matches!(error, ProjectionError::Store(Error::Io { .. }), "{}", what);
                // The store is read-only now: no further step commits
                assert_matches!(drain(&mut first, &store), Err(ProjectionError::Store(_)), "{}", what);
                // A crash: no close
                drop(store);

                let store = Store::open(dir.path(), options(2)).unwrap();
                let mark = store.default_namespace().mark(&name()).unwrap_or(0);
                assert!(mark < n, "{}: the failure came before the end", what);
                // A new process: a new projection, resuming from the mark
                drain(&mut projection(&log, batch, OnError::Stop), &store).unwrap();
                assert_exactly_once(&store, n, &[], &what);
                // And across a checkpoint and a restart, nothing more
                store.close().unwrap();
                let store = Store::open(dir.path(), options(2)).unwrap();
                drain(&mut projection(&log, batch, OnError::Stop), &store).unwrap();
                assert_exactly_once(&store, n, &[], &what);
            }
        }
    }
}

/// The largest length each WAL file was fsynced at.
type Synced = Arc<Mutex<HashMap<PathBuf, u64>>>;

fn recording_fs() -> (FailFs, Synced) {
    let fs = FailFs::new();
    let synced: Synced = Arc::default();
    let log = synced.clone();
    fs.set_hook(Some(Arc::new(move |call, path: &Path| {
        if call == Call::Sync
            && let Ok(meta) = std::fs::metadata(path)
        {
            let mut log = log.lock().unwrap();
            let len = log.entry(path.to_path_buf()).or_default();
            *len = (*len).max(meta.len());
        }
    })));
    (fs, synced)
}

/// An OS crash: each WAL file loses what was written after its last fsync.
fn os_crash(dir: &Path, synced: &HashMap<PathBuf, u64>) -> u64 {
    let mut lost = 0;
    let wal = dir.join("ns").read_dir().unwrap().map(|e| e.unwrap().path().join("wal"));
    for segment in wal.flat_map(|w| w.read_dir().unwrap().map(|e| e.unwrap().path()).collect::<Vec<_>>()) {
        let len = std::fs::metadata(&segment).unwrap().len();
        let durable = synced.get(&segment).copied().unwrap_or(0).min(len);
        if durable < len {
            std::fs::OpenOptions::new().write(true).open(&segment).unwrap().set_len(durable).unwrap();
            lost += len - durable;
        }
    }
    lost
}

/// Under group commit an OS crash loses the last commits, and their marks
/// with them: the projection reads those events again, and applies them
/// once.
#[test]
fn an_os_crash_under_group_commit_loses_events_and_their_marks_together() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, synced) = recording_fs();
    let mut opts = options(2);
    opts.wal.fsync = FsyncPolicy::Group { max_delay: Duration::from_secs(3600), max_batch: 1_000_000 };
    let n = 80;
    let log = Log::with(n);
    {
        let store = Store::open_with(fs.clone(), dir.path(), opts.clone()).unwrap();
        create_nodes(&store);
        store.sync().unwrap();
        drain(&mut projection(&log, 3, OnError::Stop), &store).unwrap();
        assert_eq!(store.default_namespace().mark(&name()), Some(n));
    }
    assert!(os_crash(dir.path(), &synced.lock().unwrap()) > 0, "the crash lost something");
    let store = Store::open(dir.path(), opts).unwrap();
    let mark = store.default_namespace().mark(&name()).unwrap_or(0);
    assert!(mark < n, "the crash took back events: mark {}", mark);
    drain(&mut projection(&log, 3, OnError::Stop), &store).unwrap();
    assert_exactly_once(&store, n, &[], "after an OS crash");
}

/// Two projectors with the same name: the mark is compare-and-set, so one
/// of them loses each race and commits nothing.
#[test]
fn two_projectors_with_one_name_apply_each_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    create_nodes(&store);
    let log = Log::with(50);
    let (mut a, mut b) = (projection(&log, 5, OnError::Stop), projection(&log, 3, OnError::Stop));
    let ns = store.default_namespace();
    // `b` reads the mark, then `a` moves it: `b`'s commit conflicts
    struct Stale<'a, T: iwdb::projection::Target>(&'a T, Option<u64>);
    impl<T: iwdb::projection::Target> iwdb::projection::Target for Stale<'_, T> {
        fn mark(&self, _: &MarkName) -> Result<Option<u64>, Error> {
            Ok(self.1)
        }
        fn commit(&self, m: &[Mutation], u: &iwdb::MarkUpdate) -> Result<iwdb::CommitResult, Error> {
            self.0.commit(m, u)
        }
    }
    a.step(&ns).unwrap();
    let progress = b.step(&Stale(&ns, None)).unwrap();
    assert!(progress.conflict && progress.applied == 0, "{:?}", progress);
    // Alternating, both make progress, and nothing is applied twice
    let mut conflicts = 0;
    loop {
        let (pa, pb) = (a.step(&ns).unwrap(), b.step(&ns).unwrap());
        conflicts += usize::from(pa.conflict) + usize::from(pb.conflict);
        if pa.read == 0 && pb.read == 0 {
            break;
        }
    }
    assert_eq!(conflicts, 0, "sequential steps read the mark fresh");
    assert_exactly_once(&store, 50, &[], "two projectors");
}

/// An event whose commit fails (a missing node) stops the projection
/// before it, or is skipped with `skip`; so is an event that can't be
/// mapped.
#[test]
fn a_bad_event_stops_the_projection_or_is_skipped() {
    for batch in [1, 10] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), options(2)).unwrap();
        create_nodes(&store);
        let log = Log::default();
        log.append_bad(30, &[12]);
        // A mapping error at 20: no node field
        log.events.lock().unwrap()[19].fields.remove("node");

        let error = drain(&mut projection(&log, batch, OnError::Stop), &store).unwrap_err();
        assert_matches!(error, ProjectionError::Event { position: 12, .. }, "batch {}", batch);
        assert_eq!(store.default_namespace().mark(&name()), Some(11), "batch {}", batch);

        let mut skipping = projection(&log, batch, OnError::Skip);
        let ns = store.default_namespace();
        let mut skipped = 0;
        loop {
            let progress = skipping.step(&ns).unwrap();
            skipped += progress.skipped;
            if progress.read == 0 {
                break;
            }
        }
        assert_eq!(skipped, 2, "batch {}", batch);
        assert_exactly_once(&store, 30, &[12, 20], &format!("batch {}", batch));
    }
}

/// `Store::project` runs a projection on its own thread: it follows the
/// source as it grows, retries source errors, stops on its handle, and the
/// store's close stops the others.
#[test]
fn the_store_runs_projections_until_they_are_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    create_nodes(&store);
    let log = Log::with(20);
    let handle = store.project(iwdb::NAMESPACE, projection(&log, 4, OnError::Stop)).unwrap();
    let caught_up = |n| move |s: &iwdb::projection::ProjectionStatus| s.mark == Some(n);
    let status = handle.wait_until(Duration::from_secs(10), caught_up(20));
    assert_eq!((status.mark, status.applied), (Some(20), 20), "{:?}", status);
    // The source fails twice, then has more
    log.fail_next(2);
    log.append(35);
    let status = handle.wait_until(Duration::from_secs(10), caught_up(35));
    assert_eq!((status.mark, status.applied, status.error.clone()), (Some(35), 35, None), "{:?}", status);
    handle.stop();
    assert_eq!(handle.status().state, ProjectionState::Stopped);
    log.append(40);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(store.default_namespace().mark(&name()), Some(35), "a stopped projection commits nothing");

    // A bad event fails it, with the mark before the event
    log.append_bad(45, &[43]);
    let handle = store.project(iwdb::NAMESPACE, projection(&log, 4, OnError::Stop)).unwrap();
    let status = handle.wait_until(Duration::from_secs(10), |s| s.state == ProjectionState::Failed);
    assert_eq!(status.mark, Some(42), "{:?}", status);
    assert!(status.error.unwrap().contains("event 43"));

    // Close stops a running one
    let skipping = store.project(iwdb::NAMESPACE, projection(&log, 4, OnError::Skip)).unwrap();
    skipping.wait_until(Duration::from_secs(10), caught_up(45));
    assert!(matches!(store.project("nope", projection(&log, 1, OnError::Stop)), Err(Error::NoSuchNamespace { .. })));
    assert_eq!(store.projections().len(), 3);
    store.close().unwrap();
    assert_eq!(skipping.status().state, ProjectionState::Stopped);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_exactly_once(&store, 45, &[43], "projected by the store");
}

/// A mapping can be any `Mapping`, not only a closure.
#[test]
fn mappings_are_pluggable() {
    struct Nothing;
    impl Mapping for Nothing {
        fn map(&self, _: &SourceEvent) -> Result<Vec<Mutation>, MappingError> {
            Ok(vec![])
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let log = Log::with(9);
    let mut p = Projection::new(name(), log, Nothing, ProjectionOptions { batch: 4, ..Default::default() });
    drain(&mut p, &store).unwrap();
    // Events that change nothing still move the mark, in a commit per batch
    assert_eq!((store.default_namespace().mark(&name()), store.seq()), (Some(9), 3));
}
