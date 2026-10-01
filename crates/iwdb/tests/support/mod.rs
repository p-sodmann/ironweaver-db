//! Helpers for the store tests: options, running a workload against a
//! store and a reference namespace, and reading files.

#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{
    CheckpointOptions, Error, FsyncPolicy, LogFs, Namespace, NamespaceCatalog, Store, StoreOptions, WalOptions,
};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::testutil::canonical;
use iwdb_storage::MIN_SEGMENT_SIZE;

pub use iwdb_engine::testutil::workload::{self, pad, Step};

/// `always`, 1 KiB segments (many rotations), no background threads, keep
/// `keep` checkpoints.
pub fn options(keep: usize) -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync: FsyncPolicy::Always, segment_size: MIN_SEGMENT_SIZE },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: true, keep, background: false },
        create_if_missing: true,
        archive: None,
    }
}

pub fn reference() -> Namespace {
    Namespace::new(NamespaceName::new(iwdb::NAMESPACE).unwrap())
}

/// The observable state: canonical graph, catalog, seq.
pub type State = (Vec<String>, NamespaceCatalog, u64);

pub fn state(ns: &Namespace) -> State {
    (canonical(ns.graph()), ns.catalog().clone(), ns.seq())
}

pub fn store_state<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>) -> State
where
    F::File: Send,
{
    store.read(state)
}

/// Run `steps` against the store and the reference: the same outcome for
/// each (a result, or the same engine error).
pub fn run<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, reference: &mut Namespace, steps: &[Step])
where
    F::File: Send,
{
    for step in steps {
        let (outcome, expected) = match step {
            Step::Tx(mutations) => (store.commit(mutations), reference.commit(mutations)),
            Step::Catalog(change) => (store.commit_catalog(change.clone()), reference.commit_catalog(change.clone())),
        };
        match (outcome, expected) {
            (Ok(a), Ok(b)) => assert_eq!(a, b),
            (Err(Error::Engine(a)), Err(b)) => assert_eq!(a, b),
            (outcome, expected) => panic!("store {:?}, reference {:?}", outcome, expected),
        }
    }
}

/// Every file under `dir` with its bytes, sorted by path.
pub fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

/// The checkpoint seqs in a data directory.
pub fn checkpoints(dir: &Path) -> Vec<u64> {
    iwdb_storage::checkpoint::list_checkpoints(&dir.join("checkpoints")).unwrap().into_iter().map(|(s, _)| s).collect()
}

/// The first seqs of the WAL segments in a data directory.
pub fn segment_seqs(dir: &Path) -> Vec<u64> {
    iwdb_storage::list_segments(&dir.join("wal")).unwrap().into_iter().map(|(s, _)| s).collect()
}

pub fn checkpoint_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join("checkpoints").join(iwdb_storage::checkpoint::checkpoint_name(seq))
}

pub fn last_segment(dir: &Path) -> PathBuf {
    iwdb_storage::list_segments(&dir.join("wal")).unwrap().pop().unwrap().1
}

/// A data record frame of the current WAL format
/// (`documentation/formats/wal.md`).
pub fn frame(seq: u64, synced_seq: u64, payload: &[u8]) -> Vec<u8> {
    use iwdb_storage::format::{encode_frame, FrameHeader, FORMAT_VERSION, KIND_DATA};
    let mut out = Vec::new();
    encode_frame(&mut out, FORMAT_VERSION, FrameHeader { seq, synced_seq, time: 0, kind: KIND_DATA }, payload);
    out
}

/// A fixed workload of `n` steps from `seed`, padded so that 1 KiB
/// segments rotate every few commits ([`workload::seeded`]).
pub fn workload(n: usize, seed: u64) -> Vec<Step> {
    workload::seeded(n, seed)
}

/// A reference that keeps every commit record, so that it can give the
/// state at any seq (point-in-time restore tests).
pub struct History {
    pub ns: Namespace,
    pub records: Vec<iwdb_engine::CommitRecord>,
}

impl Default for History {
    fn default() -> Self {
        History { ns: reference(), records: Vec::new() }
    }
}

impl History {
    /// Run `steps` against the store and the reference: the same outcome
    /// for each, as [`run`] checks.
    pub fn run<F: LogFs + Clone + Send + Sync + 'static>(&mut self, store: &Store<F>, steps: &[Step])
    where
        F::File: Send,
    {
        for step in steps {
            let (outcome, prepared) = match step {
                Step::Tx(mutations) => (store.commit(mutations), self.ns.prepare(mutations)),
                Step::Catalog(change) => {
                    (store.commit_catalog(change.clone()), self.ns.prepare_catalog(change.clone()))
                }
            };
            match (outcome, prepared) {
                (Ok(a), Ok(prepared)) => {
                    self.records.push(prepared.record().clone());
                    assert_eq!(a, self.ns.apply(prepared).unwrap());
                }
                (Err(Error::Engine(a)), Err(b)) => assert_eq!(a, b),
                (outcome, expected) => {
                    panic!("store {:?}, reference {:?}", outcome, expected.map(|p| p.result().clone()))
                }
            }
        }
    }

    pub fn seq(&self) -> u64 {
        self.ns.seq()
    }

    /// The state after the commits `1 ..= seq`.
    pub fn state_at(&self, seq: u64) -> State {
        let mut ns = reference();
        for record in &self.records[..seq as usize] {
            ns.replay(record.clone()).unwrap();
        }
        state(&ns)
    }
}
