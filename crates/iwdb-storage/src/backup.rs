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
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;
use serde::{Deserialize, Serialize};

use crate::checkpoint::{checkpoint_name, list_checkpoints};
use crate::history::HistoryId;
use crate::io::{LogFile, LogFs};
use crate::layout::{encode_marker, NsPaths, BACKUP_NAME, CHECKPOINT_DIR, MARKER_NAME, WAL_DIR};
use crate::namespaces::{ns_dir_name, parse_ns_dir_name, DEFAULT_ID, DEFAULT_NAME, NAMESPACES_NAME, NS_DIR};
use crate::verify::Finding;
use crate::{format, reader, Error};
use iwdb_engine::CommitTime;

/// The first 8 bytes of a manifest.
pub const MANIFEST_MAGIC: [u8; 8] = *b"IWDBBAK\n";
/// The manifest format this version writes. Version 1 (a single
/// namespace, `seq` and `time` at the top) is read too.
pub const MANIFEST_VERSION: u32 = 2;
/// The largest manifest document read (it lists a few files per
/// checkpoint and segment): 64 MiB.
pub const MAX_MANIFEST_LEN: u32 = 64 << 20;
/// Files are copied in chunks of this size.
const CHUNK: usize = 1 << 20;

/// A namespace of a backup, as its manifest lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestNamespace {
    pub id: u64,
    pub name: NamespaceName,
    /// The seq the backup reaches in this namespace: its WAL ends with this
    /// record (or, without WAL, its newest checkpoint is at it).
    pub seq: u64,
    /// The commit time of record `seq`, if the backup holds it (WAL format
    /// 2 or later).
    pub time: Option<CommitTime>,
}

/// A backup's manifest (`BACKUP`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The manifest's format version (1 or 2).
    pub version: u32,
    /// The history of the store it was taken from.
    pub history: HistoryId,
    /// The namespaces it holds, by id.
    pub namespaces: Vec<ManifestNamespace>,
    /// When the backup was taken.
    pub created: CommitTime,
    /// The source data directory, as given to the backup.
    pub source: String,
    /// Every file of the backup apart from `IWDB` and `BACKUP`, sorted by
    /// path.
    pub files: Vec<ManifestFile>,
}

impl Manifest {
    /// The namespace `id` of the backup.
    pub fn namespace(&self, id: u64) -> Option<&ManifestNamespace> {
        self.namespaces.iter().find(|n| n.id == id)
    }

    /// Whether this is a version 1 manifest: one namespace, whose files
    /// are at the top of the backup.
    pub fn is_legacy(&self) -> bool {
        self.version < 2
    }
}

/// A file listed in a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    /// Relative to the backup, with `/` separators:
    /// `ns/<id>/checkpoints/<name>`, `ns/<id>/wal/<name>` or `NAMESPACES`
    /// (`checkpoints/<name>` and `wal/<name>` in a version 1 manifest).
    pub path: String,
    pub len: u64,
    pub crc32c: u32,
}

/// The manifest's document, as stored (version 2).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    history: String,
    created: i64,
    source: String,
    namespaces: Vec<ManifestNamespace>,
    files: Vec<ManifestFile>,
}

/// The document of version 1.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentV1 {
    history: String,
    seq: u64,
    time: Option<i64>,
    created: i64,
    source: String,
    files: Vec<ManifestFile>,
}

impl Manifest {
    /// The manifest's bytes: magic, version (u32 LE), document length (u32
    /// LE), the JSON document, and a CRC32C of everything before it.
    pub fn encode(&self) -> Vec<u8> {
        let doc = Document {
            history: self.history.to_string(),
            created: self.created.0,
            source: self.source.clone(),
            namespaces: self.namespaces.clone(),
            files: self.files.clone(),
        };
        // Structs, strings and numbers always serialize
        let json = serde_json::to_vec(&doc).unwrap_or_default();
        let mut out = MANIFEST_MAGIC.to_vec();
        out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(&json);
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decode a manifest (version 1 or 2); an error message if it isn't a
    /// valid one.
    pub fn decode(bytes: &[u8]) -> Result<Manifest, String> {
        if !bytes.starts_with(&MANIFEST_MAGIC) {
            return Err("not a backup manifest (wrong magic)".into());
        }
        if bytes.len() < 20 {
            return Err("the manifest is truncated".into());
        }
        let (data, crc) = bytes.split_at(bytes.len() - 4);
        if crc32c::crc32c(data).to_le_bytes() != crc {
            return Err("the manifest's checksum doesn't match".into());
        }
        let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if !(1..=MANIFEST_VERSION).contains(&version) {
            return Err(format!("manifest version {} (this version reads 1 to {})", version, MANIFEST_VERSION));
        }
        let len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if len > MAX_MANIFEST_LEN || data.len() != 16 + len as usize {
            return Err(format!("the manifest's length field ({}) doesn't match its size", len));
        }
        let invalid = |e: serde_json::Error| format!("invalid manifest: {}", e);
        let (history, created, source, namespaces, files) = if version == 1 {
            let doc: DocumentV1 = serde_json::from_slice(&data[16..]).map_err(invalid)?;
            let name = NamespaceName::new(DEFAULT_NAME).map_err(|e| e.to_string())?;
            let namespace = ManifestNamespace { id: DEFAULT_ID, name, seq: doc.seq, time: doc.time.map(CommitTime) };
            (doc.history, doc.created, doc.source, vec![namespace], doc.files)
        } else {
            let doc: Document = serde_json::from_slice(&data[16..]).map_err(invalid)?;
            (doc.history, doc.created, doc.source, doc.namespaces, doc.files)
        };
        let history = history.parse()?;
        for file in &files {
            if !valid_path(&file.path, version == 1) {
                return Err(format!("the manifest lists an invalid path '{}'", file.path));
            }
        }
        let mut ids: Vec<u64> = namespaces.iter().map(|n| n.id).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) || ids.first() == Some(&0) {
            return Err("the manifest lists a namespace id twice, or id 0".into());
        }
        for file in &files {
            if let Some(id) = file_namespace(&file.path) {
                if !ids.contains(&id) {
                    return Err(format!("the manifest lists '{}', but no namespace {}", file.path, id));
                }
            }
        }
        Ok(Manifest { version, history, namespaces, created: CommitTime(created), source, files })
    }
}

/// The namespace id of a version 2 file path (`ns/<id>/...`).
fn file_namespace(path: &str) -> Option<u64> {
    let rest = path.strip_prefix("ns/")?;
    parse_ns_dir_name(rest.split('/').next()?)
}

/// Paths a manifest may list: a checkpoint or a segment name in its
/// directory (under `ns/<id>/`, or at the top in version 1), and the
/// namespace log; nothing that leaves the backup.
fn valid_path(path: &str, legacy: bool) -> bool {
    let in_dir = |dir: &str, name: &str| match dir {
        CHECKPOINT_DIR => crate::checkpoint::parse_checkpoint_name(name).is_some(),
        WAL_DIR => format::parse_segment_name(name).is_some(),
        _ => false,
    };
    let parts: Vec<&str> = path.split('/').collect();
    match (legacy, parts.as_slice()) {
        (true, [dir, name]) => in_dir(dir, name),
        (false, [NAMESPACES_NAME]) => true,
        (false, [NS_DIR, id, dir, name]) => parse_ns_dir_name(id).is_some() && in_dir(dir, name),
        _ => false,
    }
}

/// Read and decode the manifest file at `path`.
pub fn read_manifest(path: &Path) -> Result<Manifest, Error> {
    let len = fs::metadata(path).map_err(|e| Error::io("stat", path, e))?.len();
    if len > u64::from(MAX_MANIFEST_LEN) + 20 {
        return Err(Error::InvalidManifest { path: path.to_path_buf(), reason: format!("{} bytes is too large", len) });
    }
    let bytes = fs::read(path).map_err(|e| Error::io("read", path, e))?;
    Manifest::decode(&bytes).map_err(|reason| Error::InvalidManifest { path: path.to_path_buf(), reason })
}

/// Check a backup's files against its manifest: every listed file present
/// with its length and CRC32C, and no other file in a namespace's
/// `checkpoints/` or `wal/` (or `ns/`). Returns the problems found.
pub fn check_files(root: &Path, manifest: &Manifest) -> Vec<Finding> {
    let mut problems = Vec::new();
    let problem = |path: &Path, message: String| Finding { path: Some(path.to_path_buf()), message };
    for file in &manifest.files {
        let path = root.join(&file.path);
        match crc_of(&path) {
            Ok((len, _)) if len != file.len => problems.push(problem(
                &path,
                format!("has {} bytes, the manifest says {} (truncated or replaced)", len, file.len),
            )),
            Ok((_, crc)) if crc != file.crc32c => {
                problems.push(problem(&path, "its CRC32C differs from the manifest's (damaged)".into()))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                problems.push(problem(&path, "is listed in the manifest but missing".into()))
            }
            Err(e) => problems.push(problem(&path, format!("can't be read: {}", e))),
        }
    }
    let listed: BTreeSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    if manifest.is_legacy() {
        for sub in [CHECKPOINT_DIR, WAL_DIR] {
            dirs.push((sub.to_owned(), root.join(sub)));
        }
    } else {
        // Namespace directories the manifest doesn't know
        if let Ok(entries) = fs::read_dir(root.join(NS_DIR)) {
            for entry in entries.flatten() {
                let known = parse_ns_dir_name(&entry.file_name().to_string_lossy())
                    .is_some_and(|id| manifest.namespace(id).is_some());
                if !known {
                    problems.push(problem(&entry.path(), "is not a namespace of the backup's manifest".into()));
                }
            }
        }
        for ns in &manifest.namespaces {
            for sub in [CHECKPOINT_DIR, WAL_DIR] {
                dirs.push((
                    format!("{}/{}/{}", NS_DIR, ns_dir_name(ns.id), sub),
                    root.join(NS_DIR).join(ns_dir_name(ns.id)).join(sub),
                ));
            }
        }
    }
    for (prefix, dir) in dirs {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = format!("{}/{}", prefix, entry.file_name().to_string_lossy());
            if !listed.contains(name.as_str()) {
                problems.push(problem(&entry.path(), "is not listed in the backup's manifest".into()));
            }
        }
    }
    problems
}

/// The length and CRC32C of a file, read in chunks.
fn crc_of(path: &Path) -> io::Result<(u64, u32)> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; CHUNK];
    let (mut len, mut crc) = (0u64, 0u32);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok((len, crc));
        }
        crc = crc32c::crc32c_append(crc, &buf[..n]);
        len += n as u64;
    }
}

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
            return Err(Error::DestinationNotEmpty { path: dest.to_path_buf() })
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

/// Create `dir` (and its parents) if missing, and sync its parent.
pub(crate) fn create_dir<F: LogFs>(fs: &F, dir: &Path) -> Result<(), Error> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|e| Error::io("create directory", dir, e))?;
    match dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => sync_dir(fs, parent),
        None => Ok(()),
    }
}

pub(crate) fn sync_dir<F: LogFs>(fs: &F, dir: &Path) -> Result<(), Error> {
    fs.sync_dir(dir).map_err(|e| Error::io("sync directory", dir, e))
}

pub(crate) fn write_atomic<F: LogFs>(fs: &F, path: &Path, bytes: &[u8]) -> Result<(), Error> {
    fs.write_atomic(path, &mut |out| out.write_all(bytes)).map_err(|e| Error::io("write", path, e))
}

/// Create `target` with `content`, in chunks, and fsync it.
pub(crate) fn write_file<F: LogFs>(fs: &F, target: &Path, content: &[u8]) -> Result<(), Error> {
    let mut file = fs.create(target).map_err(|e| Error::io("create", target, e))?;
    for chunk in content.chunks(CHUNK) {
        file.write_all(chunk).map_err(|e| Error::io("write", target, e))?;
    }
    file.sync().map_err(|e| Error::io("fsync", target, e))
}

/// Copy `source` to a new file `target` in chunks, and fsync it. Returns
/// its length and CRC32C.
fn copy_file<F: LogFs>(fs: &F, source: &Path, target: &Path) -> Result<(u64, u32), Error> {
    let mut input = File::open(source).map_err(|e| Error::io("open", source, e))?;
    let mut file = fs.create(target).map_err(|e| Error::io("create", target, e))?;
    let mut buf = vec![0u8; CHUNK];
    let (mut len, mut crc) = (0u64, 0u32);
    loop {
        let n = input.read(&mut buf).map_err(|e| Error::io("read", source, e))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| Error::io("write", target, e))?;
        crc = crc32c::crc32c_append(crc, &buf[..n]);
        len += n as u64;
    }
    file.sync().map_err(|e| Error::io("fsync", target, e))?;
    Ok((len, crc))
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

/// Single-namespace convenience: field access to the `default` namespace's
/// part of a report (an empty one if there is none).
macro_rules! default_deref {
    ($report:ty, $part:ty, $field:ident, |$n:ident| $name:expr) => {
        impl std::ops::Deref for $report {
            type Target = $part;
            fn deref(&self) -> &$part {
                static EMPTY: std::sync::OnceLock<$part> = std::sync::OnceLock::new();
                self.$field
                    .iter()
                    .find(|$n| $name == DEFAULT_NAME)
                    .unwrap_or_else(|| EMPTY.get_or_init(Default::default))
            }
        }
    };
}
pub(crate) use default_deref;

default_deref!(BackupReport, NamespaceBackup, namespaces, |n| n.name.as_str());
