//! Backups: a consistent copy of a data directory up to a synced seq, with
//! a manifest (`documentation/formats/backup.md`, ADR 0009).
//!
//! ```text
//! <backup>/
//!   IWDB           the source's marker: layout 2, the source's history id
//!   BACKUP         the manifest: seq, history, time, every file with its length and CRC32C
//!   checkpoints/   the source's checkpoints at or below the seq
//!   wal/           the segments from the oldest checkpoint's seq + 1 to the seq,
//!                  the last one cut right after the record at the seq
//! ```

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::checkpoint::{checkpoint_name, list_checkpoints};
use crate::history::HistoryId;
use crate::io::{LogFile, LogFs};
use crate::layout::{encode_marker, BACKUP_NAME, CHECKPOINT_DIR, MARKER_NAME, WAL_DIR};
use crate::time::CommitTime;
use crate::verify::Finding;
use crate::{format, reader, Error};

/// The first 8 bytes of a manifest.
pub const MANIFEST_MAGIC: [u8; 8] = *b"IWDBBAK\n";
/// The manifest format this version writes and reads.
pub const MANIFEST_VERSION: u32 = 1;
/// The largest manifest document read (it lists a few files per
/// checkpoint and segment): 64 MiB.
pub const MAX_MANIFEST_LEN: u32 = 64 << 20;
/// Files are copied in chunks of this size.
const CHUNK: usize = 1 << 20;

/// A backup's manifest (`BACKUP`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The history of the store it was taken from.
    pub history: HistoryId,
    /// The seq it reaches: its WAL ends with this record (or, without WAL,
    /// its newest checkpoint is at it).
    pub seq: u64,
    /// The commit time of record `seq`, if the backup holds it (WAL format
    /// 2).
    pub time: Option<CommitTime>,
    /// When the backup was taken.
    pub created: CommitTime,
    /// The source data directory, as given to the backup.
    pub source: String,
    /// Every file of the backup apart from `IWDB` and `BACKUP`, sorted by
    /// path.
    pub files: Vec<ManifestFile>,
}

/// A file listed in a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    /// Relative to the backup, with `/` separators: `checkpoints/<name>` or
    /// `wal/<name>`.
    pub path: String,
    pub len: u64,
    pub crc32c: u32,
}

/// The manifest's document, as stored.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
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
            seq: self.seq,
            time: self.time.map(|t| t.0),
            created: self.created.0,
            source: self.source.clone(),
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

    /// Decode a manifest; an error message if it isn't a valid one.
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
        if version != MANIFEST_VERSION {
            return Err(format!("manifest version {} (this version reads {})", version, MANIFEST_VERSION));
        }
        let len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if len > MAX_MANIFEST_LEN || data.len() != 16 + len as usize {
            return Err(format!("the manifest's length field ({}) doesn't match its size", len));
        }
        let doc: Document = serde_json::from_slice(&data[16..]).map_err(|e| format!("invalid manifest: {}", e))?;
        let history = doc.history.parse()?;
        for file in &doc.files {
            if !valid_path(&file.path) {
                return Err(format!("the manifest lists an invalid path '{}'", file.path));
            }
        }
        Ok(Manifest {
            history,
            seq: doc.seq,
            time: doc.time.map(CommitTime),
            created: CommitTime(doc.created),
            source: doc.source,
            files: doc.files,
        })
    }
}

/// Paths a manifest may list: a checkpoint or a segment name in its
/// directory, nothing that leaves the backup.
fn valid_path(path: &str) -> bool {
    match path.split_once('/') {
        Some((CHECKPOINT_DIR, name)) => crate::checkpoint::parse_checkpoint_name(name).is_some(),
        Some((WAL_DIR, name)) => format::parse_segment_name(name).is_some(),
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
/// with its length and CRC32C, and no other file in `checkpoints/` or
/// `wal/`. Returns the problems found.
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
    for sub in [CHECKPOINT_DIR, WAL_DIR] {
        let Ok(entries) = fs::read_dir(root.join(sub)) else { continue };
        for entry in entries.flatten() {
            let name = format!("{}/{}", sub, entry.file_name().to_string_lossy());
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

/// What a backup wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupReport {
    pub path: PathBuf,
    /// The seq the backup reaches (see [`Manifest::seq`]).
    pub seq: u64,
    /// The commit time of that record, if known.
    pub time: Option<CommitTime>,
    pub history: HistoryId,
    /// The seqs of the checkpoints copied.
    pub checkpoints: Vec<u64>,
    /// The first seqs of the WAL segments copied.
    pub segments: Vec<u64>,
    /// Bytes copied.
    pub bytes: u64,
}

/// What a backup of `source` up to `seq` copies.
struct Plan {
    checkpoints: Vec<(u64, PathBuf)>,
    /// `(first seq, path, last record to copy)`.
    segments: Vec<(u64, PathBuf, u64)>,
}

/// Choose the checkpoints at or below `seq` not known to be damaged, and
/// the segments from the oldest one's seq + 1 through `seq`.
fn plan(source: &Path, seq: u64, damaged: &BTreeSet<u64>) -> Result<Plan, Error> {
    let checkpoints: Vec<(u64, PathBuf)> = list_checkpoints(&source.join(CHECKPOINT_DIR))?
        .into_iter()
        .filter(|(s, _)| *s <= seq && !damaged.contains(s))
        .collect();
    let base = checkpoints.first().map_or(0, |(s, _)| *s);
    let all = reader::list_segments(&source.join(WAL_DIR))?;
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

/// Write a backup of the data directory `source` at `seq` into `dest`,
/// through `fs`. `history` is the source's history id; `damaged` the
/// checkpoints known not to load (not copied).
///
/// The caller must keep the files it copies in place: the store holds its
/// checkpointer's mutex, so that no checkpoint or segment is removed
/// meanwhile, and makes sure that records up to `seq` are synced (a
/// backup never holds a record the source could still lose). Records after
/// `seq` may be appended meanwhile; they are never read.
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
/// reader error for a damaged segment, [`Error::MissingRecords`] if the
/// WAL doesn't reach back to the oldest checkpoint at or below `seq`.
pub fn write_backup<F: LogFs>(
    fs: &F,
    source: &Path,
    history: HistoryId,
    seq: u64,
    damaged: &BTreeSet<u64>,
    dest: &Path,
) -> Result<BackupReport, Error> {
    check_destination(source, dest)?;
    let plan = plan(source, seq, damaged)?;
    create_dir(fs, dest)?;
    // An empty manifest first, replaced by the real one at the end: until
    // the marker is written, the directory holds a file that no store
    // initializes a directory with
    write_file(fs, &dest.join(BACKUP_NAME), &[])?;
    for sub in [CHECKPOINT_DIR, WAL_DIR] {
        let path = dest.join(sub);
        fs::create_dir(&path).map_err(|e| Error::io("create directory", &path, e))?;
    }
    sync_dir(fs, dest)?;

    let mut files = Vec::new();
    let mut bytes = 0;
    for (ckpt, path) in &plan.checkpoints {
        let name = checkpoint_name(*ckpt);
        let target = dest.join(CHECKPOINT_DIR).join(&name);
        let (len, crc) = copy_file(fs, path, &target)?;
        files.push(ManifestFile { path: format!("{}/{}", CHECKPOINT_DIR, name), len, crc32c: crc });
        bytes += len;
    }
    let mut time = None;
    for (first, path, last) in &plan.segments {
        let (content, last_time) = reader::segment_prefix(path, *first, *last)?;
        let name = format::segment_name(*first);
        let target = dest.join(WAL_DIR).join(&name);
        write_file(fs, &target, &content)?;
        let crc = crc32c::crc32c(&content);
        files.push(ManifestFile { path: format!("{}/{}", WAL_DIR, name), len: content.len() as u64, crc32c: crc });
        bytes += content.len() as u64;
        if *last == seq {
            time = last_time;
        }
    }
    sync_dir(fs, &dest.join(CHECKPOINT_DIR))?;
    sync_dir(fs, &dest.join(WAL_DIR))?;

    files.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest =
        Manifest { history, seq, time, created: CommitTime::now(), source: source.display().to_string(), files };
    write_atomic(fs, &dest.join(BACKUP_NAME), &manifest.encode())?;
    sync_dir(fs, dest)?;
    write_atomic(fs, &dest.join(MARKER_NAME), &encode_marker(history))?;
    sync_dir(fs, dest)?;
    Ok(BackupReport {
        path: dest.to_path_buf(),
        seq,
        time,
        history,
        checkpoints: plan.checkpoints.iter().map(|(s, _)| *s).collect(),
        segments: plan.segments.iter().map(|(s, _, _)| *s).collect(),
        bytes,
    })
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
            history: HistoryId([3; 16]),
            seq: 42,
            time: Some(CommitTime(1_759_312_800_000_000)),
            created: CommitTime(1_759_312_900_000_000),
            source: "/data/store".into(),
            files: vec![
                ManifestFile { path: "checkpoints/00000000000000000030.ckpt".into(), len: 1000, crc32c: 7 },
                ManifestFile { path: "wal/00000000000000000031.wal".into(), len: 500, crc32c: 9 },
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
        let mut escaping = m;
        escaping.files[0].path = "../outside".into();
        assert!(Manifest::decode(&escaping.encode()).unwrap_err().contains("invalid path"));
    }
}
