//! A quick, read-only look at a data directory, a backup or a WAL archive:
//! what `iwctl status` shows from the files alone (`verify` reads
//! everything; this reads the markers, the manifest, the file names and
//! the last segment).

use std::fs;
use std::path::{Path, PathBuf};

use crate::archive::{read_archive_marker, ARCHIVE_MARKER_NAME, ARCHIVE_VERSION};
use crate::backup::{read_manifest, Manifest};
use crate::checkpoint::list_checkpoints;
use crate::history::HistoryId;
use crate::layout::{self, BACKUP_NAME, CHECKPOINT_DIR, TEMP_SUFFIX, WAL_DIR};
use crate::time::CommitTime;
use crate::verify::Kind;
use crate::{reader, Error};

/// What the files of a directory say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirStatus {
    pub path: PathBuf,
    pub kind: Kind,
    /// The layout version (data directory, backup) or archive version.
    pub version: Option<u32>,
    pub history: Option<HistoryId>,
    /// A store has the directory open (its lock is held): what follows may
    /// be changing.
    pub in_use: bool,
    /// The checkpoints' seqs (none in an archive).
    pub checkpoints: Vec<u64>,
    /// The number of WAL segments, and the first one's first seq.
    pub segments: usize,
    pub first_segment: Option<u64>,
    /// The seq of the last record in the WAL (`None` without a segment):
    /// the last record of the last segment, or for a segment without
    /// records, the seq before its first one. Its commit time, if read, and
    /// whether the last segment has a torn tail (read as recovery would,
    /// without cutting it).
    pub last_seq: Option<u64>,
    pub last_time: Option<CommitTime>,
    pub torn_tail: bool,
    /// A backup's manifest.
    pub manifest: Option<Manifest>,
    /// Temporary files (an interrupted write).
    pub temp_files: usize,
}

/// Look at the directory `dir` without changing anything or taking a
/// lock. Errors: [`Error::NotADataDir`] for a directory that is neither a
/// data directory, a backup nor an archive; marker and manifest errors;
/// [`Error::Io`]. A damaged last segment is reported as an error.
pub fn inspect(dir: &Path) -> Result<DirStatus, Error> {
    if !dir.is_dir() {
        return Err(Error::NotADataDir { path: dir.to_path_buf(), reason: "it is not a directory".into() });
    }
    let mut status = DirStatus {
        path: dir.to_path_buf(),
        kind: Kind::DataDir,
        version: None,
        history: None,
        in_use: false,
        checkpoints: Vec::new(),
        segments: 0,
        first_segment: None,
        last_seq: None,
        last_time: None,
        torn_tail: false,
        manifest: None,
        temp_files: 0,
    };
    let wal = if dir.join(ARCHIVE_MARKER_NAME).exists() {
        status.kind = Kind::Archive;
        status.version = Some(ARCHIVE_VERSION);
        status.history = read_archive_marker(dir)?;
        dir.to_path_buf()
    } else {
        let Some(marker) = layout::read_marker(dir)? else {
            return Err(Error::NotADataDir {
                path: dir.to_path_buf(),
                reason: "it has no marker file (an interrupted initialization, backup or restore?)".into(),
            });
        };
        status.version = Some(marker.version);
        status.history = marker.history;
        status.in_use = matches!(layout::lock_shared(dir), Err(Error::Locked { .. }));
        if dir.join(BACKUP_NAME).exists() {
            status.kind = Kind::Backup;
            status.manifest = Some(read_manifest(&dir.join(BACKUP_NAME))?);
        }
        status.checkpoints = list_checkpoints(&dir.join(CHECKPOINT_DIR))?.into_iter().map(|(s, _)| s).collect();
        dir.join(WAL_DIR)
    };
    let segments = reader::list_segments(&wal)?;
    status.segments = segments.len();
    status.first_segment = segments.first().map(|(s, _)| *s);
    if let Some((first, path)) = segments.last() {
        let end = reader::read_segment_file(path, *first, true)?;
        status.last_seq = Some(end.next_seq - 1).filter(|s| *s > 0);
        status.last_time = end.last_time;
        status.torn_tail = end.torn.is_some();
    }
    let dirs = match status.kind {
        Kind::Archive => vec![wal],
        _ => vec![dir.to_path_buf(), dir.join(CHECKPOINT_DIR), wal],
    };
    for sub in dirs {
        if let Ok(entries) = fs::read_dir(&sub) {
            let temp = entries.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(TEMP_SUFFIX));
            status.temp_files += temp.count();
        }
    }
    Ok(status)
}
