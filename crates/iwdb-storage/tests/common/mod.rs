//! Shared helpers for the WAL tests: a [`LogFs`] that counts calls and
//! fails the ones a test asks it to, and small builders.

// Each test binary uses a different subset.
#![allow(dead_code)]

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use ironweaver_core::{Attrs, Value};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::testutil::canonical;
use iwdb_engine::{CommitRecord, Mutation, Namespace};
use iwdb_storage::io::{LogFile, LogFs, StdFs};

/// A file operation of [`LogFs`] / [`LogFile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Create,
    OpenAppend,
    Rename,
    SyncDir,
    Write,
    Sync,
}

/// How the next matching call fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Fail without doing anything.
    Fail,
    /// For writes: write the first half of the bytes, then fail.
    Partial,
}

#[derive(Debug, Default)]
pub struct State {
    /// Every call, in order (failed ones included).
    pub calls: Vec<Call>,
    /// Faults to inject, each once, on the next call of its kind.
    pub faults: Vec<(Call, Fault)>,
    /// Calls that failed.
    pub failed: Vec<Call>,
}

impl State {
    pub fn count(&self, call: Call) -> usize {
        self.calls.iter().filter(|c| **c == call).count()
    }
}

/// The real file system, with every call recorded and injectable faults.
#[derive(Clone, Debug, Default)]
pub struct TestFs(Arc<Mutex<State>>);

impl TestFs {
    pub fn state(&self) -> MutexGuard<'_, State> {
        match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Fail the next call of kind `call`.
    pub fn inject(&self, call: Call, fault: Fault) {
        self.state().faults.push((call, fault));
    }

    pub fn count(&self, call: Call) -> usize {
        self.state().count(call)
    }

    /// Record `call`; the fault to inject, if any.
    fn enter(&self, call: Call) -> Option<Fault> {
        let mut state = self.state();
        state.calls.push(call);
        let at = state.faults.iter().position(|(c, _)| *c == call)?;
        let (_, fault) = state.faults.remove(at);
        state.failed.push(call);
        Some(fault)
    }
}

fn injected(call: Call) -> io::Error {
    io::Error::other(format!("injected {:?} failure", call))
}

impl LogFs for TestFs {
    type File = TestFile;

    fn create(&self, path: &Path) -> io::Result<TestFile> {
        if self.enter(Call::Create).is_some() {
            return Err(injected(Call::Create));
        }
        Ok(TestFile { inner: StdFs.create(path)?, fs: self.clone() })
    }

    fn open_append(&self, path: &Path) -> io::Result<TestFile> {
        if self.enter(Call::OpenAppend).is_some() {
            return Err(injected(Call::OpenAppend));
        }
        Ok(TestFile { inner: StdFs.open_append(path)?, fs: self.clone() })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.enter(Call::Rename).is_some() {
            return Err(injected(Call::Rename));
        }
        StdFs.rename(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        if self.enter(Call::SyncDir).is_some() {
            return Err(injected(Call::SyncDir));
        }
        StdFs.sync_dir(dir)
    }
}

pub struct TestFile {
    inner: File,
    fs: TestFs,
}

impl LogFile for TestFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.fs.enter(Call::Write) {
            None => self.inner.write_all(bytes),
            Some(Fault::Fail) => Err(injected(Call::Write)),
            Some(Fault::Partial) => {
                self.inner.write_all(&bytes[..bytes.len() / 2])?;
                Err(injected(Call::Write))
            }
        }
    }

    fn sync(&mut self) -> io::Result<()> {
        if self.fs.enter(Call::Sync).is_some() {
            return Err(injected(Call::Sync));
        }
        self.inner.sync()
    }
}

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
        ns.replay(record).unwrap();
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
