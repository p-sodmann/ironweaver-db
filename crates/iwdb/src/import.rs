//! Bulk import and export (step 13, ADR 0033).
//!
//! - [`Store::import_namespace`](crate::Store::import_namespace) creates a
//!   namespace from a file, as one checkpoint and without WAL records, all
//!   or nothing across crashes. Formats: the core's JSON and binary files
//!   ([`iwdb_engine::plain`]) and LGF ([`lgf`]).
//! - [`Ns::export`](crate::Ns::export) writes a namespace's graph as a
//!   core JSON or binary file, which the Ironweaver library and the import
//!   read.
//!
//! Both take an optional progress callback ([`Progress`]).

pub mod lgf;

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::str::FromStr;

use iwdb_engine::DbGraph;
use iwdb_engine::catalog::AttrPath;
use iwdb_storage::Error;
use iwdb_storage::namespaces::Event;

/// The formats an import reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImportFormat {
    /// A core JSON file (version 2, or version 1, which the core migrates).
    Json,
    /// A core binary file (version 2).
    Binary,
    /// LGF, the LEMON Graph Format ([`lgf`]).
    Lgf,
}

impl ImportFormat {
    /// The format of a file starting with `head` (its first few KiB): the
    /// binary magic `IRONWEAV`; `{` (after whitespace) for JSON; for LGF, a
    /// first line that is neither empty nor a comment and starts with `@`.
    /// `None` if it is none of them.
    pub fn detect(head: &[u8]) -> Option<Self> {
        if head.starts_with(b"IRONWEAV") {
            return Some(ImportFormat::Binary);
        }
        if head.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{') {
            return Some(ImportFormat::Json);
        }
        let first = head
            .split(|b| *b == b'\n')
            .map(|line| line.trim_ascii())
            .find(|line| !line.is_empty() && !line.starts_with(b"#"))?;
        first.starts_with(b"@").then_some(ImportFormat::Lgf)
    }

    pub fn name(self) -> &'static str {
        match self {
            ImportFormat::Json => "json",
            ImportFormat::Binary => "binary",
            ImportFormat::Lgf => "lgf",
        }
    }
}

impl fmt::Display for ImportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ImportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "json" => Ok(ImportFormat::Json),
            "binary" => Ok(ImportFormat::Binary),
            "lgf" => Ok(ImportFormat::Lgf),
            other => Err(format!("unknown import format '{}' (json, binary or lgf)", other)),
        }
    }
}

/// The formats an export writes: the core's files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    Json,
    Binary,
}

impl ExportFormat {
    /// `Json` for a path ending in `.json` (any case), `Binary` otherwise.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("json") => ExportFormat::Json,
            _ => ExportFormat::Binary,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::Binary => "binary",
        }
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ExportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "json" => Ok(ExportFormat::Json),
            "binary" => Ok(ExportFormat::Binary),
            other => Err(format!("unknown export format '{}' (json or binary)", other)),
        }
    }
}

/// What an import or export is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Reading the file (import).
    Reading,
    /// Writing the checkpoint (import) or the file (export).
    Writing,
}

/// Progress of an import or export: the bytes read or written so far in
/// the current phase. Reported about every [`PROGRESS_STEP`] bytes, and at
/// the end of each phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    pub bytes: u64,
}

/// How often progress is reported, in bytes.
pub const PROGRESS_STEP: u64 = 4 << 20;

/// A progress callback.
pub type OnProgress<'a> = &'a mut dyn FnMut(Progress);

/// What an import did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    /// The namespace log's create event of the new namespace.
    pub event: Event,
    pub format: ImportFormat,
    /// The namespace's seq: its first commit is `seq + 1`.
    pub seq: u64,
    pub nodes: usize,
    pub edges: usize,
    /// The indexes the file declared, now the namespace's.
    pub indexes: Vec<AttrPath>,
    /// What the file held that a namespace has no place for, and was left
    /// out: graph meta keys of a core file, `@attributes` of an LGF file.
    pub dropped: Vec<String>,
    /// The bytes read, and the size of the checkpoint written.
    pub bytes_read: u64,
    pub checkpoint_bytes: u64,
}

/// What an export did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportReport {
    pub format: ExportFormat,
    /// The namespace's seq whose state was exported.
    pub seq: u64,
    pub nodes: usize,
    pub edges: usize,
    pub bytes: u64,
}

/// Calls a progress callback every [`PROGRESS_STEP`] bytes.
pub(crate) struct Reporter<'a> {
    on: Option<OnProgress<'a>>,
    phase: Phase,
    last: u64,
}

impl<'a> Reporter<'a> {
    pub(crate) fn new(on: Option<OnProgress<'a>>, phase: Phase) -> Self {
        Reporter { on, phase, last: 0 }
    }

    pub(crate) fn update(&mut self, bytes: u64) {
        if bytes >= self.last + PROGRESS_STEP {
            self.report(bytes);
        }
    }

    /// The end of the phase: report `bytes`, and go on with `next`.
    pub(crate) fn end(&mut self, bytes: u64, next: Phase) {
        self.report(bytes);
        self.phase = next;
        self.last = 0;
    }

    fn report(&mut self, bytes: u64) {
        self.last = bytes;
        if let Some(on) = self.on.as_mut() {
            on(Progress { phase: self.phase, bytes });
        }
    }
}

/// A reader that counts its bytes for a [`Reporter`].
pub(crate) struct CountingReader<'r, 'a, R> {
    pub(crate) inner: R,
    pub(crate) bytes: u64,
    pub(crate) reporter: &'r mut Reporter<'a>,
}

impl<R: Read> Read for CountingReader<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        self.reporter.update(self.bytes);
        Ok(n)
    }
}

/// A writer that counts its bytes for a [`Reporter`].
pub(crate) struct CountingWriter<'r, 'a, W> {
    pub(crate) inner: W,
    pub(crate) bytes: u64,
    pub(crate) reporter: &'r mut Reporter<'a>,
}

impl<W: Write> Write for CountingWriter<'_, '_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        self.reporter.update(self.bytes);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn invalid(reason: impl fmt::Display) -> Error {
    Error::InvalidImport { reason: reason.to_string() }
}

/// A file read into a graph: every record at version 1.
pub(crate) struct ReadGraph {
    pub(crate) graph: DbGraph,
    pub(crate) dropped: Vec<String>,
    pub(crate) bytes: u64,
}

/// Read `reader` in `format` into a graph. Every error of the file's
/// content is an [`Error::InvalidImport`]; an error reading is an
/// [`Error::Io`] (on `what`).
pub(crate) fn read_graph(
    format: ImportFormat,
    reader: impl Read,
    reporter: &mut Reporter<'_>,
    what: &Path,
) -> Result<ReadGraph, Error> {
    let mut input = CountingReader { inner: reader, bytes: 0, reporter };
    let (graph, dropped) = match format {
        ImportFormat::Json => {
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes).map_err(|e| Error::Io { op: "read", path: what.into(), source: e })?;
            let plain = iwdb_engine::plain::from_json(&bytes).map_err(invalid)?;
            (plain.graph, plain.dropped_meta.into_iter().map(|k| format!("graph meta '{}'", k)).collect())
        }
        ImportFormat::Binary => {
            let plain = iwdb_engine::plain::from_binary_reader(&mut input).map_err(invalid)?;
            (plain.graph, plain.dropped_meta.into_iter().map(|k| format!("graph meta '{}'", k)).collect())
        }
        ImportFormat::Lgf => {
            let read = lgf::read(io::BufReader::with_capacity(1 << 20, &mut input)).map_err(|e| match e {
                lgf::LgfError::Io(source) => Error::Io { op: "read", path: what.into(), source },
                other => invalid(other),
            })?;
            (read.graph, read.dropped)
        }
    };
    let bytes = input.bytes;
    Ok(ReadGraph { graph, dropped, bytes })
}
