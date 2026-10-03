//! The marker file `IWDB`: magic, layout version, history id, CRC32C.

use std::fs;
use std::io;
use std::path::Path;

use super::{LAYOUT_VERSION, MARKER_LEN, MARKER_LEN_V1, MARKER_MAGIC, MARKER_NAME};
use crate::Error;
use crate::history::HistoryId;

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
    /// The layout version (1 to 4).
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
        (2..=4, MARKER_LEN) => {
            let mut id = [0u8; 16];
            id.copy_from_slice(&bytes[12..28]);
            Marker::Valid(MarkerInfo { version, history: Some(HistoryId(id)) })
        }
        (version, _) if version > LAYOUT_VERSION => Marker::Newer(version),
        _ => Marker::Damaged,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_checked() {
        let history = HistoryId([7; 16]);
        let marker = encode_marker(history);
        assert_eq!(marker.len(), MARKER_LEN);
        assert_eq!(decode_marker(&marker), Marker::Valid(MarkerInfo { version: 4, history: Some(history) }));
        let v3 = encode_marker_with(3, &history.0);
        assert_eq!(decode_marker(&v3), Marker::Valid(MarkerInfo { version: 3, history: Some(history) }));
        let v2 = encode_marker_with(2, &history.0);
        assert_eq!(decode_marker(&v2), Marker::Valid(MarkerInfo { version: 2, history: Some(history) }));
        let v1 = encode_marker_with(1, &[]);
        assert_eq!(v1.len(), MARKER_LEN_V1);
        assert_eq!(decode_marker(&v1), Marker::Valid(MarkerInfo { version: 1, history: None }));
        // A newer layout may have any body
        assert_eq!(decode_marker(&encode_marker_with(7, b"whatever")), Marker::Newer(7));
        assert_eq!(decode_marker(&encode_marker_with(5, b"whatever")), Marker::Newer(5));
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
