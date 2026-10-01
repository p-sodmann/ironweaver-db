//! Restore and point-in-time recovery: a new data directory at a chosen
//! seq, from a backup, a WAL archive, or both (ADR 0009;
//! `documentation/formats/data-dir.md`, "Restore").

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::Namespace;

use crate::archive::read_archive_marker;
use crate::backup::{self, read_manifest};
use crate::checkpoint::{list_checkpoints, load_checkpoint, write_checkpoint, SkippedCheckpoint};
use crate::history::HistoryId;
use crate::io::{LogFile, LogFs};
use crate::layout::{
    self, encode_marker, BACKUP_NAME, CHECKPOINT_DIR, LOCK_NAME, MARKER_NAME, RESTORING_NAME, WAL_DIR,
};
use crate::time::CommitTime;
use crate::{reader, Error, WalReader};

/// Where a restore reads from: a backup (or any data directory that no
/// store has open), a WAL archive, or both, of the same history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreSources {
    pub backup: Option<PathBuf>,
    pub archive: Option<PathBuf>,
}

/// The seq a restore goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreTarget {
    /// The last seq the sources reach together (a backup alone: its seq).
    Latest,
    /// Exactly this seq.
    Seq(u64),
    /// The last record, in seq order, whose commit time is at or before
    /// this time (ADR 0010).
    Time(CommitTime),
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// The directory written.
    pub path: PathBuf,
    /// The seq it was restored to: its next commit is `seq + 1`.
    pub seq: u64,
    /// The commit time of record `seq`, if a record was replayed and has
    /// one.
    pub time: Option<CommitTime>,
    /// The new directory's history id (a new one).
    pub history: HistoryId,
    /// The history of the sources (`None` for a layout 1 data directory).
    pub source_history: Option<HistoryId>,
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

/// A source opened for reading: its checkpoints and segments, its history,
/// and the shared lock on a data directory.
struct Opened {
    history: Option<HistoryId>,
    /// The seq a backup's manifest says it reaches.
    manifest_seq: Option<u64>,
    checkpoints: Vec<(u64, PathBuf)>,
    backup_segments: Vec<(u64, PathBuf)>,
    archive_segments: Vec<(u64, PathBuf)>,
    _lock: Option<File>,
}

/// Restore the namespace `name` into the new directory `dest` at `target`,
/// reading `sources` and writing through `fs`.
///
/// 1. The sources are checked: a backup must have a marker (it is read
///    under a shared lock: [`Error::Locked`] if a store has it open) and,
///    if it has one, a valid manifest; an archive must have its marker.
///    Their histories must be equal ([`Error::HistoryMismatch`]).
/// 2. Their segments are read as one log; where both have a segment of
///    the same name, one must be a prefix of the other (the backup cuts
///    its last segment), and the longer one is used; otherwise
///    [`Error::ArchiveConflict`].
/// 3. The target seq `N` is chosen ([`RestoreTarget`]).
/// 4. The newest backup checkpoint at or below `N` that loads (or an empty
///    namespace at seq 0) is loaded and the log replayed onto it up to
///    `N`, with a bounded reader: nothing after `N` is read.
/// 5. `dest` (missing or empty, not inside a source) is written: a
///    `RESTORING` file (fsynced), the lock, `checkpoints/` and `wal/`,
///    synced; the checkpoint at `N` (`write_atomic`, unless `N` is 0) and
///    `checkpoints/` synced; `RESTORING` removed and the directory synced;
///    then the marker, with a **new** history id, and a directory sync.
///
/// The result opens as a store at seq `N`, with an empty WAL, so its next
/// commit is `N + 1`. An interrupted restore leaves `RESTORING` (refused
/// with [`Error::InterruptedRestore`]) or a checkpoint without a marker
/// (refused with [`Error::NotADataDir`]); a restore to seq 0 interrupted
/// after `RESTORING` is gone leaves an empty directory, which is that
/// state. Remove the directory and restore again. The sources are never
/// written.
///
/// Errors: [`Error::InvalidOptions`] (no source, or `dest` inside one),
/// [`Error::DestinationNotEmpty`], the sources' (`NotADataDir`,
/// `NotAnArchive`, `InvalidManifest`, `InterruptedRestore`, `Locked`,
/// `HistoryMismatch`, `ArchiveConflict`), [`Error::MissingRecords`] (the
/// log doesn't reach back to the checkpoint, or to seq 1),
/// [`Error::LogEndsBefore`] (`N` is beyond the log),
/// [`Error::NoCommitAtOrBefore`], WAL corruption, [`Error::ReplayFailed`],
/// [`Error::Io`].
///
/// Memory: one namespace. Time: the checkpoint's load, the replay, one
/// save (and, to a time, a read of the whole log first).
pub fn restore<F: LogFs>(
    fs: &F,
    sources: &RestoreSources,
    target: RestoreTarget,
    dest: &Path,
    name: &NamespaceName,
) -> Result<RestoreReport, Error> {
    if sources.backup.is_none() && sources.archive.is_none() {
        return Err(Error::InvalidOptions("a restore needs a backup, an archive, or both".into()));
    }
    for source in [&sources.backup, &sources.archive].into_iter().flatten() {
        backup::check_destination(source, dest)?;
    }
    let opened = open_sources(sources)?;
    let (log, backup_segments, archive_segments) = merge(&opened.backup_segments, &opened.archive_segments)?;

    let mut skipped = Vec::new();
    let (namespace, checkpoint, replayed, time) = match target {
        RestoreTarget::Latest => {
            // From the newest checkpoint, to the end of the log
            let (mut namespace, checkpoint) = load_base(&opened.checkpoints, u64::MAX, name, &mut skipped);
            let (replayed, time) = replay(&log, &mut namespace, u64::MAX, true)?;
            if let Some(seq) = opened.manifest_seq.filter(|seq| namespace.seq() < *seq) {
                return Err(Error::LogEndsBefore { from: seq, next_seq: namespace.seq() + 1 });
            }
            (namespace, checkpoint, replayed, time)
        }
        RestoreTarget::Seq(seq) => {
            let (mut namespace, checkpoint) = load_base(&opened.checkpoints, seq, name, &mut skipped);
            let (replayed, time) = replay(&log, &mut namespace, seq, false)?;
            (namespace, checkpoint, replayed, time)
        }
        RestoreTarget::Time(at) => {
            let seq = seq_at_time(&log, at)?;
            let (mut namespace, checkpoint) = load_base(&opened.checkpoints, seq, name, &mut skipped);
            let (replayed, time) = replay(&log, &mut namespace, seq, false)?;
            (namespace, checkpoint, replayed, time)
        }
    };

    let history = HistoryId::random();
    write_restored(fs, dest, &namespace, history)?;
    Ok(RestoreReport {
        path: dest.to_path_buf(),
        seq: namespace.seq(),
        time,
        history,
        source_history: opened.history,
        checkpoint,
        skipped_checkpoints: skipped,
        replayed,
        backup_segments,
        archive_segments,
    })
}

fn open_sources(sources: &RestoreSources) -> Result<Opened, Error> {
    let mut opened = Opened {
        history: None,
        manifest_seq: None,
        checkpoints: Vec::new(),
        backup_segments: Vec::new(),
        archive_segments: Vec::new(),
        _lock: None,
    };
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
        if dir.join(BACKUP_NAME).exists() {
            let manifest = read_manifest(&dir.join(BACKUP_NAME))?;
            if Some(manifest.history) != info.history {
                return Err(Error::InvalidManifest {
                    path: dir.join(BACKUP_NAME),
                    reason: format!("its history {} differs from the marker's", manifest.history),
                });
            }
            opened.manifest_seq = Some(manifest.seq);
        }
        backup_history = Some(info.history);
        opened.history = info.history;
        opened.checkpoints = list_checkpoints(&dir.join(CHECKPOINT_DIR))?;
        opened.backup_segments = reader::list_segments(&dir.join(WAL_DIR))?;
    }
    if let Some(dir) = &sources.archive {
        let Some(history) = read_archive_marker(dir)? else {
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
        opened.archive_segments = reader::list_segments(dir)?;
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
    while let Some(record) = reader.next() {
        let record = match record {
            Ok(record) => record,
            // To the end: the log ends before `from`, nothing after the checkpoint
            Err(Error::LogEndsBefore { from: f, .. }) if to_end && f == from => break,
            Err(e) => return Err(e),
        };
        let seq = record.seq;
        namespace.replay(record).map_err(|source| Error::ReplayFailed { seq, source })?;
        replayed += 1;
        time = reader.time();
    }
    Ok((replayed, time))
}

/// The last record, in seq order, whose commit time is at or before `at`.
fn seq_at_time(log: &[(u64, PathBuf)], at: CommitTime) -> Result<u64, Error> {
    let Some(&(first, _)) = log.first() else {
        return Err(Error::NoCommitAtOrBefore { time: at, first: None });
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
    found.ok_or(Error::NoCommitAtOrBefore { time: at, first: earliest })
}

/// Write the restored directory (see [`restore`], step 5).
fn write_restored<F: LogFs>(fs: &F, dest: &Path, namespace: &Namespace, history: HistoryId) -> Result<(), Error> {
    match fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::DestinationNotEmpty { path: dest.to_path_buf() });
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => backup::create_dir(fs, dest)?,
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
    for sub in [CHECKPOINT_DIR, WAL_DIR] {
        let path = dest.join(sub);
        fs::create_dir(&path).map_err(|e| Error::io("create directory", &path, e))?;
    }
    backup::sync_dir(fs, dest)?;
    if namespace.seq() > 0 {
        write_checkpoint(fs, &dest.join(CHECKPOINT_DIR), namespace)?;
        backup::sync_dir(fs, &dest.join(CHECKPOINT_DIR))?;
    }
    fs.remove_file(&restoring).map_err(|e| Error::io("remove", &restoring, e))?;
    backup::sync_dir(fs, dest)?;
    backup::write_atomic(fs, &dest.join(MARKER_NAME), &encode_marker(history))?;
    backup::sync_dir(fs, dest)?;
    drop(lock);
    Ok(())
}
