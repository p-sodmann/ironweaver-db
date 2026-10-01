//! The data directory: its layout, marker and exclusive lock. The normative
//! description is `documentation/formats/data-dir.md`.
//!
//! ```text
//! <dir>/
//!   IWDB           marker: magic, layout version, history id, CRC32C (32 bytes; 16 in layout 1)
//!   LOCK           held with an exclusive lock while a store has the directory open
//!   checkpoints/   <seq, 20 digits>.ckpt   (see `checkpoint`)
//!   wal/           <first seq, 20 digits>.wal   (see `documentation/formats/wal.md`)
//!   BACKUP         only in a backup: its manifest (`documentation/formats/backup.md`)
//!   RESTORING      only while a restore writes the directory
//! ```

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::history::HistoryId;
use crate::io::LogFs;
use crate::Error;

/// The layout version this version writes. It reads layouts 1 and 2 too,
/// and upgrades them when it opens a store (see [`DataDir::open`]).
/// Layout 3 (step 8) adds the idempotency key table (`iwdb.keys`) to the
/// checkpoints' graph meta; its marker is layout 2's with version 3.
pub const LAYOUT_VERSION: u32 = 3;
/// The marker file's name.
pub const MARKER_NAME: &str = "IWDB";
/// The lock file's name.
pub const LOCK_NAME: &str = "LOCK";
/// The checkpoint directory's name.
pub const CHECKPOINT_DIR: &str = "checkpoints";
/// The WAL directory's name.
pub const WAL_DIR: &str = "wal";
/// A backup's manifest. A directory with it is a backup, which a store
/// refuses to open: it is restored instead.
pub const BACKUP_NAME: &str = "BACKUP";
/// Present while a restore writes a directory. A directory with it is an
/// interrupted restore, which a store refuses to open.
pub const RESTORING_NAME: &str = "RESTORING";
/// The first 8 bytes of the marker.
pub const MARKER_MAGIC: [u8; 8] = *b"IWDBDIR\n";
/// Length of the marker file (layouts 2 and 3).
pub const MARKER_LEN: usize = 32;
/// Length of a layout 1 marker.
pub const MARKER_LEN_V1: usize = 16;
/// Suffix of temporary files (a checkpoint, marker or segment being
/// written). Any file with it is stale when a store opens.
pub const TEMP_SUFFIX: &str = ".tmp";

/// A marker of layout `version`: magic, version (u32 LE), `body`, and the
/// CRC32C of everything before it. Every layout keeps this frame, so that a
/// reader can tell a newer marker from a damaged one.
pub fn encode_marker_with(version: u32, body: &[u8]) -> Vec<u8> {
    let mut marker = MARKER_MAGIC.to_vec();
    marker.extend_from_slice(&version.to_le_bytes());
    marker.extend_from_slice(body);
    let crc = crc32c::crc32c(&marker);
    marker.extend_from_slice(&crc.to_le_bytes());
    marker
}

/// The marker (current layout) of a directory of history `history`.
pub fn encode_marker(history: HistoryId) -> Vec<u8> {
    encode_marker_with(LAYOUT_VERSION, &history.0)
}

/// What a valid marker says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerInfo {
    /// The layout version (1, 2 or 3).
    pub version: u32,
    /// The history id (from layout 2; `None` in layout 1).
    pub history: Option<HistoryId>,
}

/// What a marker file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Valid(MarkerInfo),
    /// A valid marker of a layout this version doesn't know.
    Newer(u32),
    /// Not our magic: some other file.
    Foreign,
    /// Our magic, but the wrong length or checksum.
    Damaged,
}

fn decode_marker(bytes: &[u8]) -> Marker {
    if !bytes.starts_with(&MARKER_MAGIC) {
        return Marker::Foreign;
    }
    if bytes.len() < MARKER_LEN_V1 {
        return Marker::Damaged;
    }
    let (data, crc) = bytes.split_at(bytes.len() - 4);
    if crc32c::crc32c(data).to_le_bytes() != crc {
        return Marker::Damaged;
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    match (version, bytes.len()) {
        (1, MARKER_LEN_V1) => Marker::Valid(MarkerInfo { version, history: None }),
        (2 | 3, MARKER_LEN) => {
            let mut id = [0u8; 16];
            id.copy_from_slice(&bytes[12..28]);
            Marker::Valid(MarkerInfo { version, history: Some(HistoryId(id)) })
        }
        (version, _) if version > LAYOUT_VERSION => Marker::Newer(version),
        _ => Marker::Damaged,
    }
}

/// An open data directory: its paths, its history and the exclusive lock,
/// held until this value is dropped (or the process exits).
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
    history: HistoryId,
    /// The layout version found, if the marker still has to be upgraded to
    /// the current layout ([`upgrade`](Self::upgrade)).
    upgrade_from: Option<u32>,
    /// Holds the lock; closing it releases the lock.
    _lock: File,
}

impl DataDir {
    /// Open the data directory `root` and take its lock. With `create`, a
    /// missing or empty directory is initialized in the current layout
    /// (so is one left by an interrupted initialization), with a new
    /// history id; otherwise it must exist with a marker. Returns the
    /// directory and whether it was created.
    ///
    /// A layout 1 (step 5) or layout 2 (step 7) directory opens too: a
    /// layout 1 directory gets a new history id, a layout 2 one keeps its
    /// own, and [`upgrade`](Self::upgrade) writes it into a marker of the
    /// current layout. Recovery does that once the directory has been read
    /// successfully, so a failed open changes nothing.
    ///
    /// Errors, all before anything is changed except as noted:
    /// [`Error::NotADataDir`] (no marker, and files that aren't ours, or
    /// `create` is false), [`Error::UnsupportedLayout`] (a newer layout),
    /// [`Error::InvalidDataDir`] (a damaged marker, or a missing `wal/` or
    /// `checkpoints/`), [`Error::IsBackup`] (a backup: restore it instead),
    /// [`Error::InterruptedRestore`], [`Error::Locked`] (another store has
    /// it open; the `LOCK` file exists afterwards), [`Error::Io`].
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

        check_openable(root)?;
        let initialized = read_marker(root)?.is_some();
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
        lock_file(&lock, &lock_path, true)?;

        // Checked again under the lock: another store may have initialized
        // it, or a restore may have finished it
        check_openable(root)?;
        let marker = read_marker(root)?;
        let created = marker.is_none();
        let (history, upgrade_from) = match marker {
            Some(MarkerInfo { version, history: Some(history) }) => {
                (history, (version < LAYOUT_VERSION).then_some(version))
            }
            Some(MarkerInfo { version, history: None }) => (HistoryId::random(), Some(version)),
            None => (HistoryId::random(), None),
        };
        let dir = DataDir {
            root: root.to_path_buf(),
            checkpoints: root.join(CHECKPOINT_DIR),
            wal: root.join(WAL_DIR),
            history,
            upgrade_from,
            _lock: lock,
        };
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
        self.write_marker(fs)
    }

    /// Write the current layout's marker with `write_atomic`, and sync the
    /// directory.
    fn write_marker<F: LogFs>(&self, fs: &F) -> Result<(), Error> {
        let marker = self.root.join(MARKER_NAME);
        let bytes = encode_marker(self.history);
        fs.write_atomic(&marker, &mut |out| out.write_all(&bytes)).map_err(|e| Error::io("write", &marker, e))?;
        fs.sync_dir(&self.root).map_err(|e| Error::io("sync directory", &self.root, e))
    }

    /// Upgrade a layout 1 or 2 directory to the current layout: replace its
    /// marker (atomically, then a directory sync) with one of the current
    /// layout that holds its history id (layout 2), or the one
    /// [`open`](Self::open) chose (layout 1, which has none). Nothing else
    /// changes: the checkpoints and segments of layouts 1 and 2 are valid
    /// in layout 3 (a checkpoint without `iwdb.keys` has an empty key
    /// table). Does nothing for a directory in the current layout. Returns
    /// the layout version it upgraded from.
    ///
    /// An older version of Ironweaver DB can't open the directory
    /// afterwards. A crash leaves the old marker or the new one.
    pub fn upgrade<F: LogFs>(&mut self, fs: &F) -> Result<Option<u32>, Error> {
        let Some(from) = self.upgrade_from else { return Ok(None) };
        self.write_marker(fs)?;
        self.upgrade_from = None;
        Ok(Some(from))
    }

    /// The directory's history id (for a layout 1 directory, the one that
    /// [`upgrade`](Self::upgrade) writes).
    pub fn history(&self) -> HistoryId {
        self.history
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

/// Read the marker of the data directory `root`, without taking the lock
/// or changing anything: `None` if it has none. Errors:
/// [`Error::UnsupportedLayout`] (a newer layout), [`Error::NotADataDir`]
/// (a file named `IWDB` that isn't ours), [`Error::InvalidDataDir`] (a
/// damaged marker), [`Error::Io`].
pub fn read_marker(root: &Path) -> Result<Option<MarkerInfo>, Error> {
    let path = root.join(MARKER_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("read", &path, e)),
    };
    match decode_marker(&bytes) {
        Marker::Valid(info) => Ok(Some(info)),
        Marker::Newer(version) => Err(Error::UnsupportedLayout { path: root.to_path_buf(), version }),
        Marker::Foreign => Err(Error::NotADataDir {
            path: root.to_path_buf(),
            reason: format!("'{}' is not an Ironweaver DB marker", MARKER_NAME),
        }),
        Marker::Damaged => {
            Err(Error::InvalidDataDir { path: root.to_path_buf(), reason: format!("'{}' is damaged", MARKER_NAME) })
        }
    }
}

/// A store opens neither an interrupted restore nor a backup.
fn check_openable(root: &Path) -> Result<(), Error> {
    if root.join(RESTORING_NAME).exists() {
        return Err(Error::InterruptedRestore { path: root.to_path_buf() });
    }
    if root.join(BACKUP_NAME).exists() && root.join(MARKER_NAME).exists() {
        return Err(Error::IsBackup { path: root.to_path_buf() });
    }
    Ok(())
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
            let what = if matches!(&*name, CHECKPOINT_DIR | WAL_DIR) { "a non-empty " } else { "" };
            return Err(Error::NotADataDir {
                path: root.to_path_buf(),
                reason: format!(
                    "it has no marker file and holds {}'{}' (an interrupted backup or restore leaves such a directory)",
                    what, name
                ),
            });
        }
    }
    Ok(())
}

/// Take a shared lock on `<root>/LOCK`, if that file exists, without
/// creating it: readers that must not run while a store has the directory
/// open (verify, restore reading a data directory) hold it while they
/// read. Shared locks don't exclude each other. Returns the locked file,
/// or `None` if there is no `LOCK` file (then no store has it open: a
/// store creates `LOCK` before anything else). Fails with
/// [`Error::Locked`] if a store has it open.
pub fn lock_shared(root: &Path) -> Result<Option<File>, Error> {
    let path = root.join(LOCK_NAME);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open", &path, e)),
    };
    lock_file(&file, &path, false)?;
    Ok(Some(file))
}

/// How long [`lock_file`] waits between attempts, in milliseconds: about
/// 80 ms in all.
const LOCK_RETRIES_MS: [u64; 6] = [1, 2, 5, 10, 20, 40];

/// Lock `file` (the lock file at `path`), exclusively or shared, without
/// blocking; [`Error::Locked`] if it is held.
///
/// A held lock is tried again a few times over about 80 ms before giving
/// up. A process that another thread of this process is spawning holds a
/// copy of every open file between its fork and its exec (close-on-exec
/// takes effect only at the exec), and a `flock` belongs to the open file,
/// so the lock of a store that was just closed can look held for that
/// moment. Without the retries, reopening a store while another thread
/// starts processes failed now and then (step 7 found it: 3.5% of reopens
/// under heavy spawning). A real holder makes the open fail after the
/// retries.
pub fn lock_file(file: &File, path: &Path, exclusive: bool) -> Result<(), Error> {
    let attempt = || {
        if exclusive {
            fs4::FileExt::try_lock(file)
        } else {
            fs4::FileExt::try_lock_shared(file)
        }
    };
    for wait in LOCK_RETRIES_MS.iter().map(|ms| Some(std::time::Duration::from_millis(*ms))).chain([None]) {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(fs4::TryLockError::WouldBlock) => match wait {
                Some(wait) => std::thread::sleep(wait),
                None => return Err(Error::Locked { path: path.to_path_buf() }),
            },
            Err(fs4::TryLockError::Error(e)) => return Err(Error::io("lock", path, e)),
        }
    }
    Err(Error::Locked { path: path.to_path_buf() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_checked() {
        let history = HistoryId([7; 16]);
        let marker = encode_marker(history);
        assert_eq!(marker.len(), MARKER_LEN);
        assert_eq!(decode_marker(&marker), Marker::Valid(MarkerInfo { version: 3, history: Some(history) }));
        let v2 = encode_marker_with(2, &history.0);
        assert_eq!(decode_marker(&v2), Marker::Valid(MarkerInfo { version: 2, history: Some(history) }));
        let v1 = encode_marker_with(1, &[]);
        assert_eq!(v1.len(), MARKER_LEN_V1);
        assert_eq!(decode_marker(&v1), Marker::Valid(MarkerInfo { version: 1, history: None }));
        // A newer layout may have any body
        assert_eq!(decode_marker(&encode_marker_with(7, b"whatever")), Marker::Newer(7));
        assert_eq!(decode_marker(&encode_marker_with(1, &[0; 16])), Marker::Damaged);
        assert_eq!(decode_marker(&encode_marker_with(0, &[])), Marker::Damaged);
        assert_eq!(decode_marker(b"hello"), Marker::Foreign);
        assert_eq!(decode_marker(&marker[..15]), Marker::Damaged);
        assert_eq!(decode_marker(&marker[..31]), Marker::Damaged);
        for bit in 64..MARKER_LEN * 8 {
            let mut bad = marker.clone();
            bad[bit / 8] ^= 1 << (bit % 8);
            assert_eq!(decode_marker(&bad), Marker::Damaged, "bit {}", bit);
        }
    }
}
