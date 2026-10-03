//! Backups: a consistent copy of a data directory, namespace by namespace,
//! each up to a synced seq, with a manifest
//! (`documentation/formats/backup.md`, ADR 0009, ADR 0017).
//!
//! ```text
//! <backup>/
//!   IWDB           the source's marker: layout 4, the source's history id
//!   BACKUP         the manifest: history, time, every namespace's seq, every file with its length and CRC32C
//!   NAMESPACES     the source's namespace log, up to the moment of the backup
//!   ns/<id>/
//!     checkpoints/   the namespace's checkpoints at or below its seq
//!     wal/           the segments from the oldest checkpoint's seq + 1 to the seq,
//!                    the last one cut right after the record at the seq
//! ```
//!
//! A backup of a layout 1 to 3 directory (manifest version 1) has its one
//! namespace's `checkpoints/` and `wal/` at the top, and no `NAMESPACES`.

use std::collections::BTreeSet;
use std::fs::{self};
use std::io;
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;

use crate::checkpoint::{checkpoint_name, list_checkpoints};
use crate::history::HistoryId;
use crate::io::{LogFs, copy_file, create_dir, sync_dir, write_atomic, write_file};
use crate::layout::{BACKUP_NAME, CHECKPOINT_DIR, MARKER_NAME, NsPaths, WAL_DIR, encode_marker};
use crate::namespaces::{NAMESPACES_NAME, NS_DIR, ns_dir_name};
use crate::{Error, format, reader};
use iwdb_engine::CommitTime;

mod manifest;

pub use manifest::*;

/// What a backup wrote for one namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamespaceBackup {
    pub id: u64,
    pub name: String,
    /// The seq the backup reaches in this namespace (see
    /// [`ManifestNamespace::seq`]).
    pub seq: u64,
    /// The commit time of that record, if known.
    pub time: Option<CommitTime>,
    /// The seqs of the checkpoints copied.
    pub checkpoints: Vec<u64>,
    /// The first seqs of the WAL segments copied.
    pub segments: Vec<u64>,
}

/// What a backup wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupReport {
    pub path: PathBuf,
    pub history: HistoryId,
    /// Each namespace, by id.
    pub namespaces: Vec<NamespaceBackup>,
    /// Bytes copied.
    pub bytes: u64,
}

impl BackupReport {
    /// The namespace `name`.
    pub fn namespace(&self, name: &str) -> Option<&NamespaceBackup> {
        self.namespaces.iter().find(|n| n.name == name)
    }
}

/// What a backup of a namespace up to `seq` copies.
struct Plan {
    checkpoints: Vec<(u64, PathBuf)>,
    /// `(first seq, path, last record to copy)`.
    segments: Vec<(u64, PathBuf, u64)>,
}

/// Choose the checkpoints at or below `seq` not known to be damaged, and
/// the segments from the oldest one's seq + 1 through `seq`.
fn plan(source: &NsPaths, seq: u64, damaged: &BTreeSet<u64>) -> Result<Plan, Error> {
    let checkpoints: Vec<(u64, PathBuf)> =
        list_checkpoints(&source.checkpoints)?.into_iter().filter(|(s, _)| *s <= seq && !damaged.contains(s)).collect();
    let base = checkpoints.first().map_or(0, |(s, _)| *s);
    let all = reader::list_segments(&source.wal)?;
    let mut segments = Vec::new();
    if seq > base {
        // From the last segment that starts at or before base + 1
        let start = all.partition_point(|(first, _)| *first <= base + 1);
        if start == 0 {
            let first_seq = all.first().map_or(seq + 1, |(first, _)| *first);
            return Err(Error::MissingRecords { from: base + 1, first_seq });
        }
        for (i, (first, path)) in all.iter().enumerate().skip(start - 1) {
            if *first > seq {
                break;
            }
            let next = all.get(i + 1).map(|(next, _)| *next);
            let last = next.map_or(seq, |next| (next - 1).min(seq));
            segments.push((*first, path.clone(), last));
        }
    }
    Ok(Plan { checkpoints, segments })
}

/// A namespace to back up: where its files are, and what to copy.
#[derive(Clone, Debug)]
pub struct NamespaceSource<'a> {
    pub name: NamespaceName,
    pub paths: NsPaths,
    /// The seq to back up to: a synced seq.
    pub seq: u64,
    /// Checkpoints known not to load (not copied).
    pub damaged: &'a BTreeSet<u64>,
}

/// Write a backup of the data directory `source`, its namespaces
/// `namespaces` (each up to its seq) and the namespace log `log` (the
/// bytes of its file), into `dest`, through `fs`. `history` is the source's
/// history id.
///
/// The caller must keep the files it copies in place: the store holds every
/// namespace's checkpointer mutex, so that no checkpoint or segment is
/// removed meanwhile, holds the namespace log still (no namespace is
/// created or dropped), and makes sure that records up to each seq are
/// synced (a backup never holds a record the source could still lose).
/// Records after a seq may be appended meanwhile; they are never read.
///
/// `dest` must be missing or an empty directory, and not inside `source`.
/// The order is initialization's: an empty `BACKUP` file (fsynced) and the
/// directories, synced; every file (written in chunks and fsynced), their
/// directories synced; the manifest (`write_atomic`, replacing the empty
/// one) and a directory sync; then the marker last (`write_atomic`) and a
/// directory sync. So until the marker is in place, the directory holds
/// `BACKUP` but no marker, which a store, verify and restore all refuse
/// (an empty directory, if it failed before that, is what it was). A
/// failed backup leaves such a directory behind: remove it and try again.
///
/// Every WAL segment copied is read and checked frame by frame (like the
/// reader does; [`reader::segment_prefix`]); the checkpoints are copied as
/// they are. Errors: [`Error::DestinationNotEmpty`], [`Error::Io`], any
/// reader error for a damaged segment, [`Error::MissingRecords`] if a WAL
/// doesn't reach back to the oldest checkpoint at or below its seq.
pub fn write_backup<F: LogFs>(
    fs: &F,
    source: &Path,
    history: HistoryId,
    namespaces: &[NamespaceSource<'_>],
    log: &[u8],
    dest: &Path,
) -> Result<BackupReport, Error> {
    check_destination(source, dest)?;
    let mut plans = Vec::new();
    for ns in namespaces {
        plans.push(plan(&ns.paths, ns.seq, ns.damaged)?);
    }
    create_dir(fs, dest)?;
    // An empty manifest first, replaced by the real one at the end: until
    // the marker is written, the directory holds a file that no store
    // initializes a directory with
    write_file(fs, &dest.join(BACKUP_NAME), &[])?;
    let ns_root = dest.join(NS_DIR);
    fs.create_dir(&ns_root).map_err(|e| Error::io("create directory", &ns_root, e))?;
    let targets: Vec<NsPaths> = namespaces.iter().map(|ns| NsPaths::new(dest, ns.paths.id)).collect();
    for target in &targets {
        for dir in [&target.dir, &target.checkpoints, &target.wal] {
            fs.create_dir(dir).map_err(|e| Error::io("create directory", dir, e))?;
        }
    }
    sync_dir(fs, dest)?;

    let mut files = Vec::new();
    let mut bytes = 0;
    let mut reports = Vec::new();
    let mut manifest_namespaces = Vec::new();
    for ((ns, plan), target) in namespaces.iter().zip(&plans).zip(&targets) {
        let prefix = format!("{}/{}", NS_DIR, ns_dir_name(ns.paths.id));
        for (ckpt, path) in &plan.checkpoints {
            let name = checkpoint_name(*ckpt);
            let (len, crc) = copy_file(fs, path, &target.checkpoints.join(&name))?;
            files.push(ManifestFile { path: format!("{}/{}/{}", prefix, CHECKPOINT_DIR, name), len, crc32c: crc });
            bytes += len;
        }
        let mut time = None;
        for (first, path, last) in &plan.segments {
            let (content, last_time) = reader::segment_prefix(path, *first, *last)?;
            let name = format::segment_name(*first);
            write_file(fs, &target.wal.join(&name), &content)?;
            let crc = crc32c::crc32c(&content);
            files.push(ManifestFile {
                path: format!("{}/{}/{}", prefix, WAL_DIR, name),
                len: content.len() as u64,
                crc32c: crc,
            });
            bytes += content.len() as u64;
            if *last == ns.seq {
                time = last_time;
            }
        }
        manifest_namespaces.push(ManifestNamespace { id: ns.paths.id, name: ns.name.clone(), seq: ns.seq, time });
        reports.push(NamespaceBackup {
            id: ns.paths.id,
            name: ns.name.to_string(),
            seq: ns.seq,
            time,
            checkpoints: plan.checkpoints.iter().map(|(s, _)| *s).collect(),
            segments: plan.segments.iter().map(|(s, _, _)| *s).collect(),
        });
    }
    write_file(fs, &dest.join(NAMESPACES_NAME), log)?;
    files.push(ManifestFile { path: NAMESPACES_NAME.to_owned(), len: log.len() as u64, crc32c: crc32c::crc32c(log) });
    bytes += log.len() as u64;
    for target in &targets {
        sync_dir(fs, &target.checkpoints)?;
        sync_dir(fs, &target.wal)?;
        sync_dir(fs, &target.dir)?;
    }
    sync_dir(fs, &ns_root)?;

    files.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        history,
        namespaces: manifest_namespaces,
        created: CommitTime::now(),
        source: source.display().to_string(),
        files,
    };
    write_atomic(fs, &dest.join(BACKUP_NAME), &manifest.encode())?;
    sync_dir(fs, dest)?;
    write_atomic(fs, &dest.join(MARKER_NAME), &encode_marker(history))?;
    sync_dir(fs, dest)?;
    Ok(BackupReport { path: dest.to_path_buf(), history, namespaces: reports, bytes })
}

/// `dest` must be missing or empty, and not inside `source`.
pub(crate) fn check_destination(source: &Path, dest: &Path) -> Result<(), Error> {
    match fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::DestinationNotEmpty { path: dest.to_path_buf() });
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
            return Err(Error::DestinationNotEmpty { path: dest.to_path_buf() });
        }
        Err(e) => return Err(Error::io("list", dest, e)),
    }
    let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let source = fs::canonicalize(source).unwrap_or_else(|_| absolute(source));
    // The destination may not exist yet: resolve its parent
    let dest = match (dest.parent(), dest.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            fs::canonicalize(parent).map(|p| p.join(name)).unwrap_or_else(|_| absolute(dest))
        }
        _ => absolute(dest),
    };
    if dest.starts_with(&source) {
        return Err(Error::InvalidOptions(format!(
            "the destination '{}' is inside the data directory '{}'",
            dest.display(),
            source.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            history: HistoryId([3; 16]),
            namespaces: vec![
                ManifestNamespace {
                    id: 1,
                    name: NamespaceName::new("default").expect("name"),
                    seq: 42,
                    time: Some(CommitTime(1_759_312_800_000_000)),
                },
                ManifestNamespace { id: 4, name: NamespaceName::new("b").expect("name"), seq: 0, time: None },
            ],
            created: CommitTime(1_759_312_900_000_000),
            source: "/data/store".into(),
            files: vec![
                ManifestFile { path: "NAMESPACES".into(), len: 100, crc32c: 5 },
                ManifestFile {
                    path: "ns/00000000000000000001/checkpoints/00000000000000000030.ckpt".into(),
                    len: 1000,
                    crc32c: 7,
                },
                ManifestFile {
                    path: "ns/00000000000000000001/wal/00000000000000000031.wal".into(),
                    len: 500,
                    crc32c: 9,
                },
            ],
        }
    }

    #[test]
    fn manifests_round_trip_and_detect_damage() {
        let m = manifest();
        let bytes = m.encode();
        assert_eq!(&bytes[..8], b"IWDBBAK\n");
        assert_eq!(Manifest::decode(&bytes), Ok(m.clone()));
        for at in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[at] ^= 0x04;
            assert!(Manifest::decode(&bad).is_err(), "byte {}", at);
        }
        assert!(Manifest::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut escaping = m.clone();
        escaping.files[0].path = "../outside".into();
        assert!(Manifest::decode(&escaping.encode()).unwrap_err().contains("invalid path"));
        // Files of a namespace the manifest doesn't list, and the same id twice
        let mut stray = m.clone();
        stray.files[1].path = "ns/00000000000000000009/wal/00000000000000000031.wal".into();
        assert!(Manifest::decode(&stray.encode()).unwrap_err().contains("no namespace 9"));
        let mut twice = m;
        twice.namespaces[1].id = 1;
        assert!(Manifest::decode(&twice.encode()).unwrap_err().contains("twice"));
    }

    /// A manifest frame with a valid CRC around arbitrary bytes.
    fn framed(doc: &[u8]) -> Vec<u8> {
        let mut out = MANIFEST_MAGIC.to_vec();
        out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
        out.extend_from_slice(&(doc.len() as u32).to_le_bytes());
        out.extend_from_slice(doc);
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    proptest::proptest! {
        /// Arbitrary bytes, as a whole file or as the document inside a
        /// valid frame: an error, never a panic.
        #[test]
        fn arbitrary_manifests_never_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            let _ = Manifest::decode(&bytes);
            let _ = Manifest::decode(&framed(&bytes));
        }
    }
}

crate::default_deref!(BackupReport, NamespaceBackup, namespaces, |n| n.name.as_str());
