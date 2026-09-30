//! Helpers for the store tests: options, running a workload against a
//! store and a reference namespace, and reading files.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::{
    CheckpointOptions, Error, FsyncPolicy, LogFs, Namespace, NamespaceCatalog, Store, StoreOptions, WalOptions,
};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::testutil::canonical;
use iwdb_storage::MIN_SEGMENT_SIZE;

use crate::workload::Step;

/// `always`, 1 KiB segments (many rotations), no background threads, keep
/// `keep` checkpoints.
pub fn options(keep: usize) -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync: FsyncPolicy::Always, segment_size: MIN_SEGMENT_SIZE },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: true, keep, background: false },
        create_if_missing: true,
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

/// A record frame as `documentation/formats/wal.md` describes it.
pub fn frame(seq: u64, synced_seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&synced_seq.to_le_bytes());
    out.push(1);
    let crc = crc32c::crc32c_append(crc32c::crc32c(&out), payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A fixed workload: a seed and `n` deterministic steps from the proptest
/// strategies, each followed by a commit that writes a 200-byte string to
/// one of three padding nodes, so that the WAL rotates 1 KiB segments
/// every few commits.
pub fn workload(n: usize, seed: u64) -> Vec<Step> {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &bytes);
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let strategy = (crate::workload::seed(), proptest::collection::vec(crate::workload::step(), n)).prop_map(
        |(seed, mut steps)| {
            steps.insert(0, seed);
            steps
        },
    );
    let steps = strategy.new_tree(&mut runner).unwrap().current();
    let mut padded = Vec::with_capacity(2 * steps.len());
    for (i, step) in steps.into_iter().enumerate() {
        padded.push(step);
        padded.push(pad(i));
    }
    padded
}

/// Upsert padding node `p<i % 3>` with a 200-byte string.
pub fn pad(i: usize) -> Step {
    Step::Tx(vec![iwdb::Mutation::UpsertNode {
        id: format!("p{}", i % 3),
        labels: vec![],
        attr: [("pad".to_owned(), iwdb::Value::from(format!("{:0>200}", i)))].into(),
        meta: Default::default(),
        expected_version: None,
    }])
}
