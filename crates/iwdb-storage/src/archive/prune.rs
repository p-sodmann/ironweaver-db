//! Pruning a WAL archive: removing what only restores to before a backup
//! need (step 16e, ADR 0055).

use std::path::{Path, PathBuf};

use super::{
    ARCHIVE_MARKER_NAME, archive_checkpoints, archive_namespace_ids, archive_segments, read_archive_marker_info,
};
use crate::backup::{Manifest, read_manifest};
use crate::checkpoint::parse_checkpoint_name;
use crate::io::{LogFs, sync_dir};
use crate::layout::{self, BACKUP_NAME, CHECKPOINT_DIR};
use crate::namespaces::{DEFAULT_ID, NS_DIR, ns_dir_name};
use crate::{Error, HistoryId};

/// What [`prune`] removed (or, in a dry run, would remove) from one
/// namespace of the archive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamespacePrune {
    pub id: u64,
    pub name: String,
    /// The oldest checkpoint of the namespace in the backup (0: none, and
    /// nothing is removed). Every archived record after it is kept.
    pub backup_checkpoint: u64,
    /// The first seqs of the segments removed.
    pub removed_segments: Vec<u64>,
    /// The seqs of the archived checkpoints (imports') removed.
    pub removed_checkpoints: Vec<u64>,
    /// The segments that stay.
    pub kept_segments: usize,
}

/// What [`prune`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneReport {
    pub archive: PathBuf,
    pub backup: PathBuf,
    /// Nothing was removed: the report says what would be.
    pub dry_run: bool,
    /// The namespaces the backup holds, by id.
    pub namespaces: Vec<NamespacePrune>,
    /// Namespaces of the archive the backup doesn't hold (created after
    /// it, or dropped before it): left alone.
    pub untouched: Vec<u64>,
    /// Bytes removed (or that would be).
    pub bytes: u64,
}

/// The oldest checkpoint of namespace `id` that the backup's manifest lists
/// and that is in the backup (0 if none).
fn oldest_checkpoint(backup: &Path, manifest: &Manifest, id: u64) -> u64 {
    let prefix = if manifest.version >= 2 {
        format!("{}/{}/{}/", NS_DIR, ns_dir_name(id), CHECKPOINT_DIR)
    } else if id == DEFAULT_ID {
        format!("{}/", CHECKPOINT_DIR)
    } else {
        return 0;
    };
    manifest
        .files
        .iter()
        .filter_map(|f| {
            let name = f.path.strip_prefix(&prefix)?;
            let seq = parse_checkpoint_name(name)?;
            backup.join(&f.path).is_file().then_some(seq)
        })
        .min()
        .unwrap_or(0)
}

/// Remove from the WAL archive `archive` what no restore from the backup
/// `backup` (or a later one) can need, and sync the directories it removed
/// files from. With `dry_run`, remove nothing and report what would go.
///
/// For each namespace the backup holds, with `C` its oldest checkpoint in
/// the backup: an archived segment is removed only if the next archived
/// segment starts at or before `C + 1`, so every record after `C` stays
/// (and the last segment always does); an archived checkpoint only if its
/// seq is below `C`. With no checkpoint in the backup (`C = 0`), nothing
/// is removed. Namespaces the backup doesn't hold are left alone. A
/// restore from the backup and the archive reaches every seq it reached
/// before.
///
/// Files are removed oldest first: an interruption leaves a contiguous
/// suffix of each namespace's segments, again a valid archive. Takes a
/// shared lock on the backup and no lock on the archive: a store can keep
/// archiving into it (it only adds newer segments).
///
/// Errors: [`Error::NotAnArchive`] (no marker, or a damaged one);
/// [`Error::NotADataDir`] (the backup has no marker or no manifest);
/// [`Error::InvalidManifest`]; [`Error::HistoryMismatch`] (another
/// history, or a layout 1 backup); [`Error::Locked`]; [`Error::Io`].
pub fn prune<F: LogFs>(fs: &F, archive: &Path, backup: &Path, dry_run: bool) -> Result<PruneReport, Error> {
    let Some((version, history)) = read_archive_marker_info(archive)? else {
        return Err(Error::NotAnArchive {
            path: archive.to_path_buf(),
            reason: format!("it has no '{}' marker", ARCHIVE_MARKER_NAME),
        });
    };
    let _lock = layout::lock_shared(backup)?;
    let marker = layout::read_marker(backup)?;
    let manifest_path = backup.join(BACKUP_NAME);
    if marker.is_none() || !manifest_path.is_file() {
        return Err(Error::NotADataDir { path: backup.to_path_buf(), reason: "it is not a complete backup".into() });
    }
    let manifest = read_manifest(&manifest_path)?;
    let backup_history: Option<HistoryId> = marker.and_then(|m| m.history);
    if backup_history != Some(history) || manifest.history != history {
        return Err(Error::HistoryMismatch {
            backup: backup_history.map_or("unknown (layout 1)".to_owned(), |h| h.to_string()),
            archive: history.to_string(),
        });
    }
    let mut report = PruneReport {
        archive: archive.to_path_buf(),
        backup: backup.to_path_buf(),
        dry_run,
        namespaces: Vec::new(),
        untouched: Vec::new(),
        bytes: 0,
    };
    let mut ids = if version >= 2 { archive_namespace_ids(archive)? } else { vec![DEFAULT_ID] };
    ids.sort_unstable();
    for id in ids {
        let Some(ns) = manifest.namespace(id) else {
            report.untouched.push(id);
            continue;
        };
        let cutoff = oldest_checkpoint(backup, &manifest, id);
        let segments = archive_segments(archive, version, id)?;
        let checkpoints = archive_checkpoints(archive, version, id)?;
        let mut remove: Vec<(u64, PathBuf, bool)> = Vec::new();
        if cutoff > 0 {
            for pair in segments.windows(2) {
                if let [(first, path), (next, _)] = pair
                    && *next <= cutoff + 1
                {
                    remove.push((*first, path.clone(), true));
                } else {
                    break;
                }
            }
            for (seq, path) in &checkpoints {
                if *seq < cutoff {
                    remove.push((*seq, path.clone(), false));
                }
            }
        }
        let mut pruned = NamespacePrune {
            id,
            name: ns.name.to_string(),
            backup_checkpoint: cutoff,
            kept_segments: segments.len() - remove.iter().filter(|r| r.2).count(),
            ..NamespacePrune::default()
        };
        // Oldest first: an interruption leaves a contiguous suffix
        remove.sort_by_key(|(seq, _, _)| *seq);
        for (seq, path, segment) in &remove {
            report.bytes += std::fs::metadata(path).map_err(|e| Error::io("stat", path, e))?.len();
            if !dry_run {
                fs.remove_file(path).map_err(|e| Error::io("remove", path, e))?;
            }
            if *segment { pruned.removed_segments.push(*seq) } else { pruned.removed_checkpoints.push(*seq) }
        }
        if !dry_run && !remove.is_empty() {
            let dir = if version >= 2 { archive.join(NS_DIR).join(ns_dir_name(id)) } else { archive.to_path_buf() };
            sync_dir(fs, &dir)?;
        }
        report.namespaces.push(pruned);
    }
    Ok(report)
}
