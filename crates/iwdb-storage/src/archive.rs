//! Continuous WAL archiving: before the checkpointer removes WAL segments,
//! they are copied into an archive directory and made durable there
//! (`documentation/formats/archive.md`, ADR 0009).
//!
//! ```text
//! <archive>/
//!   IWDBARCH                      marker: magic, version, history id, CRC32C (32 bytes)
//!   LOCK                          held exclusively by the store that archives into it
//!   <first seq, 20 digits>.wal    archived segments, byte for byte
//!   <name>.wal.tmp                a segment being archived
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::history::HistoryId;
use crate::io::{LogFile, LogFs};
use crate::layout::{LOCK_NAME, TEMP_SUFFIX};
use crate::verify::{Kind, VerifyReport};
use crate::{format, reader, Error, WalReader};

/// The archive marker's name.
pub const ARCHIVE_MARKER_NAME: &str = "IWDBARCH";
/// The first 8 bytes of the archive marker.
pub const ARCHIVE_MAGIC: [u8; 8] = *b"IWDBARC\n";
/// The archive format this version writes and reads.
pub const ARCHIVE_VERSION: u32 = 1;
/// Length of the archive marker.
pub const ARCHIVE_MARKER_LEN: usize = 32;
const CHUNK: usize = 1 << 20;

/// The archive marker of history `history`: magic, version (u32 LE), the
/// history id, CRC32C of the 28 bytes before it.
pub fn encode_archive_marker(history: HistoryId) -> [u8; ARCHIVE_MARKER_LEN] {
    let mut marker = [0u8; ARCHIVE_MARKER_LEN];
    marker[..8].copy_from_slice(&ARCHIVE_MAGIC);
    marker[8..12].copy_from_slice(&ARCHIVE_VERSION.to_le_bytes());
    marker[12..28].copy_from_slice(&history.0);
    let crc = crc32c::crc32c(&marker[..28]);
    marker[28..].copy_from_slice(&crc.to_le_bytes());
    marker
}

/// Read the archive marker in `dir`: its history id, `None` if there is no
/// marker. Errors: [`Error::NotAnArchive`] for a damaged or foreign marker
/// or one of a newer version.
pub fn read_archive_marker(dir: &Path) -> Result<Option<HistoryId>, Error> {
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
    if version != ARCHIVE_VERSION || bytes.len() != ARCHIVE_MARKER_LEN {
        return Err(not(format!("archive format {} (this version reads {})", version, ARCHIVE_VERSION)));
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&bytes[12..28]);
    Ok(Some(HistoryId(id)))
}

/// An archive directory a store archives into: it holds the archive's
/// exclusive lock while it exists.
#[derive(Debug)]
pub struct Archive<F: LogFs> {
    fs: F,
    dir: PathBuf,
    history: HistoryId,
    _lock: File,
}

impl<F: LogFs> Archive<F> {
    /// Open the archive directory `dir` for a store of history `history`,
    /// creating and initializing it if it is missing or empty (marker
    /// written with `write_atomic`, then the directory synced), and take
    /// its exclusive lock.
    ///
    /// Errors: [`Error::ArchiveMismatch`] if it belongs to another
    /// history (a restored store must archive into a new directory);
    /// [`Error::NotAnArchive`] for a directory with other files and no
    /// marker, or a damaged marker; [`Error::Locked`] if another store
    /// archives into it; [`Error::Io`].
    pub fn open(fs: F, dir: &Path, history: HistoryId) -> Result<Self, Error> {
        if !dir.is_dir() {
            crate::backup::create_dir(&fs, dir)?;
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
        match read_archive_marker(dir)? {
            Some(found) if found == history => {}
            Some(found) => {
                return Err(Error::ArchiveMismatch { path: dir.to_path_buf(), expected: history, found });
            }
            None => {
                for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
                    let name = entry.map_err(|e| Error::io("list", dir, e))?.file_name();
                    let name = name.to_string_lossy();
                    if name != LOCK_NAME && !name.ends_with(TEMP_SUFFIX) {
                        return Err(Error::NotAnArchive {
                            path: dir.to_path_buf(),
                            reason: format!("it has no '{}' marker and holds '{}'", ARCHIVE_MARKER_NAME, name),
                        });
                    }
                }
                let marker = encode_archive_marker(history);
                crate::backup::write_atomic(&fs, &dir.join(ARCHIVE_MARKER_NAME), &marker)?;
                crate::backup::sync_dir(&fs, dir)?;
            }
        }
        Ok(Archive { fs, dir: dir.to_path_buf(), history, _lock: lock })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn history(&self) -> HistoryId {
        self.history
    }

    /// Copy the segment files `segments` (`(first seq, path)`) into the
    /// archive, each as `<name>.tmp` (created, written in chunks, fsynced)
    /// renamed over `<name>`. They are durable once [`sync`](Self::sync)
    /// has synced the directory.
    ///
    /// A segment that is in the archive already must be byte-for-byte the
    /// same (a crash between archiving and removal leaves it in both
    /// places): it is copied again, so that its durability doesn't rest on
    /// an fsync that may never have completed. A different one fails with
    /// [`Error::ArchiveConflict`] before anything is written for it.
    pub fn copy(&self, segments: &[(u64, PathBuf)]) -> Result<(), Error> {
        for (first_seq, source) in segments {
            let name = format::segment_name(*first_seq);
            let target = self.dir.join(&name);
            if target.exists() && !same_content(source, &target)? {
                return Err(Error::ArchiveConflict { path: target });
            }
            let tmp = self.dir.join(format!("{}{}", name, TEMP_SUFFIX));
            copy_file(&self.fs, source, &tmp)?;
            self.fs.rename(&tmp, &target).map_err(|e| Error::io("rename", &tmp, e))?;
        }
        Ok(())
    }

    /// Sync the archive directory: the segments copied are then durable.
    /// The caller must never retry a failed sync (ADR 0005).
    pub fn sync(&self) -> Result<(), Error> {
        crate::backup::sync_dir(&self.fs, &self.dir)
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

/// Copy `source` to a new file `target` in chunks, and fsync it.
fn copy_file<F: LogFs>(fs: &F, source: &Path, target: &Path) -> Result<(), Error> {
    let mut input = File::open(source).map_err(|e| Error::io("open", source, e))?;
    let mut file = fs.create(target).map_err(|e| Error::io("create", target, e))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = input.read(&mut buf).map_err(|e| Error::io("read", source, e))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| Error::io("write", target, e))?;
    }
    file.sync().map_err(|e| Error::io("fsync", target, e))
}

/// Verify an archive directory without changing it: its marker, every
/// segment from the first to the last (headers, every frame's checksum,
/// seq and contents), the chain between them, and that every segment is
/// complete (an archived segment never has a torn tail). Temporary files
/// (a segment being archived) are notes. Archives aren't replayed: they
/// hold no checkpoint to compare with. Takes no lock: archived files
/// appear by rename, complete.
pub fn verify_archive(dir: &Path) -> Result<VerifyReport, Error> {
    let mut report = VerifyReport::new(dir, Kind::Archive);
    match read_archive_marker(dir) {
        Ok(Some(history)) => {
            report.version = Some(ARCHIVE_VERSION);
            report.history = Some(history);
        }
        Ok(None) => {
            return Err(Error::NotAnArchive {
                path: dir.to_path_buf(),
                reason: format!("it has no '{}' marker", ARCHIVE_MARKER_NAME),
            })
        }
        Err(Error::NotAnArchive { reason, .. }) => report.problem(Some(&dir.join(ARCHIVE_MARKER_NAME)), reason),
        Err(e) => return Err(e),
    }
    for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
        let entry = entry.map_err(|e| Error::io("list", dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(TEMP_SUFFIX) {
            report.note(Some(&entry.path()), "a segment being archived (a temporary file)");
        } else if name != ARCHIVE_MARKER_NAME && name != LOCK_NAME && format::parse_segment_name(&name).is_none() {
            report.note(Some(&entry.path()), "not a file of the archive (ignored)");
        }
    }
    let segments = reader::list_segments(dir)?;
    report.segments = segments.len();
    let Some(&(first_seq, _)) = segments.first() else { return Ok(report) };
    let mut reader = match WalReader::from_segments(segments, first_seq, u64::MAX) {
        Ok(reader) => reader,
        Err(e) => {
            report.problem(None, e.to_string());
            return Ok(report);
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
                return Ok(report);
            }
        }
    }
    if let Some(last) = reader.end().and_then(|end| end.last_segment.clone()) {
        if let Some(torn) = last.torn {
            report.problem(
                Some(&last.path),
                format!(
                    "the segment is incomplete at offset {} ({}): archived segments are whole",
                    last.valid_len, torn.damage
                ),
            );
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(matches!(read_archive_marker(dir.path()), Err(Error::NotAnArchive { .. })), "byte {}", at);
        }
    }
}
