//! Shared builders and fault-injection helpers for storage and facade tests.
//!
//! Available only with `testutil`; normal builds expose neither these helpers
//! nor their engine test utilities. Helpers panic on invalid test fixtures.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

pub use crate::failpoint::{Action, Call, FailFs, Fault, Hook, Rule, When};
use crate::io::StdFs;
use ironweaver_core::{Attrs, Value};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::{CommitRecord, Mutation, Namespace};

/// The real file system with failpoints (every call recorded).
pub type TestFs = FailFs<StdFs>;

/// An empty namespace named `wal` for storage fixtures.
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

pub use iwdb_engine::testutil::state;

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
    crate::list_segments(dir).unwrap().into_iter().map(|(_, p)| p).collect()
}

/// Offsets where the frames of a segment's bytes end, parsed from their
/// length fields (the first is the end of the header).
pub fn frame_ends(bytes: &[u8]) -> Vec<usize> {
    use crate::format::{FRAME_HEADER_LEN, SEGMENT_HEADER_LEN};
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
