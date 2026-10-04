//! The files of an import (ADR 0033): a new namespace whose first state is
//! one checkpoint at [`IMPORT_SEQ`], with no WAL record before it.
//!
//! An import is all or nothing across crashes:
//!
//! 1. [`stage`]: the checkpoint is written to a temporary file in `ns/`
//!    (`import-*.tmp`) and fsynced. A crash leaves a `.tmp` file, which
//!    the next open removes.
//! 2. [`place`]: in the new namespace's directory (made by
//!    [`create_ns_dir`](crate::layout::create_ns_dir)), the file becomes
//!    `checkpoints/import.staged`. That is not a checkpoint, so a crash
//!    leaves a directory without data, which the next open removes.
//! 3. The create event is appended to the namespace log: the commit point.
//! 4. [`finish`]: `import.staged` becomes the checkpoint at
//!    [`IMPORT_SEQ`]. Recovery calls it too ([`read_namespace`]), so an
//!    import whose event is logged is always finished.
//!
//! [`read_namespace`]: crate::recovery::read_namespace

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use iwdb_engine::{Namespace, codec};

pub use iwdb_engine::plain::IMPORT_SEQ;

use crate::Error;
use crate::checkpoint::{checkpoint_name, list_checkpoints};
use crate::io::{CHUNK, LogFile, LogFs};
use crate::layout::{NsPaths, TEMP_SUFFIX};

/// The name of a staged import in a namespace's `checkpoints/`.
pub const STAGED_NAME: &str = "import.staged";

/// Writes to a [`LogFile`], counting the bytes and reporting them to a
/// callback.
struct Counting<'a, W> {
    file: W,
    written: u64,
    progress: &'a mut dyn FnMut(u64),
}

impl<W: LogFile> Write for Counting<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write_all(buf)?;
        self.written += buf.len() as u64;
        (self.progress)(self.written);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A new temporary file name in `ns/`: unique within the process, and
/// across processes by time (only one process has the store open).
fn temp_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("import-{}-{}-{}{}", std::process::id(), nanos, NEXT.fetch_add(1, Ordering::Relaxed), TEMP_SUFFIX)
}

/// Write `namespace` as a checkpoint into a new temporary file in
/// `ns_root` and fsync it; `progress` gets the bytes written so far (about
/// every MiB). Returns the file's path. The namespace's seq must be
/// [`IMPORT_SEQ`]. On error the file is removed when possible.
pub fn stage<F: LogFs>(
    fs: &F,
    ns_root: &Path,
    namespace: &Namespace,
    progress: &mut dyn FnMut(u64),
) -> Result<PathBuf, Error> {
    if namespace.seq() != IMPORT_SEQ {
        return Err(Error::InvalidImport {
            reason: format!("an import's namespace is at seq {}, not {}", IMPORT_SEQ, namespace.seq()),
        });
    }
    let path = ns_root.join(temp_name());
    let written = (|| {
        let file = fs.create(&path).map_err(|e| Error::io("create", &path, e))?;
        let mut out = BufWriter::with_capacity(CHUNK, Counting { file, written: 0, progress });
        codec::write_binary(namespace.graph(), &namespace.graph_meta(), &mut out)?;
        let counting = out.into_inner().map_err(|e| Error::io("write", &path, e.into_error()))?;
        let mut file = counting.file;
        file.sync().map_err(|e| Error::io("fsync", &path, e))
    })();
    match written {
        Ok(()) => Ok(path),
        Err(e) => {
            let _ = fs.remove_file(&path);
            Err(e)
        }
    }
}

/// Move the staged checkpoint `staged` (from [`stage`]) into the new
/// namespace directory `paths` as `checkpoints/import.staged`, and sync
/// that directory and `staged`'s. The namespace log must not list the
/// namespace yet.
pub fn place<F: LogFs>(fs: &F, staged: &Path, paths: &NsPaths) -> Result<(), Error> {
    let target = paths.checkpoints.join(STAGED_NAME);
    fs.rename(staged, &target).map_err(|e| Error::io("rename", staged, e))?;
    fs.sync_dir(&paths.checkpoints).map_err(|e| Error::io("sync directory", &paths.checkpoints, e))?;
    if let Some(parent) = staged.parent() {
        fs.sync_dir(parent).map_err(|e| Error::io("sync directory", parent, e))?;
    }
    Ok(())
}

/// Make sure the WAL archive holds the checkpoint an imported namespace
/// starts from (ADR 0033): if the namespace in `paths` has a checkpoint at
/// [`IMPORT_SEQ`] (or a staged import) and neither its WAL nor the archive
/// holds record 1, copy it into the archive (synced). Returns whether it
/// copied. The import calls it after its create event; a store's open
/// calls it for every namespace, which covers a crash in between, and an
/// archive set up after the import.
pub fn archive_base<F: LogFs>(archive: &crate::archive::Archive<F>, id: u64, paths: &NsPaths) -> Result<bool, Error> {
    let checkpoint = paths.checkpoints.join(checkpoint_name(IMPORT_SEQ));
    let staged = paths.checkpoints.join(STAGED_NAME);
    let source = if checkpoint.is_file() {
        checkpoint
    } else if staged.is_file() {
        staged
    } else {
        return Ok(false);
    };
    let version = crate::archive::ARCHIVE_VERSION;
    let starts_at_1 = |segments: Vec<(u64, PathBuf)>| segments.first().is_some_and(|(seq, _)| *seq <= 1);
    if starts_at_1(crate::reader::list_segments(&paths.wal)?)
        || starts_at_1(crate::archive::archive_segments(archive.dir(), version, id)?)
        || crate::archive::archive_checkpoints(archive.dir(), version, id)?.iter().any(|(seq, _)| *seq == IMPORT_SEQ)
    {
        return Ok(false);
    }
    archive.copy_checkpoint(id, IMPORT_SEQ, &source)?;
    Ok(true)
}

/// Finish an import whose create event is logged: if `checkpoints/` holds
/// `import.staged`, rename it to the checkpoint at [`IMPORT_SEQ`] when the
/// namespace has no checkpoint, and remove it otherwise (an import that
/// was finished already); then sync. Returns whether there was a staged
/// import to finish (`false` also after removing a leftover).
pub fn finish<F: LogFs>(fs: &F, paths: &NsPaths) -> Result<bool, Error> {
    let staged = paths.checkpoints.join(STAGED_NAME);
    if !staged.is_file() {
        return Ok(false);
    }
    let finished = if list_checkpoints(&paths.checkpoints)?.is_empty() {
        let target = paths.checkpoints.join(checkpoint_name(IMPORT_SEQ));
        fs.rename(&staged, &target).map_err(|e| Error::io("rename", &staged, e))?;
        true
    } else {
        fs.remove_file(&staged).map_err(|e| Error::io("remove", &staged, e))?;
        false
    };
    fs.sync_dir(&paths.checkpoints).map_err(|e| Error::io("sync directory", &paths.checkpoints, e))?;
    Ok(finished)
}
