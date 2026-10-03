//! Graceful shutdown (ADR 0027, design rule 3): every commit acknowledged
//! before shutdown survives an OS crash after it, because shutdown flushes
//! the WAL whatever the fsync policy; and calls still running at the end of
//! the drain are cancelled.
//!
//! The OS crash is simulated like the crash harness does (`tests/crash`,
//! `os_crash.rs`): a hook records, before every fsync, the length the file
//! will be durable at; after shutdown every WAL file is cut back to it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use iwdb::{FsyncPolicy, Mutation, Store};
use iwdb_engine::IdempotencyKey;
use iwdb_query::exec::block_on;
use iwdb_query::{CommitOptions, Database, MatchRequest, QueryOptions};
use iwdb_server::client::Remote;
use iwdb_storage::failpoint::{Call, FailFs};
use support::{Running, embedded, options};

mod support;

const NS: &str = "default";

/// The largest length each file was fsynced at.
type Synced = Arc<Mutex<HashMap<PathBuf, u64>>>;

fn recording_fs() -> (FailFs, Synced) {
    let fs = FailFs::new();
    let synced: Synced = Arc::default();
    let log = synced.clone();
    fs.set_hook(Some(Arc::new(move |call, path: &Path| {
        if call == Call::Sync {
            if let Ok(meta) = std::fs::metadata(path) {
                let mut log = log.lock().unwrap();
                let len = log.entry(path.to_path_buf()).or_default();
                *len = (*len).max(meta.len());
            }
        }
    })));
    (fs, synced)
}

/// Every file under a `wal` directory below `dir`.
fn wal_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(wal_files(&path));
        } else if path.parent().is_some_and(|p| p.ends_with("wal")) {
            out.push(path);
        }
    }
    out
}

/// An OS crash: each WAL file loses what was written after its last fsync.
/// Returns the bytes lost.
fn os_crash(dir: &Path, synced: &HashMap<PathBuf, u64>) -> u64 {
    let mut lost = 0;
    for path in wal_files(dir) {
        let len = std::fs::metadata(&path).unwrap().len();
        let durable = synced.get(&path).copied().unwrap_or(0).min(len);
        if durable < len {
            std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(durable).unwrap();
            lost += len - durable;
        }
    }
    lost
}

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["N".into()],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    }
}

/// Clients commit until the server goes away; returns the ids of the
/// commits acknowledged.
fn commit_until_shutdown(endpoint: &str, stopping: &Arc<AtomicBool>) -> Vec<String> {
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let (endpoint, stopping) = (endpoint.to_owned(), stopping.clone());
            thread::spawn(move || {
                let client = Remote::connect(&endpoint).unwrap();
                let mut acked = Vec::new();
                for i in 0.. {
                    let id = format!("t{}-{:05}", t, i);
                    let key = Some(IdempotencyKey::new(format!("k-{}", id)).unwrap());
                    match block_on(client.commit(NS, vec![node(&id)], CommitOptions { idempotency_key: key })) {
                        Ok(_) => acked.push(id),
                        Err(e) => {
                            assert!(stopping.load(Ordering::SeqCst), "a commit failed before shutdown: {}", e);
                            break;
                        }
                    }
                }
                acked
            })
        })
        .collect();
    threads.into_iter().flat_map(|t| t.join().unwrap()).collect()
}

fn acknowledged_commits_survive_shutdown_and_an_os_crash(policy: FsyncPolicy, checkpoint: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (fs, synced) = recording_fs();
    let mut opts = options();
    opts.wal.fsync = policy;
    opts.checkpoint.on_close = checkpoint;
    let server = Running::start(embedded(fs, dir.path(), opts));
    let endpoint = server.endpoint();
    let stopping = Arc::new(AtomicBool::new(false));
    let acked = thread::scope(|s| {
        let clients = s.spawn(|| commit_until_shutdown(&endpoint, &stopping));
        thread::sleep(Duration::from_millis(400));
        stopping.store(true, Ordering::SeqCst);
        let (report, db) = server.shutdown(Duration::from_secs(10));
        assert!(report.complete, "{:?}", report);
        let before_close = synced.lock().unwrap().clone();
        // Close: finish the queue, flush every WAL, checkpoint if asked
        db.close().unwrap();
        let acked = clients.join().unwrap();
        (acked, before_close)
    });
    let (acked, before_close) = acked;
    assert!(acked.len() > 20, "only {} commits acknowledged", acked.len());
    if matches!(policy, FsyncPolicy::Off) && !checkpoint {
        // Without the flush at close, the crash would lose acknowledged
        // commits: the test would notice
        let unflushed: u64 = wal_files(dir.path())
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len() - before_close.get(p).copied().unwrap_or(0))
            .sum();
        assert!(unflushed > 0, "nothing was left to flush at close");
    }
    let lost = os_crash(dir.path(), &synced.lock().unwrap());
    assert_eq!(lost, 0, "shutdown left unsynced WAL bytes");
    let store = Store::open(dir.path(), options()).unwrap();
    let missing: Vec<&String> = acked.iter().filter(|id| store.node(id).is_none()).collect();
    assert!(
        missing.is_empty(),
        "{} of {} acknowledged commits lost: {:?}",
        missing.len(),
        acked.len(),
        &missing[..missing.len().min(5)]
    );
    store.close().unwrap();
}

#[test]
fn acknowledged_commits_survive_with_fsync_off_and_no_checkpoint() {
    acknowledged_commits_survive_shutdown_and_an_os_crash(FsyncPolicy::Off, false);
}

#[test]
fn acknowledged_commits_survive_with_group_commit_and_a_checkpoint() {
    let group = FsyncPolicy::Group { max_delay: Duration::from_secs(60), max_batch: 1_000_000 };
    acknowledged_commits_survive_shutdown_and_an_os_crash(group, true);
}

/// A complete graph of `n` nodes: billions of trails for `(a)-[*1..10]->(b)`.
fn complete(n: usize) -> Vec<Mutation> {
    let mut m: Vec<Mutation> = (0..n).map(|i| node(&format!("n{}", i))).collect();
    for a in 0..n {
        for b in 0..n {
            if a != b {
                m.push(Mutation::AddEdge {
                    from: format!("n{}", a),
                    to: format!("n{}", b),
                    ty: None,
                    attr: Default::default(),
                    meta: Default::default(),
                });
            }
        }
    }
    m
}

#[test]
fn calls_still_running_at_the_end_of_the_drain_are_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(embedded(FailFs::new(), dir.path(), options()));
    let remote = server.client();
    block_on(remote.commit(NS, complete(30), CommitOptions::default())).unwrap();
    let long = QueryOptions { timeout: Some(Duration::from_secs(120)), ..QueryOptions::default() }.with_limits(
        Some(usize::MAX),
        Some(usize::MAX),
        Some(usize::MAX),
    );
    let call = remote.match_pattern(NS, MatchRequest::parse("(a)-[*1..10]->(b)").unwrap(), long);
    thread::sleep(Duration::from_millis(300));
    let start = Instant::now();
    let (report, db) = server.shutdown(Duration::from_millis(200));
    assert!(!report.complete && report.cancelled == 1, "{:?}", report);
    // The read was cancelled: closing the database doesn't wait for it
    db.close().unwrap();
    assert!(start.elapsed() < Duration::from_secs(10), "shutdown took {:?}", start.elapsed());
    let e = block_on(call).unwrap_err();
    assert!(
        matches!(e.code(), iwdb_query::Code::Unavailable | iwdb_query::Code::Cancelled | iwdb_query::Code::Internal),
        "{}",
        e
    );
    // New calls find no server
    let e = block_on(remote.namespaces()).unwrap_err();
    assert_eq!(e.code(), iwdb_query::Code::Unavailable, "{}", e);
}
