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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    /// file at `path` is untouched. Then the directory is fsynced and its
    /// error returned, so `Ok` means the rename is durable: on Unix by the
    /// core (since `3b15149`, upstream #32), on Windows by [`StdFs`] after
    /// the core's call (ADR 0058; the core skips it there, upstream #71).
    /// An error can still come after the rename: the new file may then be
    /// in place but not durable.
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
        ironweaver_core::format::write_atomic(path, |out| write(out))?;
        // Workaround for upstream #71: the core syncs the directory on Unix
        // only. Remove once it does on Windows too (upstream check)
        #[cfg(windows)]
        if let Some(dir) = path.parent() {
            let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
            fsync_dir(dir).map_err(|e| {
                io::Error::new(e.kind(), format!("saved, but syncing the directory {} failed: {}", dir.display(), e))
            })?;
        }
        Ok(())
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
/// the directory). On Windows, `FlushFileBuffers` on a handle opened with
/// `FILE_FLAG_BACKUP_SEMANTICS` (the only way to open a directory) and
/// write access (which the flush needs); on NTFS that makes the
/// directory's entries durable (ADR 0058). An error is returned, never
/// ignored, on every platform (ADR 0005).
#[cfg(not(windows))]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(windows)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_FLAG_BACKUP_SEMANTICS` (winbase.h), part of the Win32 ABI.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new().write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir)?.sync_all()
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

/// How long [`open_unless_removed`] waits for a "delete pending" name to go.
const DELETE_PENDING_WAIT: Duration = Duration::from_millis(500);

/// Open `path` through `open`, for a file that the checkpointer may remove
/// after it was listed: `Ok(None)` if it was. On Unix, and on Windows when
/// the removal had POSIX semantics, the name goes at once (`NotFound`). On
/// Windows a removal can also leave the name "delete pending" until the
/// last handle closes (another process's too, such as a virus scanner's:
/// ADR 0058), and opening it fails with `PermissionDenied`; that is
/// retried for up to [`DELETE_PENDING_WAIT`]. A file still refused then is
/// an error, as a real permission error is everywhere.
pub(crate) fn open_unless_removed<T>(
    path: &Path,
    mut open: impl FnMut(&Path) -> io::Result<T>,
) -> io::Result<Option<T>> {
    let deadline = Instant::now() + DELETE_PENDING_WAIT;
    let mut pause = Duration::from_millis(1);
    loop {
        match open(path) {
            Ok(file) => return Ok(Some(file)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied && Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) fn write_atomic<F: LogFs>(fs: &F, path: &Path, bytes: &[u8]) -> Result<(), Error> {
    fs.write_atomic(path, &mut |out| out.write_all(bytes)).map_err(|e| Error::io("write", path, e))
}

/// A limit on how fast a copy writes (online backups, ADR 0055), and a
/// counter of the bytes it wrote. [`pass`](Self::pass) is called after
/// each chunk: it sleeps while the bytes written so far are ahead of the
/// rate, measured from the first call. So a burst is at most one chunk
/// ([`CHUNK`]), and a copy of `n` bytes takes at least `(n - CHUNK) / rate`.
#[derive(Debug)]
pub struct Throttle<'a> {
    /// Bytes per second; `None`: no limit.
    rate: Option<u64>,
    start: Option<Instant>,
    written: u64,
    /// Added to as bytes are written (the `iwdb_backup_bytes_total` metric).
    progress: Option<&'a AtomicU64>,
}

impl<'a> Throttle<'a> {
    /// At most `rate` bytes per second (`None` or 0: no limit), counting
    /// into `progress` if given.
    pub fn new(rate: Option<u64>, progress: Option<&'a AtomicU64>) -> Self {
        Throttle { rate: rate.filter(|r| *r > 0), start: None, written: 0, progress }
    }

    /// No limit, no counter.
    pub fn none() -> Self {
        Throttle::new(None, None)
    }

    /// The bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// `n` more bytes were written: wait until the rate allows them.
    pub fn pass(&mut self, n: u64) {
        let start = *self.start.get_or_insert_with(Instant::now);
        self.written += n;
        if let Some(progress) = self.progress {
            progress.fetch_add(n, Ordering::Relaxed);
        }
        if let Some(rate) = self.rate {
            let due = Duration::from_secs_f64(self.written as f64 / rate as f64);
            if let Some(ahead) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(ahead);
            }
        }
    }
}

/// Create `target` with `content`, in chunks, and fsync it.
pub(crate) fn write_file<F: LogFs>(fs: &F, target: &Path, content: &[u8]) -> Result<(), Error> {
    write_file_throttled(fs, target, content, &mut Throttle::none())
}

/// [`write_file`], each chunk through `throttle`.
pub(crate) fn write_file_throttled<F: LogFs>(
    fs: &F,
    target: &Path,
    content: &[u8],
    throttle: &mut Throttle<'_>,
) -> Result<(), Error> {
    let mut file = fs.create(target).map_err(|e| Error::io("create", target, e))?;
    for chunk in content.chunks(CHUNK) {
        file.write_all(chunk).map_err(|e| Error::io("write", target, e))?;
        throttle.pass(chunk.len() as u64);
    }
    file.sync().map_err(|e| Error::io("fsync", target, e))
}

/// Copy `source` to a new file `target` in chunks, and fsync it. Returns
/// its length and CRC32C.
pub(crate) fn copy_file<F: LogFs>(fs: &F, source: &Path, target: &Path) -> Result<(u64, u32), Error> {
    copy_file_throttled(fs, source, target, &mut Throttle::none())
}

/// [`copy_file`], each chunk through `throttle`.
pub(crate) fn copy_file_throttled<F: LogFs>(
    fs: &F,
    source: &Path,
    target: &Path,
    throttle: &mut Throttle<'_>,
) -> Result<(u64, u32), Error> {
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
        throttle.pass(n as u64);
    }
    file.sync().map_err(|e| Error::io("fsync", target, e))?;
    Ok((len, crc))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file removed meanwhile is `None`; on Windows a "delete pending"
    /// refusal is waited out, a lasting one (and any elsewhere) is an error.
    #[test]
    fn a_file_removed_meanwhile_is_none() {
        let path = Path::new("segment");
        let refused = || io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(open_unless_removed(path, |_| Ok(1)).unwrap(), Some(1));
        assert_eq!(open_unless_removed(path, |_| Err::<(), _>(io::ErrorKind::NotFound.into())).unwrap(), None);
        let mut calls = 0;
        let pending = open_unless_removed(path, |_| {
            calls += 1;
            if calls < 3 { Err::<(), _>(refused()) } else { Err(io::ErrorKind::NotFound.into()) }
        });
        if cfg!(windows) {
            assert_eq!((pending.unwrap(), calls), (None, 3));
        } else {
            assert_eq!((pending.unwrap_err().kind(), calls), (io::ErrorKind::PermissionDenied, 1));
        }
        let start = Instant::now();
        let lasting = open_unless_removed(path, |_| Err::<(), _>(refused()));
        assert_eq!(lasting.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        if cfg!(windows) {
            assert!(start.elapsed() >= DELETE_PENDING_WAIT);
        }
    }

    /// The real directory sync works on the file system tests run on (NTFS
    /// on the Windows runner, ADR 0058), and so does `write_atomic` with
    /// it, over a file that a reader has open.
    #[test]
    fn directories_are_synced_for_real() {
        let dir = tempfile::tempdir().unwrap();
        StdFs.sync_dir(dir.path()).unwrap();
        let sub = dir.path().join("a b");
        StdFs.create_dir(&sub).unwrap();
        StdFs.sync_dir(&sub).unwrap();
        let path = sub.join("file");
        StdFs.write_atomic(&path, &mut |out| out.write_all(b"one")).unwrap();
        let mut reader = File::open(&path).unwrap();
        StdFs.write_atomic(&path, &mut |out| out.write_all(b"two")).unwrap();
        let mut old = String::new();
        reader.read_to_string(&mut old).unwrap();
        assert_eq!((old.as_str(), fs::read(&path).unwrap()), ("one", b"two".to_vec()));
        // A directory that isn't there is an error, not a silent success
        assert_eq!(StdFs.sync_dir(&dir.path().join("missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// The workaround for upstream #71: on Windows the core's
    /// `write_atomic` doesn't sync the directory, `StdFs` does, and reports
    /// a failure. The directory denies the current user writing its
    /// extended attributes (`icacls`): files can be created and renamed in
    /// it, but it can't be opened for writing, which `FlushFileBuffers`
    /// needs.
    #[cfg(windows)]
    #[test]
    fn write_atomic_reports_a_failed_directory_sync_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let user = std::process::Command::new("whoami").output().unwrap();
        let user = String::from_utf8_lossy(&user.stdout).trim().to_owned();
        let icacls = |args: &[&str]| {
            let out = std::process::Command::new("icacls").arg(&sub).args(args).output().unwrap();
            assert!(out.status.success(), "icacls: {}", String::from_utf8_lossy(&out.stdout));
        };
        icacls(&["/deny", &format!("{}:(WEA)", user)]);
        let synced = StdFs.sync_dir(&sub);
        let result = StdFs.write_atomic(&sub.join("file"), &mut |out| out.write_all(b"data"));
        icacls(&["/remove:d", &user]);
        if synced.is_ok() {
            // An account whose privileges override the ACL: nothing to show
            return;
        }
        let e = result.unwrap_err();
        assert!(e.to_string().contains("syncing the directory"), "{}", e);
        // The rename happened: the new file is in place, just not known durable
        assert_eq!(fs::read(sub.join("file")).unwrap(), b"data");
    }

    #[test]
    fn a_throttle_holds_a_copy_to_its_rate_and_counts() {
        let progress = AtomicU64::new(0);
        // 4 chunks at 8 chunks a second: the last three wait, about 0.5 s
        let mut throttle = Throttle::new(Some(8 * CHUNK as u64), Some(&progress));
        let start = Instant::now();
        for _ in 0..4 {
            throttle.pass(CHUNK as u64);
        }
        assert!(start.elapsed() >= Duration::from_millis(450), "{:?}", start.elapsed());
        assert_eq!((throttle.written(), progress.load(Ordering::Relaxed)), (4 * CHUNK as u64, 4 * CHUNK as u64));
        // No limit (and 0 is none): no waiting
        for rate in [None, Some(0)] {
            let mut free = Throttle::new(rate, None);
            let start = Instant::now();
            for _ in 0..64 {
                free.pass(CHUNK as u64);
            }
            // Throttled, 64 chunks would take 8 s at the rate above
            assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
        }
    }
}
