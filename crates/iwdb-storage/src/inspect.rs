//! A quick, read-only look at a data directory, a backup or a WAL archive:
//! what `iwctl status` shows from the files alone (`verify` reads
//! everything; this reads the markers, the manifest, the file names and
//! the last segment).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::archive::{archive_namespace_ids, archive_segments, read_archive_marker_info, ARCHIVE_MARKER_NAME};
use crate::backup::{read_manifest, Manifest};
use crate::checkpoint::list_checkpoints;
use crate::history::HistoryId;
use crate::layout::{self, NsPaths, BACKUP_NAME, TEMP_SUFFIX};
use crate::namespaces::{ns_dir_name, read_log, DEFAULT_ID, DEFAULT_NAME, NAMESPACES_NAME, NS_DIR};
use crate::verify::Kind;
use crate::{reader, Error};
use iwdb_engine::CommitTime;

/// What the files of a directory say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirStatus {
    pub path: PathBuf,
    pub kind: Kind,
    /// The layout version of a data directory or backup, the format
    /// version of an archive.
    pub version: Option<u32>,
    pub history: Option<HistoryId>,
    /// A store has the data directory open (its lock is held).
    pub in_use: bool,
    /// The namespaces, by id: from the namespace log (the live ones; for
    /// an archive, the directories it has). One for layouts 1 to 3.
    pub namespaces: Vec<NamespaceFiles>,
    /// The number of events in the namespace log (layout 4), if it has one.
    pub namespace_events: Option<usize>,
    /// A backup's manifest.
    pub manifest: Option<Manifest>,
    /// Temporary files left by an interrupted write.
    pub temp_files: usize,
}

/// What the files of one namespace say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamespaceFiles {
    pub id: u64,
    /// `None` for an archive without a namespace log.
    pub name: Option<String>,
    /// The seqs of the checkpoints (a data directory or backup).
    pub checkpoints: Vec<u64>,
    pub segments: usize,
    pub first_segment: Option<u64>,
    /// The last record in the WAL (or archive), from the last segment.
    pub last_seq: Option<u64>,
    pub last_time: Option<CommitTime>,
    /// The last segment has a torn tail (recovery cuts it).
    pub torn_tail: bool,
}

/// Look at the data directory, backup or archive `dir` without changing
/// anything or taking its lock (it only *tries* a shared lock, to tell
/// whether a store has it open).
///
/// Errors: [`Error::NotADataDir`] (not a directory, no marker),
/// [`Error::UnsupportedLayout`], [`Error::InvalidDataDir`] (a damaged
/// marker), [`Error::NotAnArchive`], [`Error::InvalidManifest`],
/// [`Error::InvalidNamespaceLog`], [`Error::Io`].
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
        namespaces: Vec::new(),
        namespace_events: None,
        manifest: None,
        temp_files: 0,
    };
    let mut temp_dirs = vec![dir.to_path_buf()];
    if dir.join(ARCHIVE_MARKER_NAME).exists() {
        status.kind = Kind::Archive;
        let (version, history) = read_archive_marker_info(dir)?
            .ok_or_else(|| Error::NotAnArchive { path: dir.to_path_buf(), reason: "it has no marker".into() })?;
        status.version = Some(version);
        status.history = Some(history);
        let names = archive_names(dir, version)?;
        status.namespace_events = names.1;
        let ids: Vec<u64> = if version >= 2 { archive_namespace_ids(dir)? } else { vec![DEFAULT_ID] };
        for id in ids {
            let segments = archive_segments(dir, version, id)?;
            let name = names.0.get(&id).cloned();
            status.namespaces.push(files_of(id, name, Vec::new(), &segments)?);
            if version >= 2 {
                temp_dirs.push(dir.join(NS_DIR).join(ns_dir_name(id)));
            }
        }
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
        let listed: Vec<(u64, String, NsPaths)> = if marker.version >= 4 {
            let (_, table) = read_log(&dir.join(NAMESPACES_NAME))?;
            status.namespace_events = Some(table.events().len());
            temp_dirs.push(dir.join(NS_DIR));
            match &status.manifest {
                Some(m) => m.namespaces.iter().map(|n| (n.id, n.name.to_string(), NsPaths::new(dir, n.id))).collect(),
                None => table.live().map(|n| (n.id, n.name.to_string(), NsPaths::new(dir, n.id))).collect(),
            }
        } else {
            vec![(DEFAULT_ID, DEFAULT_NAME.to_owned(), NsPaths::legacy(dir))]
        };
        for (id, name, paths) in listed {
            let checkpoints = if paths.checkpoints.is_dir() {
                list_checkpoints(&paths.checkpoints)?.into_iter().map(|(s, _)| s).collect()
            } else {
                Vec::new()
            };
            let segments = if paths.wal.is_dir() { reader::list_segments(&paths.wal)? } else { Vec::new() };
            status.namespaces.push(files_of(id, Some(name), checkpoints, &segments)?);
            temp_dirs.push(paths.checkpoints);
            temp_dirs.push(paths.wal);
        }
    }
    for sub in temp_dirs {
        if let Ok(entries) = fs::read_dir(&sub) {
            let temp = entries.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(TEMP_SUFFIX));
            status.temp_files += temp.count();
        }
    }
    Ok(status)
}

fn files_of(
    id: u64,
    name: Option<String>,
    checkpoints: Vec<u64>,
    segments: &[(u64, PathBuf)],
) -> Result<NamespaceFiles, Error> {
    let mut files = NamespaceFiles {
        id,
        name,
        checkpoints,
        segments: segments.len(),
        first_segment: segments.first().map(|(s, _)| *s),
        last_seq: None,
        last_time: None,
        torn_tail: false,
    };
    if let Some((first, path)) = segments.last() {
        let end = reader::read_segment_file(path, *first, true)?;
        files.last_seq = Some(end.next_seq - 1).filter(|s| *s > 0);
        files.last_time = end.last_time;
        files.torn_tail = end.torn.is_some();
    }
    Ok(files)
}

/// The names of an archive's namespaces, from its namespace log copy, and
/// the log's length.
fn archive_names(dir: &Path, version: u32) -> Result<(BTreeMap<u64, String>, Option<usize>), Error> {
    let mut names = BTreeMap::new();
    if version < 2 {
        names.insert(DEFAULT_ID, DEFAULT_NAME.to_owned());
        return Ok((names, None));
    }
    let path = dir.join(NAMESPACES_NAME);
    if !path.exists() {
        return Ok((names, None));
    }
    let (_, table) = read_log(&path)?;
    for event in table.events() {
        if event.kind == crate::namespaces::EventKind::Create {
            names.insert(event.id, event.name.to_string());
        }
    }
    Ok((names, Some(table.events().len())))
}

crate::default_deref!(DirStatus, NamespaceFiles, namespaces, |n| n.name.as_deref().unwrap_or(""));
