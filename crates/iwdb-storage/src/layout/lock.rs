//! The data directory's lock file `LOCK`.

use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;

use super::LOCK_NAME;
use crate::Error;

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
/// blocking; [`Error::Locked`] if it is still held after about 80 ms of
/// retries.
pub fn lock_file(file: &File, path: &Path, exclusive: bool) -> Result<(), Error> {
    // A child being spawned by another thread holds a copy of every open
    // file until its exec, so a just-released flock can look held briefly
    let attempt = || {
        if exclusive { file.try_lock() } else { file.try_lock_shared() }
    };
    for wait in LOCK_RETRIES_MS.iter().map(|ms| Some(std::time::Duration::from_millis(*ms))).chain([None]) {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) => match wait {
                Some(wait) => std::thread::sleep(wait),
                None => return Err(Error::Locked { path: path.to_path_buf() }),
            },
            Err(TryLockError::Error(e)) => return Err(Error::io("lock", path, e)),
        }
    }
    Err(Error::Locked { path: path.to_path_buf() })
}
