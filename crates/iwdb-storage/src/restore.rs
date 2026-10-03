//! Restore and point-in-time recovery: a new data directory at a chosen
//! point, from a backup, a WAL archive, or both (ADR 0009, ADR 0017;
//! `documentation/formats/data-dir.md`, "Restore").

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::Namespace;

use crate::archive::{archive_segments, read_archive_marker_info};
use crate::backup::{self, read_manifest, Manifest};
use crate::checkpoint::{list_checkpoints, load_checkpoint, write_checkpoint, SkippedCheckpoint};
use crate::history::HistoryId;
use crate::io::{LogFile, LogFs};
use crate::layout::{self, encode_marker, NsPaths, BACKUP_NAME, LOCK_NAME, MARKER_NAME, RESTORING_NAME};
use crate::namespaces::{
    read_log, write_whole, Event, EventKind, NamespaceInfo, NamespaceTable, DEFAULT_ID, DEFAULT_NAME, NAMESPACES_NAME,
    NS_DIR,
};
use crate::{reader, Error, WalReader};
use iwdb_engine::CommitTime;

/// Where a restore reads from: a backup (or any data directory that no
/// store has open), a WAL archive, or both, of the same history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreSources {
    pub backup: Option<PathBuf>,
    pub archive: Option<PathBuf>,
}

/// Where a restore goes. A restore always restores a whole store: every
/// namespace that existed at the target (or the ones named in `only`),
/// each to its own seq.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreTarget {
    /// The last seq the sources reach together, in each namespace (a
    /// backup alone: its seqs).
    Latest,
    /// Exactly this seq, in the one namespace the restore selects. With
    /// several namespaces seqs mean nothing together (every namespace has
    /// its own), so a restore to a seq selects one with `only` (or finds
    /// just one): [`Error::AmbiguousTarget`] otherwise.
    Seq(u64),
    /// The state as of this time: the namespaces that existed then, each
    /// at the last record, in seq order, whose commit time is at or before
    /// it (ADR 0010; a namespace created before the time with no commit
    /// yet is empty).
    Time(CommitTime),
}

/// What a restore did to one namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamespaceRestore {
    pub id: u64,
    pub name: String,
    /// The seq it was restored to: its next commit is `seq + 1`.
    pub seq: u64,
    /// The commit time of record `seq`, if a record was replayed and has
    /// one.
    pub time: Option<CommitTime>,
    /// The backup checkpoint replay started from (`None`: seq 0).
    pub checkpoint: Option<u64>,
    /// Newer checkpoints at or below `seq` that failed to load, newest first.
    pub skipped_checkpoints: Vec<SkippedCheckpoint>,
    /// WAL records replayed onto the checkpoint.
    pub replayed: u64,
    /// The segments read: from the backup, and from the archive (where
    /// both had a segment, the longer copy is counted).
    pub backup_segments: usize,
    pub archive_segments: usize,
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// The directory written.
    pub path: PathBuf,
    /// The new directory's history id (a new one).
    pub history: HistoryId,
    /// The history of the sources (`None` for a layout 1 data directory).
    pub source_history: Option<HistoryId>,
    /// Each namespace restored, by id.
    pub namespaces: Vec<NamespaceRestore>,
}

impl RestoreReport {
    /// The namespace `name`.
    pub fn namespace(&self, name: &str) -> Option<&NamespaceRestore> {
        self.namespaces.iter().find(|n| n.name == name)
    }
}

/// A backup (or data directory) opened for reading.
struct BackupSource {
    manifest: Option<Manifest>,
    /// The namespaces it holds, with their files.
    namespaces: Vec<(u64, NamespaceName, NsPaths)>,
    /// Its namespace log, if it has one (layout 4).
    log: Option<NamespaceTable>,
}

/// An archive opened for reading.
struct ArchiveSource {
    dir: PathBuf,
    version: u32,
    log: Option<NamespaceTable>,
}

/// The sources: their history and the shared lock on a data directory.
struct Opened {
    history: Option<HistoryId>,
    backup: Option<BackupSource>,
    archive: Option<ArchiveSource>,
    _lock: Option<File>,
}

/// What one namespace restores from.
struct Input {
    info: NamespaceInfo,
    /// The seq a backup's manifest says it reaches.
    manifest_seq: Option<u64>,
    checkpoints: Vec<(u64, PathBuf)>,
    backup_segments: Vec<(u64, PathBuf)>,
    archive_segments: Vec<(u64, PathBuf)>,
}

/// Restore into the new directory `dest` at `target`, reading `sources` and
/// writing through `fs`. With `only`, just the namespaces named there.
///
/// 1. The sources are checked: a backup must have a marker (it is read
///    under a shared lock: [`Error::Locked`] if a store has it open) and,
///    if it has one, a valid manifest; an archive must have its marker.
///    Their histories must be equal ([`Error::HistoryMismatch`]).
/// 2. The namespaces are chosen: the namespace log of the backup and of the
///    archive (one must be a prefix of the other: the longer is used)
///    says which existed at the target. A layout 1 to 3 backup has the one
///    namespace.
/// 3. For each namespace, the backup's and the archive's segments are read
///    as one log; where both have a segment of the same name, one must be
///    a prefix of the other (the backup cuts its last segment), and the
///    longer one is used; otherwise [`Error::ArchiveConflict`]. The target
///    seq `N` of the namespace is chosen ([`RestoreTarget`]).
/// 4. The newest backup checkpoint at or below `N` that loads (or an empty
///    namespace at seq 0) is loaded and the log replayed onto it up to
///    `N`, with a bounded reader: nothing after `N` is read.
/// 5. `dest` (missing or empty, not inside a source) is written: a
///    `RESTORING` file (fsynced), the lock, `ns/` with each namespace's
///    `checkpoints/` and `wal/`, synced; each checkpoint at its `N`
///    (`write_atomic`, unless `N` is 0), synced; the namespace log; then
///    `RESTORING` removed and the directory synced; then the marker, with
///    a **new** history id, and a directory sync.
///
/// The result opens as a store, each namespace at its seq `N` with an
/// empty WAL, so its next commit is `N + 1`. Its namespace log holds the
/// events up to the target (those of the namespaces restored, if `only`),
/// with their idempotency keys. An interrupted restore leaves `RESTORING`
/// (refused with [`Error::InterruptedRestore`]) or checkpoints without a
/// marker (refused with [`Error::NotADataDir`]); a restore of empty
/// namespaces interrupted after `RESTORING` is gone leaves directories
/// that a store initializes over, which is that state. Remove the
/// directory and restore again. The sources are never written.
///
/// Errors: [`Error::InvalidOptions`] (no source, or `dest` inside one),
/// [`Error::DestinationNotEmpty`], the sources' (`NotADataDir`,
/// `NotAnArchive`, `InvalidManifest`, `InvalidNamespaceLog`,
/// `InterruptedRestore`, `Locked`, `HistoryMismatch`, `ArchiveConflict`),
/// [`Error::NoSuchNamespace`] (a name in `only`), [`Error::AmbiguousTarget`],
/// [`Error::MissingRecords`] (a log doesn't reach back to the checkpoint,
/// or to seq 1), [`Error::LogEndsBefore`] (`N` is beyond the log),
/// [`Error::NoCommitAtOrBefore`], WAL corruption, [`Error::ReplayFailed`],
/// [`Error::Io`].
///
/// Memory: one namespace. Time: per namespace, the checkpoint's load, the
/// replay, one save (and, to a time, a read of the whole log first).
pub fn restore<F: LogFs>(
    fs: &F,
    sources: &RestoreSources,
    target: RestoreTarget,
    dest: &Path,
    only: Option<&[NamespaceName]>,
) -> Result<RestoreReport, Error> {
    if sources.backup.is_none() && sources.archive.is_none() {
        return Err(Error::InvalidOptions("a restore needs a backup, an archive, or both".into()));
    }
    for source in [&sources.backup, &sources.archive].into_iter().flatten() {
        backup::check_destination(source, dest)?;
    }
    let opened = open_sources(sources)?;
    let table = merged_log(&opened)?;

    // The namespaces at the target
    let cut = match target {
        RestoreTarget::Time(at) => Some(at),
        _ => None,
    };
    let mut chosen: Vec<NamespaceInfo> = match cut {
        Some(at) => table.live_at(at),
        None => table.live().cloned().collect(),
    };
    if let Some(only) = only {
        for name in only {
            if !table.events().iter().any(|e| &e.name == name) {
                return Err(Error::NoSuchNamespace { name: name.to_string() });
            }
        }
        chosen.retain(|n| only.contains(&n.name));
    }
    if let RestoreTarget::Seq(_) = target {
        if chosen.len() != 1 {
            return Err(Error::AmbiguousTarget { namespaces: chosen.iter().map(|n| n.name.to_string()).collect() });
        }
    }
    if chosen.is_empty() {
        if let Some(at) = cut {
            let first = table.events().first().map(|e| e.time);
            return Err(Error::NoCommitAtOrBefore { time: at, first });
        }
    }

    let mut restored = Vec::new();
    for info in chosen {
        let input = input_of(&opened, info)?;
        restored.push(restore_namespace(&input, target)?);
    }

    // The log of the new history: the events up to the target, of the
    // namespaces restored
    let selected: Vec<u64> = restored.iter().map(|(n, _)| n.id).collect();
    let mut events: Vec<Event> = table
        .events()
        .iter()
        .filter(|e| cut.is_none_or(|at| e.time <= at))
        .filter(|e| only.is_none() || selected.contains(&e.id))
        .cloned()
        .collect();
    for (i, event) in events.iter_mut().enumerate() {
        event.seq = i as u64 + 1;
    }
    let history = HistoryId::random();
    write_restored(fs, dest, &restored, &events, history)?;
    Ok(RestoreReport {
        path: dest.to_path_buf(),
        history,
        source_history: opened.history,
        namespaces: restored.into_iter().map(|(report, _)| report).collect(),
    })
}

/// Restore one namespace in memory: the report, and the namespace at its
/// target.
fn restore_namespace(input: &Input, target: RestoreTarget) -> Result<(NamespaceRestore, Namespace), Error> {
    let name = &input.info.name;
    let (log, backup_segments, archive_segments) = merge(&input.backup_segments, &input.archive_segments)?;
    let mut skipped = Vec::new();
    let (namespace, checkpoint, replayed, time) = match target {
        RestoreTarget::Latest => {
            // From the newest checkpoint, to the end of the log
            let (mut namespace, checkpoint) = load_base(&input.checkpoints, u64::MAX, name, &mut skipped);
            let (replayed, time) = replay(&log, &mut namespace, u64::MAX, true)?;
            if let Some(seq) = input.manifest_seq.filter(|seq| namespace.seq() < *seq) {
                return Err(Error::LogEndsBefore { from: seq, next_seq: namespace.seq() + 1 });
            }
            (namespace, checkpoint, replayed, time)
        }
        RestoreTarget::Seq(seq) => {
            let (mut namespace, checkpoint) = load_base(&input.checkpoints, seq, name, &mut skipped);
            let (replayed, time) = replay(&log, &mut namespace, seq, false)?;
            (namespace, checkpoint, replayed, time)
        }
        RestoreTarget::Time(at) => {
            let seq = seq_at_time(&log, at, &input.info, !input.checkpoints.is_empty())?;
            let (mut namespace, checkpoint) = load_base(&input.checkpoints, seq, name, &mut skipped);
            let (replayed, time) = replay(&log, &mut namespace, seq, false)?;
            (namespace, checkpoint, replayed, time)
        }
    };
    let report = NamespaceRestore {
        id: input.info.id,
        name: name.to_string(),
        seq: namespace.seq(),
        time,
        checkpoint,
        skipped_checkpoints: skipped,
        replayed,
        backup_segments,
        archive_segments,
    };
    Ok((report, namespace))
}

/// What namespace `info` restores from.
fn input_of(opened: &Opened, info: NamespaceInfo) -> Result<Input, Error> {
    let mut input = Input {
        manifest_seq: None,
        checkpoints: Vec::new(),
        backup_segments: Vec::new(),
        archive_segments: Vec::new(),
        info,
    };
    if let Some(backup) = &opened.backup {
        if let Some((_, _, paths)) = backup.namespaces.iter().find(|(id, _, _)| *id == input.info.id) {
            input.checkpoints = list_checkpoints(&paths.checkpoints)?;
            input.backup_segments = reader::list_segments(&paths.wal)?;
            input.manifest_seq = backup.manifest.as_ref().and_then(|m| m.namespace(input.info.id)).map(|n| n.seq);
        }
    }
    if let Some(archive) = &opened.archive {
        input.archive_segments = archive_segments(&archive.dir, archive.version, input.info.id)?;
    }
    Ok(input)
}

/// The namespace log of the sources: the backup's and the archive's, the
/// longer of the two (one must be a prefix of the other), or the single
/// `default` namespace of a layout 1 to 3 backup and format 1 archive.
fn merged_log(opened: &Opened) -> Result<NamespaceTable, Error> {
    let logs: Vec<&NamespaceTable> =
        [opened.backup.as_ref().and_then(|b| b.log.as_ref()), opened.archive.as_ref().and_then(|a| a.log.as_ref())]
            .into_iter()
            .flatten()
            .collect();
    let mut longest: Option<&NamespaceTable> = None;
    for log in logs {
        match longest {
            None => longest = Some(log),
            Some(have) => {
                let (short, long) = if have.events().len() <= log.events().len() { (have, log) } else { (log, have) };
                if !long.events().starts_with(short.events()) {
                    return Err(Error::ArchiveConflict {
                        path: opened.archive.as_ref().map_or_else(PathBuf::new, |a| a.dir.join(NAMESPACES_NAME)),
                    });
                }
                longest = Some(long);
            }
        }
    }
    match longest {
        Some(table) => Ok(table.clone()),
        None => {
            // Layout 1 to 3: one namespace, which has been there all along
            let name = NamespaceName::new(DEFAULT_NAME).map_err(iwdb_engine::Error::from)?;
            let event =
                Event { seq: 1, time: CommitTime(0), kind: EventKind::Create, id: DEFAULT_ID, name, keyed: None };
            NamespaceTable::from_events(vec![event])
                .map_err(|reason| Error::InvalidNamespaceLog { path: PathBuf::new(), reason })
        }
    }
}

fn open_sources(sources: &RestoreSources) -> Result<Opened, Error> {
    let mut opened = Opened { history: None, backup: None, archive: None, _lock: None };
    let mut backup_history = None;
    if let Some(dir) = &sources.backup {
        if !dir.is_dir() {
            return Err(Error::NotADataDir { path: dir.clone(), reason: "it is not a directory".into() });
        }
        opened._lock = layout::lock_shared(dir)?;
        if dir.join(RESTORING_NAME).exists() {
            return Err(Error::InterruptedRestore { path: dir.clone() });
        }
        let Some(info) = layout::read_marker(dir)? else {
            return Err(Error::NotADataDir {
                path: dir.clone(),
                reason: "it has no marker file (an interrupted backup or restore?)".into(),
            });
        };
        let mut manifest = None;
        if dir.join(BACKUP_NAME).exists() {
            let found = read_manifest(&dir.join(BACKUP_NAME))?;
            if Some(found.history) != info.history {
                return Err(Error::InvalidManifest {
                    path: dir.join(BACKUP_NAME),
                    reason: format!("its history {} differs from the marker's", found.history),
                });
            }
            manifest = Some(found);
        }
        let (namespaces, log) = if info.version >= 4 {
            let (_, table) = read_log(&dir.join(NAMESPACES_NAME))?;
            let list = match &manifest {
                // The manifest says what the backup holds
                Some(m) => m.namespaces.iter().map(|n| (n.id, n.name.clone())).collect(),
                None => table.live().map(|n| (n.id, n.name.clone())).collect::<Vec<_>>(),
            };
            (list.into_iter().map(|(id, name)| (id, name, NsPaths::new(dir, id))).collect(), Some(table))
        } else {
            let name = NamespaceName::new(DEFAULT_NAME).map_err(iwdb_engine::Error::from)?;
            (vec![(DEFAULT_ID, name, NsPaths::legacy(dir))], None)
        };
        backup_history = Some(info.history);
        opened.history = info.history;
        opened.backup = Some(BackupSource { manifest, namespaces, log });
    }
    if let Some(dir) = &sources.archive {
        let Some((version, history)) = read_archive_marker_info(dir)? else {
            return Err(Error::NotAnArchive { path: dir.clone(), reason: "it has no marker".into() });
        };
        if let Some(backup) = backup_history {
            if backup != Some(history) {
                return Err(Error::HistoryMismatch {
                    backup: backup.map_or("unknown (layout 1)".to_owned(), |h| h.to_string()),
                    archive: history.to_string(),
                });
            }
        }
        opened.history = Some(history);
        let log_path = dir.join(NAMESPACES_NAME);
        let log = if version >= 2 && log_path.exists() { Some(read_log(&log_path)?.1) } else { None };
        opened.archive = Some(ArchiveSource { dir: dir.clone(), version, log });
    }
    Ok(opened)
}

/// The backup's and the archive's segments as one log, and how many
/// segments each contributed.
/// Segments by first seq, as the reader takes them.
type Segments = Vec<(u64, PathBuf)>;

fn merge(backup: &[(u64, PathBuf)], archive: &[(u64, PathBuf)]) -> Result<(Segments, usize, usize), Error> {
    let mut log: BTreeMap<u64, (PathBuf, bool)> = archive.iter().map(|(s, p)| (*s, (p.clone(), false))).collect();
    for (seq, path) in backup {
        match log.get(seq) {
            None => {
                log.insert(*seq, (path.clone(), true));
            }
            Some((archived, _)) => {
                let (a, b) = (read(path)?, read(archived)?);
                if !(a.starts_with(&b) || b.starts_with(&a)) {
                    return Err(Error::ArchiveConflict { path: archived.clone() });
                }
                if a.len() > b.len() {
                    log.insert(*seq, (path.clone(), true));
                }
            }
        }
    }
    let from_backup = log.values().filter(|(_, b)| *b).count();
    let from_archive = log.len() - from_backup;
    Ok((log.into_iter().map(|(seq, (path, _))| (seq, path)).collect(), from_backup, from_archive))
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|e| Error::io("read", path, e))
}

/// The newest checkpoint at or below `seq` that loads, or an empty
/// namespace; the ones that fail are added to `skipped`.
fn load_base(
    checkpoints: &[(u64, PathBuf)],
    seq: u64,
    name: &NamespaceName,
    skipped: &mut Vec<SkippedCheckpoint>,
) -> (Namespace, Option<u64>) {
    for (ckpt, path) in checkpoints.iter().rev().filter(|(s, _)| *s <= seq) {
        match load_checkpoint(path, *ckpt, name) {
            Ok(loaded) => return (Namespace::from_loaded(loaded), Some(*ckpt)),
            Err(e) => {
                let reason = match e {
                    Error::InvalidCheckpoint { reason, .. } => reason,
                    other => other.to_string(),
                };
                skipped.push(SkippedCheckpoint { seq: *ckpt, path: path.clone(), reason });
            }
        }
    }
    (Namespace::new(name.clone()), None)
}

/// Replay `log` onto `namespace` up to `until` (`u64::MAX`: to the end).
/// With `to_end`, a log that ends at or before the namespace's seq is
/// fine: there is nothing to replay. Returns the records replayed and the
/// commit time of the last one.
fn replay(
    log: &[(u64, PathBuf)],
    namespace: &mut Namespace,
    until: u64,
    to_end: bool,
) -> Result<(u64, Option<CommitTime>), Error> {
    let from = namespace.seq() + 1;
    // The checkpoint is at the target (never above it)
    if until < from {
        return Ok((0, None));
    }
    if log.is_empty() {
        return if to_end { Ok((0, None)) } else { Err(Error::LogEndsBefore { from: until, next_seq: from }) };
    }
    // Fails with MissingRecords if the log starts after `from`
    let mut reader = WalReader::from_segments(log.to_vec(), from, until)?;
    let (mut replayed, mut time) = (0, None);
    while let Some(record) = reader.next_timed() {
        let (record, record_time) = match record {
            Ok(record) => record,
            // To the end: the log ends before `from`, nothing after the checkpoint
            Err(Error::LogEndsBefore { from: f, .. }) if to_end && f == from => break,
            Err(e) => return Err(e),
        };
        let seq = record.seq;
        namespace.replay(record, record_time).map_err(|source| Error::ReplayFailed { seq, source })?;
        replayed += 1;
        time = record_time;
    }
    Ok((replayed, time))
}

/// The last record, in seq order, whose commit time is at or before `at`.
/// A namespace created at a known time (not a layout 1 to 3 one, whose
/// create time is 0) at or before `at` that has no such record was empty
/// then, if its log starts at seq 1: seq 0.
fn seq_at_time(
    log: &[(u64, PathBuf)],
    at: CommitTime,
    info: &NamespaceInfo,
    has_checkpoints: bool,
) -> Result<u64, Error> {
    let known_empty = info.created != CommitTime(0) && info.created <= at;
    let Some(&(first, _)) = log.first() else {
        return if known_empty && !has_checkpoints {
            Ok(0)
        } else {
            Err(Error::NoCommitAtOrBefore { time: at, first: None })
        };
    };
    let mut reader = WalReader::from_segments(log.to_vec(), first, u64::MAX)?;
    let (mut found, mut earliest) = (None, None);
    while let Some(record) = reader.next() {
        let record = record?;
        if let Some(time) = reader.time() {
            earliest = Some(earliest.map_or(time, |e: CommitTime| e.min(time)));
            if time <= at {
                found = Some(record.seq);
            }
        }
    }
    match found {
        Some(seq) => Ok(seq),
        None if known_empty && first == 1 => Ok(0),
        None => Err(Error::NoCommitAtOrBefore { time: at, first: earliest }),
    }
}

/// Write the restored directory (see [`restore`]).
fn write_restored<F: LogFs>(
    fs: &F,
    dest: &Path,
    restored: &[(NamespaceRestore, Namespace)],
    events: &[Event],
    history: HistoryId,
) -> Result<(), Error> {
    match fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::DestinationNotEmpty { path: dest.to_path_buf() });
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => crate::io::create_dir(fs, dest)?,
        Err(e) => return Err(Error::io("list", dest, e)),
    }
    let restoring = dest.join(RESTORING_NAME);
    let mut file = fs.create(&restoring).map_err(|e| Error::io("create", &restoring, e))?;
    file.write_all(b"a restore is writing this directory; if it stopped, remove the directory and restore again\n")
        .map_err(|e| Error::io("write", &restoring, e))?;
    file.sync().map_err(|e| Error::io("fsync", &restoring, e))?;
    drop(file);
    let lock_path = dest.join(LOCK_NAME);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| Error::io("open", &lock_path, e))?;
    crate::layout::lock_file(&lock, &lock_path, true)?;
    let ns_root = dest.join(NS_DIR);
    fs.create_dir(&ns_root).map_err(|e| Error::io("create directory", &ns_root, e))?;
    crate::io::sync_dir(fs, dest)?;
    let mut paths = Vec::new();
    for (report, _) in restored {
        paths.push(crate::layout::create_ns_dir(fs, dest, report.id)?);
    }
    for ((_, namespace), paths) in restored.iter().zip(&paths) {
        if namespace.seq() > 0 {
            write_checkpoint(fs, &paths.checkpoints, namespace)?;
            crate::io::sync_dir(fs, &paths.checkpoints)?;
        }
    }
    write_whole(fs, &dest.join(NAMESPACES_NAME), events)?;
    crate::io::sync_dir(fs, dest)?;
    fs.remove_file(&restoring).map_err(|e| Error::io("remove", &restoring, e))?;
    crate::io::sync_dir(fs, dest)?;
    crate::io::write_atomic(fs, &dest.join(MARKER_NAME), &encode_marker(history))?;
    crate::io::sync_dir(fs, dest)?;
    drop(lock);
    Ok(())
}

crate::default_deref!(RestoreReport, NamespaceRestore, namespaces, |n| n.name.as_str());
