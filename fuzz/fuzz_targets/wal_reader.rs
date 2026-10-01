//! Fuzz the WAL reader: arbitrary bytes must never panic, hang or allocate
//! without bound, whatever part of the format they land in.
//!
//! The first input byte picks what the rest is:
//! - 0: a whole segment file (as the last segment and as an earlier one);
//! - 1: the frames after a valid segment header (format 2), and after a
//!   format 1 header;
//! - 2..: the payload of a frame with a valid checksum, of record kind
//!   `byte - 2` (so that the payload decoder is reached, which random
//!   frames almost never do).

#![no_main]

use std::path::Path;

use iwdb_storage::format::{encode_frame, encode_segment_header, encode_segment_header_version, FrameHeader, FORMAT_VERSION};
use iwdb_storage::read_segment;
use libfuzzer_sys::fuzz_target;

/// A frame of the current format (`documentation/formats/wal.md`).
fn frame(seq: u64, synced_seq: u64, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_frame(&mut out, FORMAT_VERSION, FrameHeader { seq, synced_seq, time: 0, kind }, payload);
    out
}

fuzz_target!(|data: &[u8]| {
    let path = Path::new("00000000000000000001.wal");
    let Some((&mode, rest)) = data.split_first() else { return };
    match mode {
        0 => {
            let _ = read_segment(path, rest, 1, true);
            let _ = read_segment(path, rest, 1, false);
        }
        1 => {
            for version in [FORMAT_VERSION, 1] {
                let mut segment = encode_segment_header_version(1, version).to_vec();
                segment.extend_from_slice(rest);
                if let Ok((records, end)) = read_segment(path, &segment, 1, true) {
                    assert!(end.valid_len <= end.file_len);
                    assert_eq!(end.next_seq, 1 + records.len() as u64);
                }
            }
        }
        kind => {
            let mut segment = encode_segment_header(1).to_vec();
            segment.extend_from_slice(&frame(1, 0, kind - 2, rest));
            let _ = read_segment(path, &segment, 1, true);
        }
    }
});
