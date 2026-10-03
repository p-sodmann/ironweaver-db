//! Checkpoints: binary saves of a namespace at a `seq`, and the
//! [`Checkpointer`] that writes them and cuts the WAL. The normative
//! description is `documentation/formats/data-dir.md`.
//!
//! A checkpoint `checkpoints/<seq, 20 digits>.ckpt` is a file in the core's
//! binary format (version 2) written with the core's `write_atomic`. Its
//! graph meta holds `iwdb.catalog` and `iwdb.seq` (ADR 0003). It holds
//! exactly the state after the commits `1 ..= seq`, catalog included, and
//! `seq` equals the number in its name.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use iwdb_engine::Namespace;
use iwdb_engine::catalog::{IndexChanges, NamespaceName};
use iwdb_engine::codec::{self, Loaded};

use crate::archive::ArchiveHandle;
use crate::io::LogFs;
use crate::layout::NsPaths;
use crate::{Error, WalReader, reader};

/// Suffix of checkpoint file names: `<seq, 20 digits>.ckpt`.
pub const CHECKPOINT_SUFFIX: &str = ".ckpt";

/// The file name of the checkpoint at `seq`, zero-padded to 20 digits so
/// that names sort like seqs.
pub fn checkpoint_name(seq: u64) -> String {
    format!("{:020}{}", seq, CHECKPOINT_SUFFIX)
}

/// The seq of a checkpoint file name, or `None` if the name isn't one.
/// Only the canonical form (exactly 20 digits, `.ckpt`) is accepted.
pub fn parse_checkpoint_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(CHECKPOINT_SUFFIX)?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The checkpoints in `dir`, sorted by seq (oldest first). Other files,
/// such as temporary files, are ignored.
pub fn list_checkpoints(dir: &Path) -> Result<Vec<(u64, PathBuf)>, Error> {
    let mut checkpoints = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
        let entry = entry.map_err(|e| Error::io("list", dir, e))?;
        if let Some(seq) = entry.file_name().to_str().and_then(parse_checkpoint_name) {
            checkpoints.push((seq, entry.path()));
        }
    }
    checkpoints.sort_unstable();
    Ok(checkpoints)
}

/// A checkpoint that failed to load and was skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedCheckpoint {
    pub seq: u64,
    pub path: PathBuf,
    /// Why it failed to load.
    pub reason: String,
}

pub(crate) fn describe(skipped: &[SkippedCheckpoint]) -> String {
    if skipped.is_empty() {
        return "none".into();
    }
    skipped.iter().map(|s| format!("{} ({})", s.seq, s.reason)).collect::<Vec<_>>().join(", ")
}

/// Load the checkpoint at `path`, which its name says is at `seq`, for the
/// namespace `name`: streaming (peak memory is the graph plus a small
/// buffer), with the indexes rebuilt from the catalog.
///
/// Fails with [`Error::InvalidCheckpoint`] if the file can't be read or
/// decoded (a checksum mismatch, truncation, an invalid catalog), or if its
/// seq or namespace differ from what its name and the store say. Never
/// panics on corrupt input.
pub fn load_checkpoint(path: &Path, seq: u64, name: &NamespaceName) -> Result<Loaded, Error> {
    let invalid = |reason: String| Error::InvalidCheckpoint { path: path.to_path_buf(), reason };
    let file = File::open(path).map_err(|e| invalid(e.to_string()))?;
    let loaded = codec::from_binary_reader(file).map_err(|e| invalid(e.to_string()))?;
    if loaded.meta.seq != seq {
        return Err(invalid(format!("it holds seq {}, its name says {}", loaded.meta.seq, seq)));
    }
    if &loaded.meta.namespace != name {
        return Err(invalid(format!("it belongs to namespace '{}', not '{}'", loaded.meta.namespace, name)));
    }
    Ok(loaded)
}

/// Where recovery or the checkpointer starts: the newest checkpoint that
/// loads, as a namespace.
#[derive(Debug)]
pub struct Base {
    pub namespace: Namespace,
    /// The seq of the checkpoint loaded, `None` if none loaded (an empty
    /// namespace at seq 0).
    pub checkpoint: Option<u64>,
    /// Newer checkpoints that failed to load, newest first.
    pub skipped: Vec<SkippedCheckpoint>,
    /// How the indexes saved in the loaded checkpoint differed from its
    /// catalog. Empty for checkpoints the database wrote; anything else
    /// is a bug worth reporting.
    pub index_changes: IndexChanges,
}

/// Load the newest checkpoint in `dir` that loads, trying older ones in
/// turn; an empty namespace `name` if none does. Whether the WAL still
/// reaches back to the checkpoint found is for the caller to check.
pub fn load_newest(dir: &Path, name: &NamespaceName) -> Result<Base, Error> {
    let mut skipped = Vec::new();
    for (seq, path) in list_checkpoints(dir)?.into_iter().rev() {
        match load_checkpoint(&path, seq, name) {
            Ok(loaded) => {
                let index_changes = loaded.index_changes.clone();
                return Ok(Base {
                    namespace: Namespace::from_loaded(loaded),
                    checkpoint: Some(seq),
                    skipped,
                    index_changes,
                });
            }
            Err(e) => {
                let reason = match e {
                    Error::InvalidCheckpoint { reason, .. } => reason,
                    other => other.to_string(),
                };
                skipped.push(SkippedCheckpoint { seq, path, reason });
            }
        }
    }
    Ok(Base {
        namespace: Namespace::new(name.clone()),
        checkpoint: None,
        skipped,
        index_changes: IndexChanges::default(),
    })
}

/// Write the checkpoint of `namespace` (at its seq) into `dir` with
/// `write_atomic`, replacing a file of the same name. On error the previous
/// checkpoints are untouched and the temporary file is removed when
/// possible. The rename is durable only after the caller syncs `dir`.
pub fn write_checkpoint<F: LogFs>(fs: &F, dir: &Path, namespace: &Namespace) -> Result<PathBuf, Error> {
    let path = dir.join(checkpoint_name(namespace.seq()));
    let meta = namespace.graph_meta();
    fs.write_atomic(&path, &mut |out| codec::write_binary(namespace.graph(), &meta, out).map_err(io::Error::other))
        .map_err(|e| Error::io("write checkpoint", &path, e))?;
    Ok(path)
}

/// What a checkpoint run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckpointOutcome {
    /// The seq of the newest checkpoint after the run.
    pub seq: u64,
    /// Whether a checkpoint was written (false if the newest one was at
    /// the target already).
    pub written: bool,
    /// Seqs of the checkpoints removed.
    pub removed_checkpoints: Vec<u64>,
    /// First seqs of the WAL segments removed.
    pub removed_segments: Vec<u64>,
}

/// Writes checkpoints and cuts the WAL, without touching the live
/// namespace.
///
/// It keeps its own copy of the namespace, like a replica: loaded once
/// from the newest checkpoint that loads (streaming), then brought forward
/// by replaying WAL records up to each target seq. A run
///
/// 1. replays records up to the target with a bounded reader
///    ([`WalReader::open_until`]), which never looks past the target, so
///    the writer can append to the same segment meanwhile. The target must
///    be synced (at most [`Wal::synced_seq`](crate::Wal::synced_seq)):
///    otherwise an OS crash could leave a checkpoint newer than the log;
/// 2. writes `checkpoints/<target>.ckpt` with `write_atomic` and syncs the
///    directory, so the checkpoint is durable before anything is deleted;
/// 3. keeps the newest `keep` checkpoints that aren't known to be damaged,
///    and removes every checkpoint older than the oldest of them (so
///    damaged checkpoints are removed once they fall behind), then syncs;
/// 4. removes the WAL segments whose records all lie at or below the
///    oldest kept checkpoint (a segment ends where the next begins; the
///    last segment is never removed), then syncs. So recovery can always
///    fall back to any kept checkpoint and replay the WAL from there. With
///    an [`ArchiveHandle`] ([`set_archive`](Self::set_archive)), the segments are
///    first copied into the archive and the archive directory synced, so
///    a segment leaves `wal/` only once it is durable in the archive.
///
/// Failures: an error while replaying or writing the checkpoint (step 1-2
/// before the rename) deletes nothing and can be retried at the next run.
/// A failure after the rename (the directory sync, a removal, their
/// syncs) disables the checkpointer until the store is reopened
/// ([`Error::CheckpointsDisabled`]): a failed directory fsync is never
/// retried, because a retry can succeed without making the entries durable
/// (fsyncgate), and deleting on that basis could lose data. A replay
/// failure disables it too: the live namespace would have failed on the
/// same record. A failed archive copy (a write, an fsync, a rename, a
/// conflict) removes no segment and fails the run, and the next run that
/// writes a checkpoint retries it; a failed sync of the archive directory
/// disables the checkpointer, like any failed directory sync.
///
/// Memory: its namespace is a second copy of the live one, kept between
/// runs. Time: O(records replayed) plus O(graph) for the save.
pub struct Checkpointer<F: LogFs> {
    fs: F,
    checkpoints: PathBuf,
    wal: PathBuf,
    name: NamespaceName,
    keep: usize,
    namespace: Option<Namespace>,
    /// The newest checkpoint known to be valid (loaded or written).
    newest: Option<u64>,
    /// Checkpoints known to be damaged (they failed to load).
    bad: BTreeSet<u64>,
    disabled: Option<String>,
    archive: Option<ArchiveHandle<F>>,
}

impl<F: LogFs> std::fmt::Debug for Checkpointer<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpointer")
            .field("checkpoints", &self.checkpoints)
            .field("keep", &self.keep)
            .field("seq", &self.namespace.as_ref().map(Namespace::seq))
            .field("newest", &self.newest)
            .field("bad", &self.bad)
            .field("disabled", &self.disabled)
            .finish_non_exhaustive()
    }
}

impl<F: LogFs> Checkpointer<F> {
    /// A checkpointer for the namespace `name` in `paths`,
    /// keeping `keep` checkpoints (at least 1). `newest` and `bad` are what
    /// recovery learned: the checkpoint it loaded and the ones it skipped.
    pub fn new(
        fs: F,
        paths: &NsPaths,
        name: NamespaceName,
        keep: usize,
        newest: Option<u64>,
        bad: impl IntoIterator<Item = u64>,
    ) -> Self {
        Checkpointer {
            fs,
            checkpoints: paths.checkpoints.clone(),
            wal: paths.wal.clone(),
            name,
            keep: keep.max(1),
            namespace: None,
            newest,
            bad: bad.into_iter().collect(),
            disabled: None,
            archive: None,
        }
    }

    /// Archive WAL segments into `archive` before removing them (see the
    /// type docs).
    pub fn set_archive(&mut self, archive: ArchiveHandle<F>) {
        self.archive = Some(archive);
    }

    /// The archive, if segments are archived.
    pub fn archive(&self) -> Option<&ArchiveHandle<F>> {
        self.archive.as_ref()
    }

    /// The seq of the newest valid checkpoint, if any.
    pub fn newest(&self) -> Option<u64> {
        self.newest
    }

    /// The checkpoints known to be damaged (they failed to load), which a
    /// backup doesn't copy.
    pub fn damaged(&self) -> &BTreeSet<u64> {
        &self.bad
    }

    /// Why checkpoints are disabled, if they are.
    pub fn disabled(&self) -> Option<&str> {
        self.disabled.as_deref()
    }

    /// Checkpoint at `target` (see the type docs). `target` must be a
    /// synced seq of the log. A target at or below the checkpointer's own
    /// seq writes nothing new.
    pub fn run(&mut self, target: u64) -> Result<CheckpointOutcome, Error> {
        if let Some(cause) = &self.disabled {
            return Err(Error::CheckpointsDisabled { cause: cause.clone() });
        }
        let seq = self.advance(target)?;
        if self.newest == Some(seq) || seq == 0 {
            return Ok(CheckpointOutcome { seq: self.newest.unwrap_or(0), ..CheckpointOutcome::default() });
        }
        let Some(namespace) = &self.namespace else {
            return Ok(CheckpointOutcome::default());
        };
        write_checkpoint(&self.fs, &self.checkpoints, namespace)?;
        let dir = self.checkpoints.clone();
        self.sync(&dir)?;
        self.newest = Some(seq);
        self.bad.remove(&seq);
        let (cutoff, removed_checkpoints) = self.remove_old_checkpoints(seq)?;
        let removed_segments = self.remove_segments(cutoff)?;
        Ok(CheckpointOutcome { seq, written: true, removed_checkpoints, removed_segments })
    }

    /// Load the namespace if needed and replay the log up to `target`;
    /// returns the namespace's seq.
    fn advance(&mut self, target: u64) -> Result<u64, Error> {
        let namespace = match self.namespace.take() {
            Some(namespace) => namespace,
            None => {
                let base = load_newest(&self.checkpoints, &self.name)?;
                self.bad.extend(base.skipped.iter().map(|s| s.seq));
                self.newest = self.newest.max(base.checkpoint);
                base.namespace
            }
        };
        // On error the namespace is dropped and loaded again next time
        let mut namespace = namespace;
        if target > namespace.seq() {
            let mut reader = WalReader::open_until(&self.wal, namespace.seq() + 1, target)?;
            while let Some(record) = reader.next_timed() {
                let (record, time) = record?;
                let seq = record.seq;
                if let Err(source) = namespace.replay(record, time) {
                    let error = Error::ReplayFailed { seq, source };
                    self.disabled = Some(error.to_string());
                    return Err(error);
                }
            }
        }
        let seq = namespace.seq();
        self.namespace = Some(namespace);
        Ok(seq)
    }

    /// Keep the checkpoint at `seq` and the newest `keep - 1` older ones
    /// not known to be damaged; remove everything older than the oldest
    /// kept. Returns that oldest seq and the removed ones.
    fn remove_old_checkpoints(&mut self, seq: u64) -> Result<(u64, Vec<u64>), Error> {
        let all = self.guard(list_checkpoints(&self.checkpoints))?;
        let cutoff = all
            .iter()
            .rev()
            .map(|(s, _)| *s)
            .filter(|s| *s < seq && !self.bad.contains(s))
            .take(self.keep - 1)
            .fold(seq, u64::min);
        let mut removed = Vec::new();
        for (s, path) in all.into_iter().filter(|(s, _)| *s < cutoff) {
            let result = self.fs.remove_file(&path).map_err(|e| Error::io("remove", &path, e));
            self.guard(result)?;
            self.bad.remove(&s);
            removed.push(s);
        }
        if !removed.is_empty() {
            let dir = self.checkpoints.clone();
            self.sync(&dir)?;
        }
        Ok((cutoff, removed))
    }

    /// Remove the WAL segments whose records are all at or below `cutoff`,
    /// archiving them first if there is an archive.
    fn remove_segments(&mut self, cutoff: u64) -> Result<Vec<u64>, Error> {
        let segments = self.guard(reader::list_segments(&self.wal))?;
        let mut removable = Vec::new();
        for [(first_seq, path), (next_first, _)] in segments.array_windows() {
            if *next_first > cutoff.saturating_add(1) {
                break;
            }
            removable.push((*first_seq, path.clone()));
        }
        if self.archive.is_some() && !removable.is_empty() {
            // A failed copy leaves everything in `wal/`: retried next time.
            // Making the namespace's archive directory is a directory sync
            // like the others: a failure disables the checkpointer
            let made = self.archive.as_ref().map_or(Ok(()), ArchiveHandle::ensure_dir);
            self.guard(made)?;
            if let Some(archive) = &self.archive {
                archive.copy(&removable)?;
            }
            let result = self.archive.as_ref().map_or(Ok(()), ArchiveHandle::sync);
            self.guard(result)?;
        }
        let mut removed = Vec::new();
        for (first_seq, path) in removable {
            let result = self.fs.remove_file(&path).map_err(|e| Error::io("remove", &path, e));
            self.guard(result)?;
            removed.push(first_seq);
        }
        if !removed.is_empty() {
            let dir = self.wal.clone();
            self.sync(&dir)?;
        }
        Ok(removed)
    }

    fn sync(&mut self, dir: &Path) -> Result<(), Error> {
        let result = self.fs.sync_dir(dir).map_err(|e| Error::io("sync directory", dir, e));
        self.guard(result)
    }

    /// Disable the checkpointer if `result` is an error.
    fn guard<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(e) = &result {
            self.disabled = Some(e.to_string());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sort_like_seqs() {
        assert_eq!(checkpoint_name(42), "00000000000000000042.ckpt");
        assert_eq!(parse_checkpoint_name(&checkpoint_name(u64::MAX)), Some(u64::MAX));
        for bad in
            ["42.ckpt", "0000000000000000004x.ckpt", "00000000000000000042.wal", ".00000000000000000042.ckpt.1.0.tmp"]
        {
            assert_eq!(parse_checkpoint_name(bad), None, "{}", bad);
        }
        assert!(checkpoint_name(9) < checkpoint_name(10));
    }
}
