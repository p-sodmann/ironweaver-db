//! Continuous WAL archiving: before the checkpointer removes WAL segments,
//! they are copied into an archive directory and made durable there
//! (`documentation/formats/archive.md`, ADR 0009).
//!
//! ```text
//! <archive>/
//!   IWDBARCH                      marker: magic, version, history id, CRC32C (32 bytes)
//!   LOCK                          held exclusively by the store that archives into it
//!   NAMESPACES                    a copy of the store's namespace log, kept up to date
//!   ns/<id, 20 digits>/
//!     <first seq, 20 digits>.wal  archived segments of namespace <id>, byte for byte
//!     <seq, 20 digits>.ckpt       the checkpoint an imported namespace starts from (format 3)
//!     <name>.tmp                  a segment or checkpoint being archived
//! ```
//!
//! A format 1 archive (steps 7 and 8, one namespace) has its segments at
//! the top and no `NAMESPACES`: they are namespace 1's. A store that
//! archives into one upgrades it ([`Archive::open`]). Format 2 (step 9) is
//! format 3 without checkpoints; it is upgraded by rewriting its marker.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::history::HistoryId;
use crate::io::{CHUNK, LogFs, copy_file};
use crate::layout::{LOCK_NAME, TEMP_SUFFIX};
use crate::namespaces::{
    DEFAULT_ID, Event, EventKind, NAMESPACES_NAME, NS_DIR, ns_dir_name, parse_ns_dir_name, read_log, write_whole,
};
use crate::verify::{Kind, VerifyReport};
use crate::{Error, WalReader, format, reader};

mod prune;

pub use prune::{NamespacePrune, PruneReport, prune};

/// The archive marker's name.
pub const ARCHIVE_MARKER_NAME: &str = "IWDBARCH";
/// The first 8 bytes of the archive marker.
pub const ARCHIVE_MAGIC: [u8; 8] = *b"IWDBARC\n";
/// The archive format this version writes. Formats 1 (one namespace, the
/// segments at the top) and 2 (no checkpoints) are read, and upgraded when
/// a store archives into them.
pub const ARCHIVE_VERSION: u32 = 3;
/// Length of the archive marker.
pub const ARCHIVE_MARKER_LEN: usize = 32;

/// The archive marker of history `history` in the current format: magic,
/// version (u32 LE), the history id, CRC32C of the 28 bytes before it.
pub fn encode_archive_marker(history: HistoryId) -> [u8; ARCHIVE_MARKER_LEN] {
    encode_archive_marker_with(ARCHIVE_VERSION, history)
}

/// The archive marker of format `version`.
pub fn encode_archive_marker_with(version: u32, history: HistoryId) -> [u8; ARCHIVE_MARKER_LEN] {
    let mut marker = [0u8; ARCHIVE_MARKER_LEN];
    marker[..8].copy_from_slice(&ARCHIVE_MAGIC);
    marker[8..12].copy_from_slice(&version.to_le_bytes());
    marker[12..28].copy_from_slice(&history.0);
    let crc = crc32c::crc32c(&marker[..28]);
    marker[28..].copy_from_slice(&crc.to_le_bytes());
    marker
}

/// Read the archive marker in `dir`: its history id, `None` if there is no
/// marker. Errors: [`Error::NotAnArchive`] for a damaged or foreign marker
/// or one of a newer format.
pub fn read_archive_marker(dir: &Path) -> Result<Option<HistoryId>, Error> {
    Ok(read_archive_marker_info(dir)?.map(|(_, history)| history))
}

/// Like [`read_archive_marker`], with the archive's format version.
pub fn read_archive_marker_info(dir: &Path) -> Result<Option<(u32, HistoryId)>, Error> {
    let path = dir.join(ARCHIVE_MARKER_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("read", &path, e)),
    };
    let not = |reason: String| Error::NotAnArchive { path: dir.to_path_buf(), reason };
    if !bytes.starts_with(&ARCHIVE_MAGIC) {
        return Err(not(format!("'{}' is not an archive marker", ARCHIVE_MARKER_NAME)));
    }
    if bytes.len() < 16 || crc32c::crc32c(&bytes[..bytes.len() - 4]).to_le_bytes() != bytes[bytes.len() - 4..] {
        return Err(not(format!("'{}' is damaged", ARCHIVE_MARKER_NAME)));
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if !(1..=ARCHIVE_VERSION).contains(&version) || bytes.len() != ARCHIVE_MARKER_LEN {
        return Err(not(format!("archive format {} (this version reads 1 to {})", version, ARCHIVE_VERSION)));
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&bytes[12..28]);
    Ok(Some((version, HistoryId(id))))
}

/// The segments of namespace `id` in the archive `dir`, by first seq: in
/// `ns/<id>/` (format 2), or at the top for namespace 1 of a format 1
/// archive.
pub fn archive_segments(dir: &Path, version: u32, id: u64) -> Result<Vec<(u64, PathBuf)>, Error> {
    if version < 2 {
        return if id == DEFAULT_ID { reader::list_segments(dir) } else { Ok(Vec::new()) };
    }
    let ns_dir = dir.join(NS_DIR).join(ns_dir_name(id));
    if ns_dir.is_dir() { reader::list_segments(&ns_dir) } else { Ok(Vec::new()) }
}

/// The checkpoints of namespace `id` in the archive `dir` (format 3: the
/// checkpoints imported namespaces start from), by seq.
pub fn archive_checkpoints(dir: &Path, version: u32, id: u64) -> Result<Vec<(u64, PathBuf)>, Error> {
    let ns_dir = dir.join(NS_DIR).join(ns_dir_name(id));
    if version < 3 || !ns_dir.is_dir() {
        return Ok(Vec::new());
    }
    crate::checkpoint::list_checkpoints(&ns_dir)
}

/// The ids of the namespaces that have a directory in the format 2 archive
/// `dir`.
pub fn archive_namespace_ids(dir: &Path) -> Result<Vec<u64>, Error> {
    let ns_root = dir.join(NS_DIR);
    let mut ids = Vec::new();
    if let Ok(entries) = fs::read_dir(&ns_root) {
        for entry in entries.flatten() {
            if let Some(id) = parse_ns_dir_name(&entry.file_name().to_string_lossy()) {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// An archive directory a store archives into: it holds the archive's
/// exclusive lock while it exists. The store's namespaces share it, each
/// through an [`ArchiveHandle`].
#[derive(Debug)]
pub struct Archive<F: LogFs> {
    fs: F,
    dir: PathBuf,
    history: HistoryId,
    _lock: File,
}

impl<F: LogFs> Archive<F> {
    /// Open the archive directory `dir` for a store of history `history`,
    /// creating and initializing it if it is missing or empty (`ns/`, then
    /// the marker written with `write_atomic`, then the directory synced;
    /// an empty `ns/` without a marker counts as empty, since a crash
    /// during initialization leaves it),
    /// and take its exclusive lock. A format 1 archive (one namespace, the
    /// segments at the top) is upgraded: its segments are renamed into
    /// `ns/1/` (each rename is atomic, and a crash leaves the format 1
    /// marker, so the next open moves the rest) and then the format 2
    /// marker is written. Format 1 archives belong to the one namespace of
    /// a layout 1 to 3 store, which has id 1.
    ///
    /// Errors: [`Error::ArchiveMismatch`] if it belongs to another
    /// history (a restored store must archive into a new directory);
    /// [`Error::NotAnArchive`] for a directory with other files and no
    /// marker, or a damaged marker; [`Error::Locked`] if another store
    /// archives into it; [`Error::Io`].
    pub fn open(fs: F, dir: &Path, history: HistoryId) -> Result<Self, Error> {
        if !dir.is_dir() {
            crate::io::create_dir(&fs, dir)?;
        }
        let lock_path = dir.join(LOCK_NAME);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::io("open", &lock_path, e))?;
        crate::layout::lock_file(&lock, &lock_path, true)?;
        let archive = Archive { fs, dir: dir.to_path_buf(), history, _lock: lock };
        match read_archive_marker_info(dir)? {
            Some((_, found)) if found != history => {
                return Err(Error::ArchiveMismatch { path: dir.to_path_buf(), expected: history, found });
            }
            Some((1, _)) => archive.upgrade_v1()?,
            // Format 3 adds files only: the marker says that it may have them
            Some((2, _)) => {
                let marker = encode_archive_marker(history);
                crate::io::write_atomic(&archive.fs, &dir.join(ARCHIVE_MARKER_NAME), &marker)?;
                crate::io::sync_dir(&archive.fs, dir)?;
            }
            Some(_) => {}
            None => {
                for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
                    let entry = entry.map_err(|e| Error::io("list", dir, e))?;
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    // An empty `ns/` is what a crash between creating it and
                    // writing the marker leaves behind.
                    if name == NS_DIR && dir_is_empty(&entry.path())? {
                        continue;
                    }
                    if name != LOCK_NAME && !name.ends_with(TEMP_SUFFIX) {
                        return Err(Error::NotAnArchive {
                            path: dir.to_path_buf(),
                            reason: format!("it has no '{}' marker and holds '{}'", ARCHIVE_MARKER_NAME, name),
                        });
                    }
                }
                archive.make_dir(&archive.dir.join(NS_DIR))?;
                let marker = encode_archive_marker(history);
                crate::io::write_atomic(&archive.fs, &dir.join(ARCHIVE_MARKER_NAME), &marker)?;
                crate::io::sync_dir(&archive.fs, dir)?;
            }
        }
        Ok(archive)
    }

    /// Create `dir` if it is missing.
    fn make_dir(&self, dir: &Path) -> Result<(), Error> {
        match self.fs.create_dir(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(Error::io("create directory", dir, e)),
        }
    }

    /// Upgrade a format 1 archive (see [`open`](Self::open)).
    fn upgrade_v1(&self) -> Result<(), Error> {
        let ns_root = self.dir.join(NS_DIR);
        let target = ns_root.join(ns_dir_name(DEFAULT_ID));
        self.make_dir(&ns_root)?;
        self.make_dir(&target)?;
        crate::io::sync_dir(&self.fs, &ns_root)?;
        for (first_seq, source) in reader::list_segments(&self.dir)? {
            let to = target.join(format::segment_name(first_seq));
            if to.exists() {
                return Err(Error::ArchiveConflict { path: to });
            }
            self.fs.rename(&source, &to).map_err(|e| Error::io("rename", &source, e))?;
        }
        crate::io::sync_dir(&self.fs, &target)?;
        crate::io::sync_dir(&self.fs, &self.dir)?;
        let marker = encode_archive_marker(self.history);
        crate::io::write_atomic(&self.fs, &self.dir.join(ARCHIVE_MARKER_NAME), &marker)?;
        crate::io::sync_dir(&self.fs, &self.dir)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn history(&self) -> HistoryId {
        self.history
    }

    /// Copy the segment files `segments` (`(first seq, path)`) of namespace
    /// `id` into the archive's `ns/<id>/`, each as `<name>.tmp` (created,
    /// written in chunks, fsynced) renamed over `<name>`. They are durable
    /// once [`sync`](Self::sync) has synced the directory.
    ///
    /// A segment that is in the archive already must be byte-for-byte the
    /// same (a crash between archiving and removal leaves it in both
    /// places): it is copied again, so that its durability doesn't rest on
    /// an fsync that may never have completed. A different one fails with
    /// [`Error::ArchiveConflict`] before anything is written for it.
    pub fn copy(&self, id: u64, segments: &[(u64, PathBuf)]) -> Result<(), Error> {
        self.ensure_dir(id)?;
        let dir = self.dir.join(NS_DIR).join(ns_dir_name(id));
        for (first_seq, source) in segments {
            let name = format::segment_name(*first_seq);
            let target = dir.join(&name);
            if target.exists() && !same_content(source, &target)? {
                return Err(Error::ArchiveConflict { path: target });
            }
            let tmp = dir.join(format!("{}{}", name, TEMP_SUFFIX));
            copy_file(&self.fs, source, &tmp)?;
            self.fs.rename(&tmp, &target).map_err(|e| Error::io("rename", &tmp, e))?;
        }
        Ok(())
    }

    /// Copy the checkpoint file `source` at `seq` of namespace `id` into the
    /// archive's `ns/<id>/` (as [`copy`](Self::copy) copies segments: a
    /// `.tmp` file, fsynced, renamed; one that is there already must have
    /// the same bytes), and sync the directory. For the checkpoint an
    /// imported namespace starts from, which no WAL record holds (ADR 0033).
    pub fn copy_checkpoint(&self, id: u64, seq: u64, source: &Path) -> Result<(), Error> {
        self.ensure_dir(id)?;
        let dir = self.dir.join(NS_DIR).join(ns_dir_name(id));
        let name = crate::checkpoint::checkpoint_name(seq);
        let target = dir.join(&name);
        if target.exists() && !same_content(source, &target)? {
            return Err(Error::ArchiveConflict { path: target });
        }
        let tmp = dir.join(format!("{}{}", name, TEMP_SUFFIX));
        copy_file(&self.fs, source, &tmp)?;
        self.fs.rename(&tmp, &target).map_err(|e| Error::io("rename", &tmp, e))?;
        self.sync(id)
    }

    /// Make namespace `id`'s directory in the archive, and sync its
    /// parents, if it isn't there. A failed directory sync is never
    /// retried (ADR 0005): the checkpointer calls this on its own, so that
    /// it can disable itself on such a failure.
    pub fn ensure_dir(&self, id: u64) -> Result<(), Error> {
        let ns_root = self.dir.join(NS_DIR);
        let dir = ns_root.join(ns_dir_name(id));
        if !dir.is_dir() {
            self.make_dir(&ns_root)?;
            self.make_dir(&dir)?;
            crate::io::sync_dir(&self.fs, &ns_root)?;
            crate::io::sync_dir(&self.fs, &self.dir)?;
        }
        Ok(())
    }

    /// Sync the directory of namespace `id`: the segments copied are then
    /// durable. The caller must never retry a failed sync (ADR 0005).
    pub fn sync(&self, id: u64) -> Result<(), Error> {
        crate::io::sync_dir(&self.fs, &self.dir.join(NS_DIR).join(ns_dir_name(id)))
    }

    /// Replace the archive's copy of the store's namespace log with
    /// `events` (`write_atomic`, then a directory sync). Restore reads it
    /// to know which namespaces existed when.
    pub fn write_log(&self, events: &[Event]) -> Result<(), Error> {
        write_whole(&self.fs, &self.dir.join(NAMESPACES_NAME), events)?;
        crate::io::sync_dir(&self.fs, &self.dir)
    }
}

/// One namespace's use of the archive: what its checkpointer holds.
#[derive(Debug)]
pub struct ArchiveHandle<F: LogFs> {
    archive: std::sync::Arc<Archive<F>>,
    id: u64,
}

impl<F: LogFs> ArchiveHandle<F> {
    pub fn new(archive: std::sync::Arc<Archive<F>>, id: u64) -> Self {
        ArchiveHandle { archive, id }
    }

    /// See [`Archive::ensure_dir`].
    pub fn ensure_dir(&self) -> Result<(), Error> {
        self.archive.ensure_dir(self.id)
    }

    /// See [`Archive::copy`].
    pub fn copy(&self, segments: &[(u64, PathBuf)]) -> Result<(), Error> {
        self.archive.copy(self.id, segments)
    }

    /// See [`Archive::sync`].
    pub fn sync(&self) -> Result<(), Error> {
        self.archive.sync(self.id)
    }

    pub fn archive(&self) -> &Archive<F> {
        &self.archive
    }
}

/// Whether two files have the same bytes (compared in chunks).
fn same_content(a: &Path, b: &Path) -> Result<bool, Error> {
    let len = |p: &Path| fs::metadata(p).map(|m| m.len()).map_err(|e| Error::io("stat", p, e));
    if len(a)? != len(b)? {
        return Ok(false);
    }
    let open = |p: &Path| File::open(p).map_err(|e| Error::io("open", p, e));
    let (mut fa, mut fb) = (open(a)?, open(b)?);
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let n = read_full(&mut fa, &mut ba).map_err(|e| Error::io("read", a, e))?;
        let m = read_full(&mut fb, &mut bb).map_err(|e| Error::io("read", b, e))?;
        if n != m || ba[..n] != bb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Verify an archive directory without changing it: its marker, the
/// namespace log copy, and for each namespace every segment from the first
/// to the last (headers, every frame's checksum, seq and contents), the
/// chain between them, and that every segment is complete (an archived
/// segment never has a torn tail). Temporary files (a segment being
/// archived) are notes. Archives aren't replayed: they hold no checkpoint
/// to compare with. Takes no lock: archived files appear by rename,
/// complete.
pub fn verify_archive(dir: &Path) -> Result<VerifyReport, Error> {
    let mut report = VerifyReport::new(dir, Kind::Archive);
    let mut version = 1;
    match read_archive_marker_info(dir) {
        Ok(Some((found, history))) => {
            version = found;
            report.version = Some(found);
            report.history = Some(history);
        }
        Ok(None) => {
            return Err(Error::NotAnArchive {
                path: dir.to_path_buf(),
                reason: format!("it has no '{}' marker", ARCHIVE_MARKER_NAME),
            });
        }
        Err(Error::NotAnArchive { reason, .. }) => report.problem(Some(&dir.join(ARCHIVE_MARKER_NAME)), reason),
        Err(e) => return Err(e),
    }
    for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
        let entry = entry.map_err(|e| Error::io("list", dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(TEMP_SUFFIX) {
            report.note(Some(&entry.path()), "a segment being archived (a temporary file)");
        } else if ![ARCHIVE_MARKER_NAME, LOCK_NAME].contains(&name.as_str())
            && !(version >= 2 && [NAMESPACES_NAME, NS_DIR].contains(&name.as_str()))
            && !(version < 2 && format::parse_segment_name(&name).is_some())
        {
            report.note(Some(&entry.path()), "not a file of the archive (ignored)");
        }
    }
    let table = if version >= 2 && dir.join(NAMESPACES_NAME).exists() {
        match read_log(&dir.join(NAMESPACES_NAME)) {
            Ok((parsed, table)) => {
                if let Some(reason) = parsed.torn {
                    report.note(Some(&dir.join(NAMESPACES_NAME)), format!("a torn tail ({})", reason));
                }
                Some(table)
            }
            Err(Error::InvalidNamespaceLog { reason, .. }) => {
                report.problem(Some(&dir.join(NAMESPACES_NAME)), reason);
                None
            }
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    let ids: Vec<u64> = if version >= 2 { archive_namespace_ids(dir)? } else { vec![DEFAULT_ID] };
    for id in ids {
        let name = table
            .as_ref()
            .and_then(|t| t.events().iter().find(|e| e.id == id && e.kind == EventKind::Create))
            .map_or_else(|| format!("#{}", id), |e| e.name.to_string());
        let mut sub = VerifyReport::new(dir, Kind::Archive);
        verify_namespace_segments(&mut sub, dir, version, id)?;
        verify_namespace_checkpoints(&mut sub, dir, version, id, table.as_ref())?;
        report.merge(id, &name, version >= 2, sub);
    }
    report.summarize();
    Ok(report)
}

/// The segments of one namespace of an archive, into `report`.
fn verify_namespace_segments(report: &mut VerifyReport, dir: &Path, version: u32, id: u64) -> Result<(), Error> {
    let segments = archive_segments(dir, version, id)?;
    if version >= 2 {
        let ns_dir = dir.join(NS_DIR).join(ns_dir_name(id));
        for entry in fs::read_dir(&ns_dir).map_err(|e| Error::io("list", &ns_dir, e))? {
            let entry = entry.map_err(|e| Error::io("list", &ns_dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let checkpoint = version >= 3 && crate::checkpoint::parse_checkpoint_name(&name).is_some();
            if name.ends_with(TEMP_SUFFIX) {
                report.note(Some(&entry.path()), "a file being archived (a temporary file)");
            } else if format::parse_segment_name(&name).is_none() && !checkpoint {
                report.note(Some(&entry.path()), "not a file of the archive (ignored)");
            }
        }
    }
    report.segments = segments.len();
    let Some(&(first_seq, _)) = segments.first() else { return Ok(()) };
    let mut reader = match WalReader::from_segments(segments, first_seq, u64::MAX) {
        Ok(reader) => reader,
        Err(e) => {
            report.problem(None, e.to_string());
            return Ok(());
        }
    };
    while let Some(record) = reader.next() {
        match record {
            Ok(record) => {
                report.records += 1;
                report.first_seq.get_or_insert(record.seq);
                report.last_seq = Some(record.seq);
                report.time = reader.time().or(report.time);
            }
            Err(e) => {
                report.problem(None, format!("the archive can't be read further: {}", e));
                return Ok(());
            }
        }
    }
    if let Some(last) = reader.end().and_then(|end| end.last_segment.clone())
        && let Some(torn) = last.torn
    {
        report.problem(
            Some(&last.path),
            format!(
                "the segment is incomplete at offset {} ({}): archived segments are whole",
                last.valid_len, torn.damage
            ),
        );
    }
    Ok(())
}

/// The checkpoints of one namespace of a format 3 archive, into `report`:
/// each must load as a checkpoint of its namespace at its seq, and the
/// segments must go on from it.
fn verify_namespace_checkpoints(
    report: &mut VerifyReport,
    dir: &Path,
    version: u32,
    id: u64,
    table: Option<&crate::namespaces::NamespaceTable>,
) -> Result<(), Error> {
    let checkpoints = archive_checkpoints(dir, version, id)?;
    report.checkpoints = checkpoints.len();
    let name = table.and_then(|t| t.events().iter().find(|e| e.id == id && e.kind == EventKind::Create));
    for (seq, path) in &checkpoints {
        match name {
            Some(event) => {
                if let Err(e) = crate::checkpoint::load_checkpoint(path, *seq, &event.name) {
                    report.problem(Some(path), e.to_string());
                }
            }
            None => report.note(Some(path), "a checkpoint of a namespace the archive's log doesn't list"),
        }
        if let Some(first) = report.first_seq
            && first > seq + 1
        {
            report.problem(Some(path), format!("the archived records start at {}, after the checkpoint", first));
        }
    }
    Ok(())
}

/// Whether directory `dir` has no entries.
fn dir_is_empty(dir: &Path) -> Result<bool, Error> {
    let mut entries = fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))?;
    Ok(entries.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn markers_round_trip_and_damage_is_found() {
        let dir = tempfile::tempdir().expect("dir");
        assert_eq!(read_archive_marker(dir.path()).expect("read"), None);
        let id = HistoryId([9; 16]);
        let marker = encode_archive_marker(id);
        fs::write(dir.path().join(ARCHIVE_MARKER_NAME), marker).expect("write");
        assert_eq!(read_archive_marker(dir.path()).expect("read"), Some(id));
        for at in 0..ARCHIVE_MARKER_LEN {
            let mut bad = marker;
            bad[at] ^= 1;
            fs::write(dir.path().join(ARCHIVE_MARKER_NAME), bad).expect("write");
            assert_matches!(read_archive_marker(dir.path()), Err(Error::NotAnArchive { .. }), "byte {}", at);
        }
    }

    #[test]
    fn an_empty_ns_dir_without_a_marker_is_initialized() {
        // A crash between creating `ns/` and writing the marker
        let dir = tempfile::tempdir().expect("dir");
        fs::create_dir(dir.path().join(NS_DIR)).expect("create");
        let id = HistoryId([3; 16]);
        drop(Archive::open(crate::io::StdFs, dir.path(), id).expect("open"));
        assert_eq!(read_archive_marker(dir.path()).expect("read"), Some(id));

        // A non-empty `ns/` without a marker is still refused
        let dir = tempfile::tempdir().expect("dir");
        fs::create_dir_all(dir.path().join(NS_DIR).join("1")).expect("create");
        assert_matches!(Archive::open(crate::io::StdFs, dir.path(), id), Err(Error::NotAnArchive { .. }));
    }
}
