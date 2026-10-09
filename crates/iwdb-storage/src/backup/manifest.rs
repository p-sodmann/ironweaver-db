//! The backup manifest (`BACKUP`): what a backup holds, with every file's
//! length and CRC32C (`documentation/formats/backup.md`).

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;
use serde::{Deserialize, Serialize};

use crate::history::HistoryId;
use crate::io::CHUNK;
use crate::layout::{CHECKPOINT_DIR, WAL_DIR};
use crate::namespaces::{DEFAULT_ID, DEFAULT_NAME, NAMESPACES_NAME, NS_DIR, ns_dir_name, parse_ns_dir_name};
use crate::verify::Finding;
use crate::{Error, format};
use iwdb_engine::CommitTime;

/// The first 8 bytes of a manifest.
pub const MANIFEST_MAGIC: [u8; 8] = *b"IWDBBAK\n";
/// The manifest format this version writes. Version 1 (a single
/// namespace, `seq` and `time` at the top) is read too.
pub const MANIFEST_VERSION: u32 = 2;
/// The largest manifest document read (it lists a few files per
/// checkpoint and segment): 64 MiB.
pub const MAX_MANIFEST_LEN: u32 = 64 << 20;

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
        if ids.array_windows().any(|[a, b]| a == b) || ids.first() == Some(&0) {
            return Err("the manifest lists a namespace id twice, or id 0".into());
        }
        for file in &files {
            if let Some(id) = file_namespace(&file.path)
                && !ids.contains(&id)
            {
                return Err(format!("the manifest lists '{}', but no namespace {}", file.path, id));
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
        // One component at a time: a verbatim Windows root (`\\?\D:\...`)
        // doesn't read `/` as a separator (ADR 0058)
        let path = file.path.split('/').fold(root.to_path_buf(), |path, part| path.join(part));
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
