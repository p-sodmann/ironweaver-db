//! Import and export (ADR 0033): a namespace created from a file as one
//! checkpoint, and a namespace's graph written as a core file.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;

use std::collections::HashMap;

use iwdb_engine::catalog::{AttrPath, IndexDef};
use iwdb_engine::{CatalogChange, EdgeKey, Mutation};
use iwdb_storage::Error;
use iwdb_storage::import as files;
use iwdb_storage::io::LogFs;
use iwdb_storage::layout::create_ns_dir;
use iwdb_storage::namespaces::{EventKind, Plan};

use super::{Ns, Store, background::or_abort, lock};
use crate::import::{
    CountingWriter, ExportFormat, ExportReport, ImportFormat, ImportReport, MergeReport, OnProgress, Phase, Reporter,
    invalid, read_graph,
};

/// At most this many invariant violations are named in an `InvalidImport`.
const MAX_LISTED: usize = 10;

/// Mutations per commit of a merge (halved while a commit's WAL record
/// would be too large).
const MERGE_BATCH: usize = 10_000;

/// The file `path`, buffered, and its format: `format`, or the one
/// [`ImportFormat::detect`] finds in its first bytes.
fn open_detected(path: &Path, format: Option<ImportFormat>) -> Result<(BufReader<File>, ImportFormat), Error> {
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
    Ok((input, format))
}

/// The mutations that merge `graph` into a namespace: an upsert per node
/// (in slot order), then per edge an upsert by its ends and type, or, for
/// edges that share their ends and type with another edge of the file
/// (parallel edges), an add.
fn merge_mutations(graph: &iwdb_engine::DbGraph) -> (Vec<Mutation>, Vec<Mutation>) {
    let nodes = graph
        .nodes()
        .map(|(ix, node)| Mutation::UpsertNode {
            id: node.id().to_owned(),
            labels: graph.label_names(ix).unwrap_or_default().into_iter().map(str::to_owned).collect(),
            attr: node.data.attr.clone(),
            meta: node.data.meta.clone(),
            expected_version: None,
        })
        .collect();
    let key = |ix, e: &ironweaver_core::Edge<iwdb_engine::DbRecord>| {
        (e.source(), e.target(), graph.edge_type_name(ix).map(str::to_owned))
    };
    let mut parallel: HashMap<_, usize> = HashMap::new();
    for (ix, e) in graph.edges() {
        *parallel.entry(key(ix, e)).or_default() += 1;
    }
    let id = |n| graph.node(n).map_or_else(String::new, |n| n.id().to_owned());
    let edges = graph
        .edges()
        .map(|(ix, e)| {
            let (from, to, ty) = key(ix, e);
            let (attr, meta) = (e.data.attr.clone(), e.data.meta.clone());
            if parallel.get(&(from, to, ty.clone())).copied().unwrap_or(0) > 1 {
                Mutation::AddEdge { from: id(from), to: id(to), ty, attr, meta }
            } else {
                let key = EdgeKey::Endpoints { from: id(from), to: id(to), ty };
                Mutation::UpsertEdge { key, attr, meta, expected_version: None }
            }
        })
        .collect();
    (nodes, edges)
}

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
    /// **Not in the WAL, but in the archive**: the change stream of the
    /// namespace starts at seq 2. With a WAL archive, the checkpoint is
    /// copied there before this returns (archive format 3; if that fails,
    /// it warns and the next open copies it), so restore rebuilds the
    /// namespace from the archive alone. Without one, take a backup.
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
        let (input, format) = open_detected(path, format)?;
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
        let _span = iwdb_storage::trace_span!("iwdb.import", iwdb.import.format = format.name()).entered();
        let name = super::public_name(name)?;
        if lock(&self.shared.catalog).log.table().get(&name).is_some() {
            return Err(Error::NamespaceExists { name: name.to_string() });
        }
        // The memory limit (ADR 0054), before the file is read into memory,
        // and again before the namespace is logged
        self.shared.memory.check_write()?;
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
                shared.memory.check_write()?;
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
            // The archive gets the checkpoint (no WAL record holds the
            // import); if that fails, the next open tries again
            if let Some(archive) = &catalog.archive
                && let Err(e) = files::archive_base(archive, id, &paths)
            {
                log::warn!("namespace '{}': the import isn't in the WAL archive yet: {}", name, e);
            }
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
    /// Merge a file in `format` read from `input` into this namespace,
    /// through the commit pipeline: every node of the file is upserted
    /// (its attributes and meta replace the node's, its labels are
    /// added), then every edge is upserted by its ends and type (the
    /// edge's attributes and meta replaced, or a new edge added; parallel
    /// edges of the file, with the same ends and type, are added each
    /// time). Indexes the file declares that the namespace lacks are
    /// created first. The file's edge ids are not kept.
    ///
    /// **Batches, not one transaction**: the mutations are committed in
    /// batches of up to 10 000, nodes before edges, each an ordinary
    /// commit (in the WAL, the change stream and the archive; constraints
    /// checked). A failure stops the merge with the batches before it
    /// committed; running the merge again converges (except for parallel
    /// edges, which are added again).
    ///
    /// Memory: the file's graph and one batch, plus the whole file for
    /// JSON. `progress` gets the bytes read, then the mutations committed.
    ///
    /// Errors: [`Error::InvalidImport`] (the file; nothing committed); a
    /// commit's errors ([`Error::Engine`] for a constraint, an
    /// `AmbiguousEdge` where the namespace has parallel edges an edge of
    /// the file would update, ...); [`Error::Io`].
    pub fn import(
        &self,
        format: ImportFormat,
        input: impl Read,
        progress: Option<OnProgress<'_>>,
    ) -> Result<MergeReport, Error> {
        self.merge_from(format, input, progress, Path::new("<input>"))
    }

    /// [`import`](Self::import) from the file `path`, in `format` or the
    /// one detected from its first bytes.
    pub fn import_file(
        &self,
        path: &Path,
        format: Option<ImportFormat>,
        progress: Option<OnProgress<'_>>,
    ) -> Result<MergeReport, Error> {
        let (input, format) = open_detected(path, format)?;
        self.merge_from(format, input, progress, path)
    }

    fn merge_from(
        &self,
        format: ImportFormat,
        input: impl Read,
        progress: Option<OnProgress<'_>>,
        what: &Path,
    ) -> Result<MergeReport, Error> {
        let _span = iwdb_storage::trace_span!("iwdb.import", iwdb.import.format = format.name()).entered();
        if self.state.live.is_dropped() {
            return Err(Error::NamespaceDropped { name: self.name().to_owned() });
        }
        let mut reporter = Reporter::new(progress, Phase::Reading);
        let read = read_graph(format, input, &mut reporter, what)?;
        reporter.end(read.bytes, Phase::Committing);
        let graph = read.graph;
        // The file's indexes, checked before anything is committed
        let mut wanted = Vec::new();
        for keys in graph.index_paths() {
            let path = AttrPath::new(keys.iter().cloned()).map_err(invalid)?;
            if path.keys() == ["labels"] {
                return Err(invalid("the file declares an index on 'labels', which can't be indexed"));
            }
            wanted.push(IndexDef { path });
        }
        let mut report = MergeReport {
            format,
            nodes: graph.node_count(),
            edges: graph.edge_count(),
            created_indexes: Vec::new(),
            dropped: read.dropped,
            bytes_read: read.bytes,
            commits: 0,
            first_seq: None,
            last_seq: None,
        };
        let committed = |report: &mut MergeReport, seq: u64| {
            report.commits += 1;
            report.first_seq.get_or_insert(seq);
            report.last_seq = Some(seq);
        };
        for index in wanted {
            if self.read(|n| n.catalog().has_index(&index)) {
                continue;
            }
            let result = self.commit_catalog(CatalogChange::CreateIndex(index.clone()))?;
            committed(&mut report, result.seq);
            report.created_indexes.push(index.path);
        }
        let (nodes, edges) = merge_mutations(&graph);
        drop(graph);
        let mut done = 0u64;
        for part in [nodes, edges] {
            let mut rest = &part[..];
            let mut size = MERGE_BATCH;
            while !rest.is_empty() {
                let batch = &rest[..size.min(rest.len())];
                match self.commit(batch) {
                    Ok(result) => {
                        committed(&mut report, result.seq);
                        done += batch.len() as u64;
                        reporter.committed(done);
                        rest = &rest[batch.len()..];
                    }
                    // Too large for one WAL record: smaller batches
                    Err(Error::RecordTooLarge { .. }) if batch.len() > 1 => size = batch.len() / 2,
                    Err(e) => return Err(e),
                }
            }
        }
        log::info!(
            "namespace '{}': merged {} nodes and {} edges from {} ({}) in {} commits",
            self.name(),
            report.nodes,
            report.edges,
            what.display(),
            format,
            report.commits
        );
        Ok(report)
    }

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
    /// temporary file, fsynced, then renamed over `path`, and the directory
    /// fsynced ([`StdFs::write_atomic`](iwdb_storage::io::StdFs)). The format
    /// is `format`, or [`ExportFormat::from_path`].
    pub fn export_file(
        &self,
        path: &Path,
        format: Option<ExportFormat>,
        mut progress: Option<OnProgress<'_>>,
    ) -> Result<ExportReport, Error> {
        let format = format.unwrap_or_else(|| ExportFormat::from_path(path));
        let mut result = None;
        iwdb_storage::io::StdFs
            .write_atomic(path, &mut |out| {
                let r = self.export_to(out, format, progress.take(), path);
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
