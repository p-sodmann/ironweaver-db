//! `verify`: check every file of a data directory or a backup (and, in
//! `archive`, a WAL archive) without changing anything, like SQLite's
//! `PRAGMA integrity_check` (`documentation/adr/0011-verify.md`).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::{invariants, Namespace};

use crate::backup::{self, Manifest};
use crate::checkpoint::{list_checkpoints, load_checkpoint};
use crate::history::HistoryId;
use crate::layout::{self, BACKUP_NAME, CHECKPOINT_DIR, LOCK_NAME, MARKER_NAME, TEMP_SUFFIX, WAL_DIR};
use crate::{format, reader, Error, WalReader};
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
pub fn verify(root: &Path, name: &NamespaceName) -> Result<VerifyReport, Error> {
    if !root.is_dir() {
        return Err(Error::NotADataDir { path: root.to_path_buf(), reason: "it is not a directory".into() });
    }
    let _lock = layout::lock_shared(root)?;
    let is_backup = root.join(BACKUP_NAME).exists();
    let mut report = VerifyReport::new(root, if is_backup { Kind::Backup } else { Kind::DataDir });
    match layout::read_marker(root) {
        Ok(Some(info)) => {
            report.version = Some(info.version);
            report.history = info.history;
        }
        Ok(None) => {
            let reason = if root.join(layout::RESTORING_NAME).exists() {
                "it has no marker file: an interrupted restore".to_owned()
            } else {
                "it has no marker file (an interrupted initialization, backup or restore?)".to_owned()
            };
            return Err(Error::NotADataDir { path: root.to_path_buf(), reason });
        }
        Err(Error::InvalidDataDir { reason, .. }) => report.problem(Some(&root.join(MARKER_NAME)), reason),
        Err(e) => return Err(e),
    }
    if root.join(layout::RESTORING_NAME).exists() {
        report.problem(Some(&root.join(layout::RESTORING_NAME)), "an interrupted restore");
    }

    let checkpoint_dir = root.join(CHECKPOINT_DIR);
    let wal_dir = root.join(WAL_DIR);
    for dir in [&checkpoint_dir, &wal_dir] {
        if !dir.is_dir() {
            report.problem(Some(dir), "the directory is missing");
        }
    }
    list_other_files(&mut report, root)?;

    let manifest = if is_backup { read_manifest(&mut report, root) } else { None };
    let checkpoints = if checkpoint_dir.is_dir() { list_checkpoints(&checkpoint_dir)? } else { Vec::new() };
    let segments = if wal_dir.is_dir() { reader::list_segments(&wal_dir)? } else { Vec::new() };
    report.checkpoints = checkpoints.len();
    report.segments = segments.len();

    check_coverage(&mut report, &checkpoints, &segments);
    replay(&mut report, name, &checkpoints, &segments);

    if let Some(manifest) = manifest {
        check_backup(&mut report, root, &manifest);
    }
    Ok(report)
}

/// Temporary files and anything else that isn't ours.
fn list_other_files(report: &mut VerifyReport, root: &Path) -> Result<(), Error> {
    let known_root = [MARKER_NAME, LOCK_NAME, CHECKPOINT_DIR, WAL_DIR, BACKUP_NAME];
    for (dir, known) in
        [(root.to_path_buf(), &known_root[..]), (root.join(CHECKPOINT_DIR), &[][..]), (root.join(WAL_DIR), &[][..])]
    {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir).map_err(|e| Error::io("list", &dir, e))? {
            let entry = entry.map_err(|e| Error::io("list", &dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let ours = known.contains(&name.as_str())
                || (dir.ends_with(CHECKPOINT_DIR)
                    && crate::checkpoint::parse_checkpoint_name(&name).is_some()
                    && path.is_file())
                || (dir.ends_with(WAL_DIR) && format::parse_segment_name(&name).is_some() && path.is_file());
            if name.ends_with(TEMP_SUFFIX) {
                report.note(Some(&path), "a temporary file (an interrupted write; the next open removes it)");
            } else if !ours {
                report.note(Some(&path), "not a file of the database (ignored)");
            }
        }
    }
    Ok(())
}

/// Every checkpoint needs the WAL from its seq + 1 to the newest
/// checkpoint's seq, so recovery can fall back to it.
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
                if *ckpt == seq {
                    if let Some(loaded) = load_and_check(report, name, *ckpt, path) {
                        if let Err(difference) = invariants::compare(&loaded, &namespace) {
                            report.problem(
                                Some(path),
                                format!("differs from the WAL replayed to seq {}: {}", seq, difference),
                            );
                        }
                    }
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
        if let Some(torn) = reader.end().and_then(|end| end.last_segment.clone()) {
            if let Some(tail) = torn.torn {
                report.note(
                    Some(&torn.path),
                    format!(
                        "a torn tail at offset {} of {} bytes ({}; {} later frames): the end of the log after a crash, which the next open cuts",
                        torn.valid_len, torn.file_len, tail.damage, tail.discarded_frames
                    ),
                );
            }
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
    if let Err(reason) = check_checkpoint_header(path) {
        report.problem(Some(path), reason);
    }
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

/// Workaround for upstream #33: the core's loader checks only the magic
/// and version of a binary file's 16-byte header, and its CRC covers only
/// the payload, so damage in the `flags` (bytes 10..12) and `reserved`
/// (12..16) fields, documented as 0, would go unnoticed. Remove once the
/// core checks them (`documentation/steps/upstream-check.md`).
fn check_checkpoint_header(path: &Path) -> Result<(), String> {
    use std::io::Read;
    let mut header = [0u8; 16];
    let mut file = fs::File::open(path).map_err(|e| format!("can't be read: {}", e))?;
    // A file too short for a header fails to load anyway
    if file.read_exact(&mut header).is_err() || !header.starts_with(b"IRONWEAV") {
        return Ok(());
    }
    if header[10..16] != [0; 6] {
        return Err(format!(
            "its header's flags and reserved bytes are {:02x?}, not zero (damaged, or written by a newer version)",
            &header[10..16]
        ));
    }
    Ok(())
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

/// A backup's files and seq against its manifest.
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
    match report.seq {
        Some(seq) if seq != manifest.seq => report.problem(
            Some(&path),
            format!("the backup's WAL ends at seq {}, but its manifest says {}", seq, manifest.seq),
        ),
        _ => {}
    }
    report.seq = Some(manifest.seq);
}
