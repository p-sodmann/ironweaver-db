//! Simulating an OS crash or power loss after a kill.
//!
//! A kill -9 loses nothing that was written: the page cache survives.
//! An OS crash loses what was written after the last completed fsync, and
//! the unsynced end of a file can come back partly: cut off, or with a lost
//! page in the middle and later data intact (write-back order is
//! arbitrary). The simulation does that to the last WAL segment, the only
//! file with unsynced data under `always` and `group` (earlier segments are
//! synced before a rotation; a new segment's header is synced before it is
//! renamed into place):
//!
//! - the durable length is the largest length the sync log recorded for
//!   the segment (an upper bound of what its fsyncs covered, see
//!   `child.rs`), and at least its header. With `off` only explicit syncs
//!   (`Store::sync`, `checkpoint`) fsync, and not the header;
//! - after it, the simulation truncates (at a frame boundary or inside a
//!   frame), zeroes a range and keeps what follows, or both.
//!
//! Not simulated: loss in other files, lost directory entries (renames,
//! removals not yet synced), and with `off` loss in earlier segments or in
//! a header while later frames survive. Those are outside what the
//! guarantees promise for `off`, and the data directory's fsyncs cover
//! them for `always` and `group`.

use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use iwdb_storage::format::{FRAME_HEADER_LEN, SEGMENT_HEADER_LEN};

use crate::rng::Rng;
use crate::script::Policy;

/// What the simulation did to the last segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsCrash {
    pub path: PathBuf,
    /// The segment's length at the crash, and the length known durable.
    pub len: u64,
    pub durable: u64,
    /// Bytes `from..to` were zeroed.
    pub zeroed: Option<(u64, u64)>,
    /// The file was cut to this length.
    pub cut: Option<u64>,
}

/// The largest length the sync log recorded for `path`. Paths compare as
/// paths, not strings: the store and the harness build them with
/// different separators on Windows (`ns/…` joined to `D:\…`).
pub fn synced_len(sync_log: &Path, path: &Path) -> io::Result<u64> {
    let text = match fs::read_to_string(sync_log) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter_map(|line| line.rsplit_once(' '))
        .filter(|(p, _)| Path::new(p) == path)
        .filter_map(|(_, len)| len.parse::<u64>().ok())
        .max()
        .unwrap_or(0))
}

/// Offsets where frames end in a segment's bytes, from `from` on, parsed
/// from their length fields (stops at anything that doesn't parse).
fn frame_ends(bytes: &[u8], from: usize) -> Vec<u64> {
    let mut ends = Vec::new();
    let mut pos = SEGMENT_HEADER_LEN;
    while pos + FRAME_HEADER_LEN <= bytes.len() {
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        pos += FRAME_HEADER_LEN + len;
        if pos > bytes.len() {
            break;
        }
        if pos > from {
            ends.push(pos as u64);
        }
    }
    ends
}

/// Lose part of the unsynced end of the last segment in `wal_dir`, as an
/// OS crash could. Returns `None` if nothing after the durable length was
/// written.
pub fn simulate(wal_dir: &Path, sync_log: &Path, policy: Policy, rng: &mut Rng) -> io::Result<Option<OsCrash>> {
    let segments = iwdb_storage::list_segments(wal_dir).map_err(io::Error::other)?;
    let Some((_, path)) = segments.last() else { return Ok(None) };
    let bytes = fs::read(path)?;
    let len = bytes.len() as u64;
    let header = SEGMENT_HEADER_LEN as u64;
    // `off` fsyncs only on an explicit sync, and not a new segment's header
    let synced = synced_len(sync_log, path)?;
    let durable = if policy == Policy::Off { synced } else { synced.max(header) };
    if durable >= len {
        return Ok(None);
    }
    let mut crash = OsCrash { path: path.clone(), len, durable, zeroed: None, cut: None };
    let mode = rng.below(3);
    // Zero a range (a lost page) and keep what follows. Headers are left
    // alone: with `off` a lost header with frames after it is corruption,
    // which the guarantees allow for `off` but the checks here don't expect.
    if mode != 0 && len > durable.max(header) + 1 {
        let from = rng.range(durable.max(header), len - 1);
        // A few bytes (later frames survive), or up to the end
        let to = if rng.chance(1, 2) { rng.range(from + 1, len.min(from + 64)) } else { rng.range(from + 1, len) };
        crash.zeroed = Some((from, to));
    }
    // Cut: at a frame boundary or anywhere
    if mode != 1 {
        let ends = frame_ends(&bytes, durable as usize);
        let at = if !ends.is_empty() && rng.chance(1, 2) { *rng.pick(&ends) } else { rng.range(durable, len - 1) };
        crash.cut = Some(at);
    }
    let mut file = OpenOptions::new().write(true).open(path)?;
    if let Some((from, to)) = crash.zeroed {
        file.seek(SeekFrom::Start(from))?;
        file.write_all(&vec![0; (to - from) as usize])?;
    }
    if let Some(at) = crash.cut {
        file.set_len(at)?;
    }
    Ok(Some(crash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sync_log_gives_the_largest_recorded_length() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("synclog");
        assert_eq!(synced_len(&log, Path::new("/a")).unwrap(), 0);
        fs::write(&log, "/a b/x.wal 24\n/a b/x.wal 90\n/a b/y.wal 500\n/a b/x.wal 60\n").unwrap();
        assert_eq!(synced_len(&log, Path::new("/a b/x.wal")).unwrap(), 90);
        assert_eq!(synced_len(&log, Path::new("/a b/z.wal")).unwrap(), 0);
        // The same file, written with other separators (Windows)
        #[cfg(windows)]
        {
            fs::write(&log, "D:\\w\\ns/1/wal\\x.wal 90\n").unwrap();
            assert_eq!(synced_len(&log, Path::new(r"D:\w/ns\1\wal/x.wal")).unwrap(), 90);
        }
    }

    #[test]
    fn frames_are_found_by_their_lengths() {
        let mut bytes = vec![0u8; SEGMENT_HEADER_LEN];
        for payload in [3usize, 10] {
            let mut frame = vec![0u8; FRAME_HEADER_LEN + payload];
            frame[..4].copy_from_slice(&(payload as u32).to_le_bytes());
            bytes.extend_from_slice(&frame);
        }
        let first = (SEGMENT_HEADER_LEN + FRAME_HEADER_LEN + 3) as u64;
        assert_eq!(frame_ends(&bytes, 0), vec![first, first + FRAME_HEADER_LEN as u64 + 10]);
        assert_eq!(frame_ends(&bytes, first as usize), vec![first + FRAME_HEADER_LEN as u64 + 10]);
    }
}
