//! The file operations that change the data directory, behind a small
//! trait: the seam for fault injection. Tests wrap [`StdFs`] to make
//! writes, fsyncs, renames, directory syncs, checkpoint writes, deletions
//! or truncations fail; step 6 puts failpoints on the same calls.
//!
//! Reading (the WAL reader, loading checkpoints) uses `std::fs` directly:
//! a failed read changes nothing on disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// The operations that change files: the log writer's, and those of
/// checkpoints and recovery (step 5).
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
    /// file at `path` is untouched. The core syncs the directory only on a
    /// best-effort basis (upstream #32): callers that need the rename to be
    /// durable call [`sync_dir`](Self::sync_dir) afterwards.
    fn write_atomic(&self, path: &Path, write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>) -> io::Result<()>;
    /// Remove a file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Cut the file at `path` to `len` bytes and fsync it.
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()>;
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
        sync_dir(dir)
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
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}
