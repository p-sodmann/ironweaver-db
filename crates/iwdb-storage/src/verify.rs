//! `verify`: check every file of a data directory or a backup (and, in
//! `archive`, a WAL archive) without changing anything, like SQLite's
//! `PRAGMA integrity_check` (`documentation/adr/0011-verify.md`).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::{Namespace, invariants};

use crate::backup::{self, Manifest};
use crate::checkpoint::{list_checkpoints, load_checkpoint};
use crate::history::HistoryId;
use crate::layout::{self, BACKUP_NAME, CHECKPOINT_DIR, LOCK_NAME, MARKER_NAME, NsPaths, TEMP_SUFFIX, WAL_DIR};
use crate::namespaces::{DEFAULT_ID, DEFAULT_NAME, NAMESPACES_NAME, NS_DIR, parse_ns_dir_name, read_log};
use crate::{Error, WalReader, format, reader};
use iwdb_engine::CommitTime;

/// Something verify found, with the file it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl Finding {
    fn new(path: Option<&Path>, message: impl Into<String>) -> Self {
        Finding { path: path.map(Path::to_path_buf), message: message.into() }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// What a verified directory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A store's data directory.
    DataDir,
    /// A backup (a data directory with a `BACKUP` manifest).
    Backup,
    /// A WAL archive.
    Archive,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::DataDir => "data directory",
            Kind::Backup => "backup",
            Kind::Archive => "WAL archive",
        })
    }
}

/// What verify checked and found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    pub path: PathBuf,
    pub kind: Kind,
    /// The layout version of a data directory or backup, the format
    /// version of an archive (`None` if its marker is damaged).
    pub version: Option<u32>,
    /// The history id (`None` for layout 1, or a damaged marker).
    pub history: Option<HistoryId>,
    /// **Damage**: anything that isn't what the database writes, or that a
    /// store couldn't recover from. Empty if the directory is intact.
    pub problems: Vec<Finding>,
    /// What a crash or an interrupted cleanup leaves and recovery handles:
    /// a torn tail (reported, not cut), temporary files, WAL segments
    /// that a checkpoint already covers. Not damage.
    pub notes: Vec<Finding>,
    /// Checkpoints found, and how many of them loaded and were checked.
    pub checkpoints: usize,
    pub checkpoints_checked: usize,
    /// WAL segments found, and the records read from them (each checked).
    pub segments: usize,
    pub records: u64,
    /// The seqs of the first and last record in the WAL (or archive).
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    /// The seq the directory recovers to: the end of the WAL replayed onto
    /// the oldest checkpoint that loads (`None` for an archive, or if the
    /// WAL couldn't be replayed). For a backup, the manifest's seq.
    pub seq: Option<u64>,
    /// The commit time of the last record read (WAL format 2).
    pub time: Option<CommitTime>,
    /// Each namespace (layout 4 and later, and archives of format 2; one
    /// entry for older ones), by id. The counts above are the sums over
    /// them, and `seq`, `first_seq`, `last_seq` and `time` are those of
    /// the one namespace when there is exactly one, otherwise `None`.
    pub namespaces: Vec<NamespaceVerify>,
}

/// What `verify` found in one namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceVerify {
    pub id: u64,
    pub name: String,
    pub checkpoints: usize,
    pub checkpoints_checked: usize,
    pub segments: usize,
    pub records: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    /// The seq the state reaches (the replay's, or a backup's manifest's).
    pub seq: Option<u64>,
    pub time: Option<CommitTime>,
}

impl VerifyReport {
    pub(crate) fn new(path: &Path, kind: Kind) -> Self {
        VerifyReport {
            path: path.to_path_buf(),
            kind,
            version: None,
            history: None,
            problems: Vec::new(),
            notes: Vec::new(),
            checkpoints: 0,
            checkpoints_checked: 0,
            segments: 0,
            records: 0,
            first_seq: None,
            last_seq: None,
            seq: None,
            time: None,
            namespaces: Vec::new(),
        }
    }

    /// Add a namespace's findings: its problems and notes, prefixed with
    /// its name if `prefix`, and its counts.
    pub(crate) fn merge(&mut self, id: u64, name: &str, prefix: bool, sub: VerifyReport) {
        let tag = |f: Finding| {
            if prefix { Finding { path: f.path, message: format!("namespace '{}': {}", name, f.message) } } else { f }
        };
        self.problems.extend(sub.problems.into_iter().map(tag));
        self.notes.extend(sub.notes.into_iter().map(tag));
        self.checkpoints += sub.checkpoints;
        self.checkpoints_checked += sub.checkpoints_checked;
        self.segments += sub.segments;
        self.records += sub.records;
        self.namespaces.push(NamespaceVerify {
            id,
            name: name.to_owned(),
            checkpoints: sub.checkpoints,
            checkpoints_checked: sub.checkpoints_checked,
            segments: sub.segments,
            records: sub.records,
            first_seq: sub.first_seq,
            last_seq: sub.last_seq,
            seq: sub.seq,
            time: sub.time,
        });
    }

    /// Set the single-namespace fields (see [`namespaces`](Self::namespaces)).
    pub(crate) fn summarize(&mut self) {
        if let [only] = self.namespaces.as_slice() {
            self.first_seq = only.first_seq;
            self.last_seq = only.last_seq;
            self.seq = only.seq;
            self.time = only.time;
        } else {
            self.first_seq = None;
            self.last_seq = None;
            self.seq = None;
            self.time = None;
        }
    }

    /// No damage found (notes may remain).
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }

    pub(crate) fn problem(&mut self, path: Option<&Path>, message: impl Into<String>) {
        self.problems.push(Finding::new(path, message));
    }

    pub(crate) fn note(&mut self, path: Option<&Path>, message: impl Into<String>) {
        self.notes.push(Finding::new(path, message));
    }
}

/// Verify the data directory or backup `root`, holding namespace `name`,
/// and change nothing. Like [`recover`](crate::recover), but every file is
/// read and every check runs, and nothing is cut or removed.
///
/// - **The marker**: magic, layout version and checksum.
/// - **Every WAL segment**, from the first: header, every frame's checksum,
///   seq and contents; consecutive segments follow each other. A torn
///   tail in the last segment is a note (recovery cuts it), anything else
///   a problem.
/// - **Every checkpoint** loads (checksum, format, a seq equal to its name,
///   the namespace), its saved indexes match its catalog (no
///   `IndexChanges`), and its state keeps the invariants
///   ([`invariants::check`]).
/// - **The WAL reaches every checkpoint**: from each checkpoint's seq + 1
///   to at least the newest checkpoint's seq, without a gap, as recovery
///   needs if it falls back to it.
/// - **Replay**: the WAL replayed onto the oldest checkpoint that loads
///   gives, at each newer checkpoint's seq, that checkpoint's state
///   ([`invariants::compare`]), and the state at its end keeps the
///   invariants.
/// - **A backup's manifest** (`BACKUP`): its checksum, its history equal to
///   the marker's, every file it lists present with its length and
///   CRC32C, no other file, and a WAL that ends at its seq.
///
/// Tolerated, as notes: a torn tail, temporary files, and the extra
/// checkpoints and segments an interrupted checkpoint cleanup leaves.
///
/// **Locking**: verify takes a shared lock on `LOCK` (if the file exists;
/// it never creates it), so it fails with [`Error::Locked`] while a store
/// has the directory open, and a store can't open it while verify runs.
///
/// Errors (rather than a report): [`Error::Locked`]; [`Error::NotADataDir`]
/// (no marker, or not ours); [`Error::UnsupportedLayout`];
/// [`Error::Io`] for a directory that can't be listed. Unreadable files
/// are problems in the report.
///
/// Memory: the replayed namespace plus one checkpoint at a time. Time:
/// every file is read once, and every checkpoint is loaded once.
pub fn verify(root: &Path) -> Result<VerifyReport, Error> {
    if !root.is_dir() {
        return Err(Error::NotADataDir { path: root.to_path_buf(), reason: "it is not a directory".into() });
    }
    let _lock = layout::lock_shared(root)?;
    let is_backup = root.join(BACKUP_NAME).exists();
    let mut report = VerifyReport::new(root, if is_backup { Kind::Backup } else { Kind::DataDir });
    let layout4 = match layout::read_marker(root) {
        Ok(Some(info)) => {
            report.version = Some(info.version);
            report.history = info.history;
            info.version >= 4
        }
        Ok(None) => {
            let reason = if root.join(layout::RESTORING_NAME).exists() {
                "it has no marker file: an interrupted restore".to_owned()
            } else {
                "it has no marker file (an interrupted initialization, backup or restore?)".to_owned()
            };
            return Err(Error::NotADataDir { path: root.to_path_buf(), reason });
        }
        Err(Error::InvalidDataDir { reason, .. }) => {
            report.problem(Some(&root.join(MARKER_NAME)), reason);
            // A damaged marker: the layout is a guess, from what is there
            root.join(NAMESPACES_NAME).exists()
        }
        Err(e) => return Err(e),
    };
    if root.join(layout::RESTORING_NAME).exists() {
        report.problem(Some(&root.join(layout::RESTORING_NAME)), "an interrupted restore");
    }

    let manifest = if is_backup { read_manifest(&mut report, root) } else { None };
    // The namespaces to check: layout 4 lists them in the namespace log,
    // older layouts have the one in the directory itself
    let mut namespaces: Vec<(u64, NamespaceName, NsPaths)> = Vec::new();
    if layout4 {
        match read_namespace_log(&mut report, root, manifest.as_ref()) {
            Some(list) => namespaces = list,
            None => {
                report.summarize();
                return Ok(report);
            }
        }
    } else {
        let name = NamespaceName::new(DEFAULT_NAME).map_err(iwdb_engine::Error::from)?;
        namespaces.push((DEFAULT_ID, name, layout::legacy_paths_at(root)));
    }
    list_root_files(&mut report, root, layout4)?;

    for (id, name, paths) in &namespaces {
        let mut sub = VerifyReport::new(root, report.kind);
        let found = verify_namespace(&mut sub, name, paths)?;
        if let (Some(manifest), true) = (&manifest, found) {
            check_namespace_backup(&mut sub, manifest, *id, paths);
        }
        report.merge(*id, name.as_str(), layout4, sub);
    }
    if let Some(manifest) = manifest {
        check_backup(&mut report, root, &manifest);
    }
    report.summarize();
    Ok(report)
}

/// Read the namespace log of a layout 4 directory or backup: its torn tail
/// is a note, damage a problem. Returns the namespaces it lists (for a
/// backup, those of the manifest), `None` if the log is unusable.
fn read_namespace_log(
    report: &mut VerifyReport,
    root: &Path,
    manifest: Option<&Manifest>,
) -> Option<Vec<(u64, NamespaceName, NsPaths)>> {
    let path = root.join(NAMESPACES_NAME);
    if !root.join(NS_DIR).is_dir() {
        report.problem(Some(&root.join(NS_DIR)), "the directory is missing");
    }
    let (parsed, table) = match read_log(&path) {
        Ok(read) => read,
        Err(Error::InvalidNamespaceLog { reason, .. }) => {
            report.problem(Some(&path), reason);
            return None;
        }
        Err(Error::Io { source, .. }) => {
            report.problem(Some(&path), format!("can't be read: {}", source));
            return None;
        }
        Err(_) => return None,
    };
    if let Some(reason) = &parsed.torn {
        report.note(
            Some(&path),
            format!(
                "a torn tail at offset {} of {} bytes ({}): an event that was never acknowledged, which the next open cuts",
                parsed.valid_len, parsed.file_len, reason
            ),
        );
    }
    let live: Vec<_> = table.live().cloned().collect();
    if let Some(manifest) = manifest {
        // A backup holds the namespaces its manifest lists, which are the
        // log's live ones at that moment
        for ns in &live {
            if manifest.namespace(ns.id).is_none() {
                report.problem(
                    Some(&path),
                    format!("lists namespace {} ('{}'), which the manifest doesn't", ns.id, ns.name),
                );
            }
        }
        for ns in &manifest.namespaces {
            if table.get_id(ns.id).is_none_or(|n| n.name != ns.name) {
                report
                    .problem(Some(&path), format!("doesn't list namespace {} ('{}') of the manifest", ns.id, ns.name));
            }
        }
    } else if let Ok(entries) = fs::read_dir(root.join(NS_DIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            match parse_ns_dir_name(&name) {
                Some(id) if table.get_id(id).is_none() => {
                    let dropped =
                        table.events().iter().any(|e| e.id == id && e.kind == crate::namespaces::EventKind::Drop);
                    let data = !dropped && layout::has_data_pub(&NsPaths::new(root, id)).unwrap_or(true);
                    if data {
                        report.problem(
                            Some(&entry.path()),
                            format!(
                                "holds data, but the namespace log doesn't list namespace {} (its create event may be lost: damage to the last event of the log looks like a torn tail); a store refuses to open it",
                                id
                            ),
                        );
                    } else {
                        report.note(
                            Some(&entry.path()),
                            "a namespace directory the log doesn't list (an interrupted create or drop; the next open removes it)",
                        );
                    }
                }
                Some(_) => {}
                None if name.ends_with(TEMP_SUFFIX) => report
                    .note(Some(&entry.path()), "a temporary file (an interrupted write; the next open removes it)"),
                None => report.note(Some(&entry.path()), "not a file of the database (ignored)"),
            }
        }
    }
    Some(
        live.into_iter()
            .map(|ns| {
                let paths = NsPaths::new(root, ns.id);
                (ns.id, ns.name, paths)
            })
            .collect(),
    )
}

/// Notes about files in the root that aren't ours.
fn list_root_files(report: &mut VerifyReport, root: &Path, layout4: bool) -> Result<(), Error> {
    let known: &[&str] = if layout4 {
        &[MARKER_NAME, LOCK_NAME, NAMESPACES_NAME, NS_DIR, BACKUP_NAME]
    } else {
        &[MARKER_NAME, LOCK_NAME, CHECKPOINT_DIR, WAL_DIR, BACKUP_NAME]
    };
    for entry in fs::read_dir(root).map_err(|e| Error::io("list", root, e))? {
        let entry = entry.map_err(|e| Error::io("list", root, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(TEMP_SUFFIX) {
            report.note(Some(&entry.path()), "a temporary file (an interrupted write; the next open removes it)");
        } else if !known.contains(&name.as_str()) && name != layout::RESTORING_NAME {
            report.note(Some(&entry.path()), "not a file of the database (ignored)");
        }
    }
    Ok(())
}

/// Check one namespace's directories, files, coverage and replay into
/// `report`. Returns whether its directories exist.
fn verify_namespace(report: &mut VerifyReport, name: &NamespaceName, paths: &NsPaths) -> Result<bool, Error> {
    let mut present = true;
    for dir in [&paths.checkpoints, &paths.wal] {
        if !dir.is_dir() {
            report.problem(Some(dir), "the directory is missing");
            present = false;
        }
    }
    list_other_files(report, paths)?;
    let checkpoints = if paths.checkpoints.is_dir() { list_checkpoints(&paths.checkpoints)? } else { Vec::new() };
    let segments = if paths.wal.is_dir() { reader::list_segments(&paths.wal)? } else { Vec::new() };
    report.checkpoints = checkpoints.len();
    report.segments = segments.len();

    check_coverage(report, &checkpoints, &segments);
    replay(report, name, &checkpoints, &segments);
    Ok(present)
}

fn list_other_files(report: &mut VerifyReport, paths: &NsPaths) -> Result<(), Error> {
    for dir in [&paths.checkpoints, &paths.wal] {
        if !dir.is_dir() {
            continue;
        }
        let is_checkpoints = dir == &paths.checkpoints;
        for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
            let entry = entry.map_err(|e| Error::io("list", dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let ours = (is_checkpoints && crate::checkpoint::parse_checkpoint_name(&name).is_some() && path.is_file())
                || (!is_checkpoints && format::parse_segment_name(&name).is_some() && path.is_file());
            if name.ends_with(TEMP_SUFFIX) {
                report.note(Some(&path), "a temporary file (an interrupted write; the next open removes it)");
            } else if !ours {
                report.note(Some(&path), "not a file of the database (ignored)");
            }
        }
    }
    Ok(())
}

fn check_coverage(report: &mut VerifyReport, checkpoints: &[(u64, PathBuf)], segments: &[(u64, PathBuf)]) {
    let Some(&(newest, _)) = checkpoints.last() else { return };
    let first = segments.first().map(|(seq, _)| *seq);
    for (seq, path) in checkpoints {
        if *seq == newest {
            continue;
        }
        match first {
            Some(first) if first <= seq + 1 => {}
            _ => report.problem(
                Some(path),
                format!(
                    "the WAL doesn't reach back to seq {} (it starts at {}), so recovery can't fall back to this checkpoint",
                    seq + 1,
                    first.map_or("nothing".to_owned(), |f| f.to_string())
                ),
            ),
        }
    }
    let covered = |cutoff: u64| segments.windows(2).filter(|w| w[1].0 <= cutoff + 1).count();
    if let Some(&(oldest, _)) = checkpoints.first() {
        let extra = covered(oldest);
        if extra > 0 {
            report.note(
                None,
                format!(
                    "{} WAL segments hold only records at or below the oldest checkpoint ({}): an interrupted cleanup, which the next checkpoint finishes",
                    extra, oldest
                ),
            );
        }
    }
}

/// Read the WAL from its first segment; replay it onto the oldest
/// checkpoint that loads, checking each newer checkpoint against the
/// replayed state when the replay reaches its seq.
fn replay(
    report: &mut VerifyReport,
    name: &NamespaceName,
    checkpoints: &[(u64, PathBuf)],
    segments: &[(u64, PathBuf)],
) {
    let mut pending = checkpoints.iter().peekable();
    // The base: the oldest checkpoint that loads (each one before it is a problem)
    let mut namespace = None;
    for (seq, path) in pending.by_ref() {
        if let Some(loaded) = load_and_check(report, name, *seq, path) {
            namespace = Some(loaded);
            break;
        }
    }
    let mut namespace = namespace.unwrap_or_else(|| Namespace::new(name.clone()));
    let base = namespace.seq();
    let mut replaying = true;
    if let Some(&(first_seq, _)) = segments.first() {
        let mut reader = match WalReader::from_segments(segments.to_vec(), first_seq, u64::MAX) {
            Ok(reader) => reader,
            Err(e) => {
                report.problem(None, e.to_string());
                return;
            }
        };
        while let Some(record) = reader.next_timed() {
            let (record, time) = match record {
                Ok(record) => record,
                Err(e) => {
                    report.problem(None, format!("the WAL can't be read further: {}", e));
                    replaying = false;
                    break;
                }
            };
            let seq = record.seq;
            report.records += 1;
            report.first_seq.get_or_insert(seq);
            report.last_seq = Some(seq);
            report.time = time.or(report.time);
            if !replaying || seq <= namespace.seq() {
                continue;
            }
            if seq != namespace.seq() + 1 {
                report.problem(
                    None,
                    format!(
                        "the WAL starts at seq {}, but the state to replay it onto is at {}: records are missing",
                        seq,
                        namespace.seq()
                    ),
                );
                replaying = false;
                continue;
            }
            if let Err(e) = namespace.replay(record, time) {
                report.problem(None, format!("record {} fails to replay: {}", seq, e));
                replaying = false;
                continue;
            }
            // A newer checkpoint at this seq must equal the replayed state
            while let Some((ckpt, path)) = pending.peek() {
                if *ckpt > seq {
                    break;
                }
                if *ckpt == seq
                    && let Some(loaded) = load_and_check(report, name, *ckpt, path)
                    && let Err(difference) = invariants::compare(&loaded, &namespace)
                {
                    report.problem(Some(path), format!("differs from the WAL replayed to seq {}: {}", seq, difference));
                }
                pending.next();
            }
        }
        // Recovery reads the WAL from the base's seq + 1: it must reach it
        if let Some(end) = reader.end().filter(|end| end.next_seq < base + 1) {
            report.problem(
                None,
                format!(
                    "the WAL ends at seq {}, before the checkpoint at {} (recovery refuses: LogEndsBefore)",
                    end.next_seq.saturating_sub(1),
                    base
                ),
            );
            replaying = false;
        }
        if let Some(torn) = reader.end().and_then(|end| end.last_segment.clone())
            && let Some(tail) = torn.torn
        {
            report.note(
                    Some(&torn.path),
                    format!(
                        "a torn tail at offset {} of {} bytes ({}; {} later frames): the end of the log after a crash, which the next open cuts",
                        torn.valid_len, torn.file_len, tail.damage, tail.discarded_frames
                    ),
                );
        }
    }
    // Checkpoints the replay didn't reach
    for (seq, path) in pending {
        load_and_check(report, name, *seq, path);
        if replaying {
            report.problem(
                Some(path),
                format!(
                    "the WAL ends at seq {}, before this checkpoint's seq (recovery refuses: LogEndsBefore)",
                    namespace.seq()
                ),
            );
        }
    }
    if replaying {
        let found = invariants::check(&namespace);
        for violation in found {
            report.problem(None, format!("the state at seq {}: {}", namespace.seq(), violation));
        }
        report.seq = Some(namespace.seq());
    }
}

/// Load a checkpoint and check it on its own: loads, no index changes, the
/// invariants. Problems go into the report; returns the namespace if it
/// loaded.
fn load_and_check(report: &mut VerifyReport, name: &NamespaceName, seq: u64, path: &Path) -> Option<Namespace> {
    let loaded = match load_checkpoint(path, seq, name) {
        Ok(loaded) => loaded,
        Err(e) => {
            let reason = match e {
                Error::InvalidCheckpoint { reason, .. } => reason,
                other => other.to_string(),
            };
            report.problem(Some(path), format!("can't be loaded: {}", reason));
            return None;
        }
    };
    report.checkpoints_checked += 1;
    let changes = &loaded.index_changes;
    if !changes.created.is_empty() || !changes.dropped.is_empty() {
        report.problem(Some(path), format!("its saved indexes differ from its catalog: {:?}", changes));
    }
    let namespace = Namespace::from_loaded(loaded);
    for violation in invariants::check(&namespace) {
        report.problem(Some(path), violation);
    }
    Some(namespace)
}

fn read_manifest(report: &mut VerifyReport, root: &Path) -> Option<Manifest> {
    let path = root.join(BACKUP_NAME);
    match backup::read_manifest(&path) {
        Ok(manifest) => Some(manifest),
        Err(e) => {
            report.problem(Some(&path), e.to_string());
            None
        }
    }
}

/// A backup's namespace against its manifest: the WAL must end at the
/// seq the manifest says.
fn check_namespace_backup(report: &mut VerifyReport, manifest: &Manifest, id: u64, paths: &NsPaths) {
    let Some(ns) = manifest.namespace(id) else { return };
    let path = paths.dir.clone();
    match report.seq {
        Some(seq) if seq != ns.seq => report
            .problem(Some(&path), format!("the backup's WAL ends at seq {}, but its manifest says {}", seq, ns.seq)),
        _ => {}
    }
    report.seq = Some(ns.seq);
}

/// A backup's files and history against its manifest.
fn check_backup(report: &mut VerifyReport, root: &Path, manifest: &Manifest) {
    let path = root.join(BACKUP_NAME);
    if report.history.is_some() && Some(manifest.history) != report.history {
        report.problem(
            Some(&path),
            format!("its history {} differs from the marker's {:?}", manifest.history, report.history),
        );
    }
    for problem in backup::check_files(root, manifest) {
        report.problems.push(problem);
    }
}
