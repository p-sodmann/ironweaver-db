//! Shared helpers for the WAL tests: [`TestFs`], the real file system with
//! failpoints (`iwdb_storage::failpoint`), and small builders. The store
//! tests (`crates/iwdb/tests`) include this file with `#[path]`.

// Each test binary uses a different subset.
#![allow(dead_code, unused_imports)]

use std::path::{Path, PathBuf};

use ironweaver_core::{Attrs, Value};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::testutil::canonical;
use iwdb_engine::{CommitRecord, Mutation, Namespace};
pub use iwdb_storage::failpoint::{Action, Call, FailFs, Fault, Hook, Rule, When};
use iwdb_storage::io::StdFs;

/// The real file system with failpoints (every call recorded).
pub type TestFs = FailFs<StdFs>;

pub fn namespace() -> Namespace {
    Namespace::new(NamespaceName::new("wal").unwrap())
}

/// Upsert node `id` with attribute `x`.
pub fn upsert(id: &str, x: Value) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["L".into()],
        attr: [("x".to_owned(), x)].into(),
        meta: Attrs::new(),
        expected_version: None,
    }
}

/// The observable state of a namespace.
pub fn state(ns: &Namespace) -> (Vec<String>, iwdb_engine::catalog::NamespaceCatalog, u64) {
    (canonical(ns.graph()), ns.catalog().clone(), ns.seq())
}

/// Replay `records` onto an empty namespace.
pub fn replay(records: impl IntoIterator<Item = CommitRecord>) -> Namespace {
    let mut ns = namespace();
    for record in records {
        ns.replay(record, None).unwrap();
    }
    ns
}

/// The segment files of `dir`, sorted.
pub fn segments(dir: &Path) -> Vec<PathBuf> {
    iwdb_storage::list_segments(dir).unwrap().into_iter().map(|(_, p)| p).collect()
}

/// Offsets where the frames of a segment's bytes end, parsed from their
/// length fields (the first is the end of the header).
pub fn frame_ends(bytes: &[u8]) -> Vec<usize> {
    use iwdb_storage::format::{FRAME_HEADER_LEN, SEGMENT_HEADER_LEN};
    let mut ends = vec![SEGMENT_HEADER_LEN];
    let mut pos = SEGMENT_HEADER_LEN;
    while pos < bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += FRAME_HEADER_LEN + len;
        ends.push(pos);
    }
    assert_eq!(pos, bytes.len());
    ends
}
