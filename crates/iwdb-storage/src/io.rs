//! The file operations the log writer uses, behind a small trait: the seam
//! for fault injection. Tests wrap [`StdFs`] to make writes, fsyncs,
//! renames or directory syncs fail; step 6 puts failpoints on the same
//! calls.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;

/// Directory operations of the log writer.
pub trait LogFs {
    type File: LogFile;

    /// Create `path` for writing, truncating it if it exists.
    fn create(&self, path: &Path) -> io::Result<Self::File>;
    /// Open an existing file for appending.
    fn open_append(&self, path: &Path) -> io::Result<Self::File>;
    /// Rename `from` to `to`, replacing `to`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Make the directory's entries (created and renamed files) durable.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
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
}

impl LogFile for File {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        io::Write::write_all(self, bytes)?;
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
