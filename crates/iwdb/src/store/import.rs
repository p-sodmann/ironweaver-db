//! Import and export (ADR 0033): a namespace created from a file as one
//! checkpoint, and a namespace's graph written as a core file.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;

use iwdb_engine::catalog::NamespaceName;
use iwdb_storage::Error;
use iwdb_storage::import as files;
use iwdb_storage::io::LogFs;
use iwdb_storage::layout::create_ns_dir;
use iwdb_storage::namespaces::{EventKind, Plan};

use super::{Ns, Store, background::or_abort, lock};
use crate::import::{
    CountingWriter, ExportFormat, ExportReport, ImportFormat, ImportReport, OnProgress, Phase, Reporter, invalid,
    read_graph,
};

/// At most this many invariant violations are named in an `InvalidImport`.
const MAX_LISTED: usize = 10;

impl<F: LogFs + Clone + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// Create the namespace `name` from a file in `format` read from
    /// `input`: its graph is the namespace's state at seq 1 (its first
    /// commit is 2), written as one checkpoint, with no WAL record.
    ///
    /// What a file holds and how it maps is in ADR 0033: core JSON and
    /// binary files keep their ids, labels, types, attributes, meta and
    /// indexes; LGF files ([`lgf`](crate::import::lgf)) give nodes and
    /// edges with attributes. Every node and edge is at version 1. What a
    /// namespace has no place for (graph meta, LGF `@attributes`) is left
    /// out and listed in [`ImportReport::dropped`]. The graph is checked
    /// like a recovered namespace before anything is written.
    ///
    /// **Crash safety**: all or nothing. The checkpoint is written to a
    /// temporary file without holding any lock; then, holding the
    /// namespace log, it is moved into the new namespace's directory as a
    /// staged import, and the create event is logged and fsynced (the
    /// commit point); then it becomes the namespace's checkpoint. After a
    /// crash the namespace is there with all its data (the next open
    /// finishes the import) or not at all.
    ///
    /// **Not in the WAL**: a restore from a backup taken before the import
    /// and a WAL archive can't rebuild the namespace; take a backup after
    /// an import. The change stream of the namespace starts at seq 2.
    ///
    /// Memory: the graph, plus the whole file for JSON (binary and LGF
    /// files are read as they are decoded). `progress` gets the bytes read,
    /// then the bytes of the checkpoint written.
    ///
    /// Errors: [`Error::NamespaceExists`]; an invalid name ([`Error::Engine`]);
    /// [`Error::InvalidImport`] (nothing is created); [`Error::Io`] reading
    /// `input` or writing; [`Error::ReadOnly`] (the namespace log failed).
    pub fn import_namespace(
        &self,
        name: &str,
        format: ImportFormat,
        input: impl Read,
        progress: Option<OnProgress<'_>>,
    ) -> Result<ImportReport, Error> {
        self.import_from(name, format, input, progress, Path::new("<input>"))
    }

    /// [`import_namespace`](Self::import_namespace) from the file `path`,
    /// in `format`, or the format [`ImportFormat::detect`] finds in its
    /// first bytes (an [`Error::InvalidImport`] if it finds none).
    pub fn import_file(
        &self,
        name: &str,
        path: &Path,
        format: Option<ImportFormat>,
        progress: Option<OnProgress<'_>>,
    ) -> Result<ImportReport, Error> {
        let file = File::open(path).map_err(|e| Error::Io { op: "open", path: path.into(), source: e })?;
        let mut input = BufReader::with_capacity(1 << 20, file);
        let format = match format {
            Some(format) => format,
            None => {
                let head = input.fill_buf().map_err(|e| Error::Io { op: "read", path: path.into(), source: e })?;
                ImportFormat::detect(head).ok_or_else(|| {
                    invalid(format!("can't tell the format of '{}' (json, binary or lgf): name it", path.display()))
                })?
            }
        };
        self.import_from(name, format, input, progress, path)
    }

    fn import_from(
        &self,
        name: &str,
        format: ImportFormat,
        input: impl Read,
        progress: Option<OnProgress<'_>>,
        what: &Path,
    ) -> Result<ImportReport, Error> {
        let name = NamespaceName::new(name).map_err(iwdb_engine::Error::from)?;
        if lock(&self.shared.catalog).log.table().get(&name).is_some() {
            return Err(Error::NamespaceExists { name: name.to_string() });
        }
        let mut reporter = Reporter::new(progress, Phase::Reading);
        let read = read_graph(format, input, &mut reporter, what)?;
        reporter.end(read.bytes, Phase::Writing);
        let namespace = iwdb_engine::plain::imported(name.clone(), read.graph).map_err(invalid)?;
        let problems = iwdb_engine::invariants::check(&namespace);
        if !problems.is_empty() {
            let listed: Vec<&str> = problems.iter().take(MAX_LISTED).map(String::as_str).collect();
            let more = problems.len().saturating_sub(MAX_LISTED);
            let more = if more > 0 { format!(" (and {} more)", more) } else { String::new() };
            return Err(invalid(format!("{}{}", listed.join("; "), more)));
        }
        let shared = &self.shared;
        let mut written = 0;
        let staged =
            files::stage(&shared.fs, &shared.root.join(iwdb_storage::namespaces::NS_DIR), &namespace, &mut |n| {
                written = n;
                reporter.update(n);
            })?;
        reporter.end(written, Phase::Writing);
        let (nodes, edges) = (namespace.graph().node_count(), namespace.graph().edge_count());
        let indexes = namespace.catalog().indexes().map(|i| i.path.clone()).collect();
        drop(namespace);

        let mut catalog = lock(&shared.catalog);
        let created = or_abort("importing a namespace", || {
            let created = (|| {
                if let Some(cause) = catalog.log.failure() {
                    return Err(Error::ReadOnly { cause: cause.to_owned() });
                }
                let id = match catalog.log.table().plan(EventKind::Create, &name, None)? {
                    Plan::New { id } => id,
                    // Only with a key
                    Plan::Duplicate(_) => return Err(Error::NamespaceExists { name: name.to_string() }),
                };
                let paths = create_ns_dir(&shared.fs, &shared.root, id)?;
                files::place(&shared.fs, &staged, &paths)?;
                Ok(paths)
            })();
            let paths = match created {
                Ok(paths) => paths,
                Err(e) => {
                    // Not placed: the file goes now, or at the next open
                    let _ = shared.fs.remove_file(&staged);
                    return Err(e);
                }
            };
            let id = paths.id;
            let event = catalog.log.append(EventKind::Create, id, &name, None)?;
            // Opening reads the namespace, which finishes the import first
            self.open_created(&mut catalog, &name, paths, event)
        })?;
        self.sync_archive_log(&catalog);
        drop(catalog);
        log::info!(
            "{}: imported namespace '{}' ({} nodes, {} edges) from {} ({})",
            shared.root.display(),
            name,
            nodes,
            edges,
            what.display(),
            format
        );
        Ok(ImportReport {
            event: created.event,
            format,
            seq: files::IMPORT_SEQ,
            nodes,
            edges,
            indexes,
            dropped: read.dropped,
            bytes_read: read.bytes,
            checkpoint_bytes: written,
        })
    }
}

impl<F: LogFs + Clone + Send + Sync + 'static> Ns<'_, F>
where
    F::File: Send,
{
    /// Write the namespace's graph to `out` as a core file in `format`: the
    /// state at the namespace's current seq, with nodes, edges (with their
    /// ids), labels, types, attributes, user meta and indexes, sorted like
    /// a checkpoint. Versions, constraints, idempotency keys and marks are
    /// not in it. The Ironweaver library and
    /// [`Store::import_namespace`] read it.
    ///
    /// Holds the namespace's read lock while it writes: commits to the
    /// namespace wait (reads don't). A JSON export is built in memory
    /// first; a binary one streams. `progress` gets the bytes written.
    ///
    /// Errors: [`Error::Io`] writing (on the path `<output>`);
    /// [`Error::NamespaceDropped`].
    pub fn export(
        &self,
        out: impl Write,
        format: ExportFormat,
        progress: Option<OnProgress<'_>>,
    ) -> Result<ExportReport, Error> {
        self.export_to(out, format, progress, Path::new("<output>"))
    }

    /// [`export`](Self::export) into the file `path`, written atomically: a
    /// temporary file, fsynced, then renamed over `path`. The format is
    /// `format`, or [`ExportFormat::from_path`].
    pub fn export_file(
        &self,
        path: &Path,
        format: Option<ExportFormat>,
        progress: Option<OnProgress<'_>>,
    ) -> Result<ExportReport, Error> {
        let format = format.unwrap_or_else(|| ExportFormat::from_path(path));
        let mut result = None;
        ironweaver_core::format::write_atomic(path, |out| {
            let r = self.export_to(out, format, progress, path);
            let failed = r.is_err();
            result = Some(r);
            if failed { Err(io::Error::other("the export failed")) } else { Ok(()) }
        })
        .or_else(|e| match &result {
            // The export's own error is the one to report
            Some(Err(_)) => Ok(()),
            _ => Err(Error::Io { op: "write", path: path.into(), source: e }),
        })?;
        result.unwrap_or_else(|| {
            Err(Error::Io { op: "write", path: path.into(), source: io::Error::other("not written") })
        })
    }

    fn export_to(
        &self,
        out: impl Write,
        format: ExportFormat,
        progress: Option<OnProgress<'_>>,
        what: &Path,
    ) -> Result<ExportReport, Error> {
        if self.state.live.is_dropped() {
            return Err(Error::NamespaceDropped { name: self.name().to_owned() });
        }
        let mut reporter = Reporter::new(progress, Phase::Writing);
        let io_error = |source: io::Error| Error::Io { op: "write", path: what.into(), source };
        let (seq, nodes, edges, bytes) = self.state.live.read(|ns| {
            let graph = ns.graph();
            let mut out = CountingWriter { inner: out, bytes: 0, reporter: &mut reporter };
            match format {
                ExportFormat::Json => {
                    let bytes = iwdb_engine::plain::to_json(graph, false).map_err(iwdb_engine::Error::from)?;
                    out.write_all(&bytes).map_err(io_error)?;
                }
                ExportFormat::Binary => {
                    let mut buffered = io::BufWriter::with_capacity(1 << 20, &mut out);
                    iwdb_engine::plain::write_binary(graph, &mut buffered).map_err(iwdb_engine::Error::from)?;
                    buffered.flush().map_err(io_error)?;
                }
            }
            out.flush().map_err(io_error)?;
            Ok::<_, Error>((ns.seq(), graph.node_count(), graph.edge_count(), out.bytes))
        })?;
        reporter.end(bytes, Phase::Writing);
        Ok(ExportReport { format, seq, nodes, edges, bytes })
    }
}
