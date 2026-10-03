//! The file operations that change the data directory, behind a small
//! trait: the seam for fault injection. With the `failpoints` feature,
//! `failpoint::FailFs` wraps any [`LogFs`] and makes writes, fsyncs,
//! renames, directory syncs, checkpoint writes, deletions or truncations
//! fail, pause, panic or abort. [`StdFs`] itself has no
//! failpoints.
//!
//! Reading (the WAL reader, loading checkpoints) uses `std::fs` directly:
//! a failed read changes nothing on disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::Error;

/// The operations that change files: the log writer's, and those of
/// checkpoints and recovery.
pub trait LogFs {
    type File: LogFile;

    /// Create `path` for writing, truncating it if it exists.
    fn create(&self, path: &Path) -> io::Result<Self::File>;
    /// Open an existing file for appending.
    fn open_append(&self, path: &Path) -> io::Result<Self::File>;
    /// Rename `from` to `to`, replacing `to`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Make the directory's entries (created, renamed and removed files)
    /// durable.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Write `path` atomically through `write`, with the core's
    /// `format::write_atomic`: into a temporary file next to it (named
    /// `.<name>.<pid>.<n>.tmp`), fsynced, then renamed over `path`. On
    /// error the temporary file is removed when possible and a previous
    /// file at `path` is untouched. On Unix the core then fsyncs the
    /// directory and returns its error, so `Ok` means the rename is durable
    /// (since `3b15149`, upstream #32). An error can still come after the
    /// rename: the new file may then be in place but not durable.
    fn write_atomic(&self, path: &Path, write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>) -> io::Result<()>;
    /// Remove a file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Cut the file at `path` to `len` bytes and fsync it.
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()>;
    /// Create the directory `path` (not its parents); `AlreadyExists` if
    /// there is one. Not durable until its parent is synced.
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    /// Remove the directory `path` and everything in it. Not durable until
    /// its parent is synced.
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
}

/// An open log file.
pub trait LogFile {
    /// Write all of `bytes` at the end of the file. On error, any prefix
    /// of `bytes` may have been written.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Make the file's contents and size durable (`File::sync_all`: `fsync`
    /// on Linux, `fcntl(F_FULLFSYNC)` on macOS).
    fn sync(&mut self) -> io::Result<()>;
}

/// The real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFs;

impl LogFs for StdFs {
    type File = File;

    fn create(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().write(true).create(true).truncate(true).open(path)
    }

    fn open_append(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().append(true).open(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        fsync_dir(dir)
    }

    fn write_atomic(&self, path: &Path, write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
        ironweaver_core::format::write_atomic(path, |out| write(out))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(len)?;
        file.sync_all()
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}

impl LogFile for File {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        Write::write_all(self, bytes)?;
        self.flush()
    }

    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
}

/// Sync a directory: open it and `sync_all` it (on macOS, `F_FULLFSYNC` on
/// the directory). Windows can't open directories this way; there it does
/// nothing (Windows is not a supported platform yet).
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// The size of the chunks files are written, copied and compared in.
pub(crate) const CHUNK: usize = 1 << 20;

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
pub(crate) fn copy_file<F: LogFs>(fs: &F, source: &Path, target: &Path) -> Result<(u64, u32), Error> {
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
