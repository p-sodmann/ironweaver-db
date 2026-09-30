//! The data directory: its layout, marker and exclusive lock. The normative
//! description is `documentation/formats/data-dir.md`.
//!
//! ```text
//! <dir>/
//!   IWDB           marker: magic, layout version, CRC32C (16 bytes)
//!   LOCK           held with an exclusive lock while a store has the directory open
//!   checkpoints/   <seq, 20 digits>.ckpt   (see `checkpoint`)
//!   wal/           <first seq, 20 digits>.wal   (see `documentation/formats/wal.md`)
//! ```

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::io::LogFs;
use crate::Error;

/// The layout version this version writes and reads.
pub const LAYOUT_VERSION: u32 = 1;
/// The marker file's name.
pub const MARKER_NAME: &str = "IWDB";
/// The lock file's name.
pub const LOCK_NAME: &str = "LOCK";
/// The checkpoint directory's name.
pub const CHECKPOINT_DIR: &str = "checkpoints";
/// The WAL directory's name.
pub const WAL_DIR: &str = "wal";
/// The first 8 bytes of the marker.
pub const MARKER_MAGIC: [u8; 8] = *b"IWDBDIR\n";
/// Length of the marker file.
pub const MARKER_LEN: usize = 16;
/// Suffix of temporary files (a checkpoint, marker or segment being
/// written). Any file with it is stale when a store opens.
pub const TEMP_SUFFIX: &str = ".tmp";

/// The marker's bytes: magic, version (u32 LE), CRC32C of bytes 0..12.
pub fn encode_marker(version: u32) -> [u8; MARKER_LEN] {
    let mut marker = [0u8; MARKER_LEN];
    marker[..8].copy_from_slice(&MARKER_MAGIC);
    marker[8..12].copy_from_slice(&version.to_le_bytes());
    let crc = crc32c::crc32c(&marker[..12]);
    marker[12..].copy_from_slice(&crc.to_le_bytes());
    marker
}

/// What a marker file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Version(u32),
    /// Not our magic: some other file.
    Foreign,
    /// Our magic, but the wrong length or checksum.
    Damaged,
}

fn decode_marker(bytes: &[u8]) -> Marker {
    if !bytes.starts_with(&MARKER_MAGIC) {
        return Marker::Foreign;
    }
    if bytes.len() != MARKER_LEN || crc32c::crc32c(&bytes[..12]).to_le_bytes() != bytes[12..16] {
        return Marker::Damaged;
    }
    Marker::Version(u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]))
}

/// An open data directory: its paths and the exclusive lock, held until
/// this value is dropped (or the process exits).
///
/// The lock is `flock` on `LOCK` (`LockFileEx` on Windows), taken without
/// waiting. It is per open file, so a second open in the same process fails
/// like one in another process. It is advisory: it keeps out other stores
/// and tools that take it, not arbitrary programs.
#[derive(Debug)]
pub struct DataDir {
    root: PathBuf,
    checkpoints: PathBuf,
    wal: PathBuf,
    /// Holds the lock; closing it releases the lock.
    _lock: File,
}

impl DataDir {
    /// Open the data directory `root` and take its lock. With `create`, a
    /// missing or empty directory is initialized (so is one left by an
    /// interrupted initialization); otherwise it must exist with a marker.
    /// Returns the directory and whether it was created.
    ///
    /// Errors, all before anything is changed except as noted:
    /// [`Error::NotADataDir`] (no marker, and files that aren't ours, or
    /// `create` is false), [`Error::UnsupportedLayout`] (a newer layout),
    /// [`Error::InvalidDataDir`] (a damaged marker, or a missing `wal/` or
    /// `checkpoints/`), [`Error::Locked`] (another store has it open; the
    /// `LOCK` file exists afterwards), [`Error::Io`].
    pub fn open<F: LogFs>(fs: &F, root: &Path, create: bool) -> Result<(DataDir, bool), Error> {
        let not_ours = |reason: &str| Error::NotADataDir { path: root.to_path_buf(), reason: reason.to_owned() };
        match fs::metadata(root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(not_ours("it is not a directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                fs::create_dir_all(root).map_err(|e| Error::io("create directory", root, e))?;
                if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs.sync_dir(parent).map_err(|e| Error::io("sync directory", parent, e))?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(not_ours("it doesn't exist")),
            Err(e) => return Err(Error::io("stat", root, e)),
        }

        let initialized = read_marker(root)?;
        if !initialized {
            check_empty(root)?;
            if !create {
                return Err(not_ours("it has no marker file"));
            }
        }

        let lock_path = root.join(LOCK_NAME);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::io("open", &lock_path, e))?;
        match fs4::FileExt::try_lock(&lock) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => return Err(Error::Locked { path: lock_path }),
            Err(fs4::TryLockError::Error(e)) => return Err(Error::io("lock", &lock_path, e)),
        }

        let dir = DataDir {
            root: root.to_path_buf(),
            checkpoints: root.join(CHECKPOINT_DIR),
            wal: root.join(WAL_DIR),
            _lock: lock,
        };
        // Checked again under the lock: another store may have initialized it
        let created = !read_marker(root)?;
        if created {
            check_empty(root)?;
            dir.initialize(fs)?;
        }
        for sub in [&dir.checkpoints, &dir.wal] {
            if !sub.is_dir() {
                return Err(Error::InvalidDataDir {
                    path: root.to_path_buf(),
                    reason: format!("'{}' is missing", sub.display()),
                });
            }
        }
        Ok((dir, created))
    }

    /// Create the subdirectories, then the marker, each made durable. The
    /// marker comes last: a directory without one holds nothing but these.
    fn initialize<F: LogFs>(&self, fs: &F) -> Result<(), Error> {
        for sub in [&self.checkpoints, &self.wal] {
            match fs::create_dir(sub) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(Error::io("create directory", sub, e)),
            }
        }
        fs.sync_dir(&self.root).map_err(|e| Error::io("sync directory", &self.root, e))?;
        let marker = self.root.join(MARKER_NAME);
        let bytes = encode_marker(LAYOUT_VERSION);
        fs.write_atomic(&marker, &mut |out| out.write_all(&bytes)).map_err(|e| Error::io("write", &marker, e))?;
        fs.sync_dir(&self.root).map_err(|e| Error::io("sync directory", &self.root, e))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `checkpoints/`.
    pub fn checkpoint_dir(&self) -> &Path {
        &self.checkpoints
    }

    /// `wal/`.
    pub fn wal_dir(&self) -> &Path {
        &self.wal
    }

    /// Remove stale temporary files (`*.tmp`) in the root, `checkpoints/`
    /// and `wal/`: a checkpoint, marker or segment whose creation was
    /// interrupted. None of them is ever read. Each directory where a file
    /// was removed is synced. Returns the removed files.
    pub fn remove_temp_files<F: LogFs>(&self, fs: &F) -> Result<Vec<PathBuf>, Error> {
        let mut removed = Vec::new();
        for dir in [&self.root, &self.checkpoints, &self.wal] {
            let mut any = false;
            for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
                let entry = entry.map_err(|e| Error::io("list", dir, e))?;
                let is_temp = entry.file_name().to_str().is_some_and(|n| n.ends_with(TEMP_SUFFIX));
                if is_temp && entry.file_type().map_err(|e| Error::io("stat", entry.path(), e))?.is_file() {
                    let path = entry.path();
                    fs.remove_file(&path).map_err(|e| Error::io("remove", &path, e))?;
                    removed.push(path);
                    any = true;
                }
            }
            if any {
                fs.sync_dir(dir).map_err(|e| Error::io("sync directory", dir, e))?;
            }
        }
        removed.sort();
        Ok(removed)
    }
}

/// Whether `root` has a valid marker of a known version (false if it has
/// none).
fn read_marker(root: &Path) -> Result<bool, Error> {
    let path = root.join(MARKER_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::io("read", &path, e)),
    };
    match decode_marker(&bytes) {
        Marker::Version(LAYOUT_VERSION) => Ok(true),
        Marker::Version(version) => Err(Error::UnsupportedLayout { path: root.to_path_buf(), version }),
        Marker::Foreign => Err(Error::NotADataDir {
            path: root.to_path_buf(),
            reason: format!("'{}' is not an Ironweaver DB marker", MARKER_NAME),
        }),
        Marker::Damaged => {
            Err(Error::InvalidDataDir { path: root.to_path_buf(), reason: format!("'{}' is damaged", MARKER_NAME) })
        }
    }
}

/// A directory without a marker may only hold what an interrupted
/// initialization leaves: `LOCK`, empty `checkpoints/` and `wal/`, and
/// temporary files.
fn check_empty(root: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(root).map_err(|e| Error::io("list", root, e))? {
        let entry = entry.map_err(|e| Error::io("list", root, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        let ok = match &*name {
            LOCK_NAME => path.is_file(),
            CHECKPOINT_DIR | WAL_DIR => {
                path.is_dir() && fs::read_dir(&path).map_err(|e| Error::io("list", &path, e))?.next().is_none()
            }
            other => other.ends_with(TEMP_SUFFIX) && path.is_file(),
        };
        if !ok {
            return Err(Error::NotADataDir {
                path: root.to_path_buf(),
                reason: format!("it has no marker file and holds '{}'", name),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_checked() {
        let marker = encode_marker(LAYOUT_VERSION);
        assert_eq!(decode_marker(&marker), Marker::Version(1));
        assert_eq!(decode_marker(&encode_marker(7)), Marker::Version(7));
        assert_eq!(decode_marker(b"hello"), Marker::Foreign);
        assert_eq!(decode_marker(&marker[..15]), Marker::Damaged);
        for bit in 64..128 {
            let mut bad = marker;
            bad[bit / 8] ^= 1 << (bit % 8);
            assert_eq!(decode_marker(&bad), Marker::Damaged, "bit {}", bit);
        }
    }
}
