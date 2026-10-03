//! Cursors: where a paginated read continues (ADR 0021).
//!
//! A cursor is a keyset position (the sort key of the last result
//! returned) plus what it is valid for: the namespace (by id, so a
//! namespace dropped and created again under the same name doesn't
//! match), the history, the seq of the first page and a fingerprint of the
//! request. There is no MVCC snapshot to hold, so the next page is served
//! only if the namespace is still at that seq; otherwise the read fails
//! with `cursor_expired`.
//!
//! The encoding is a wire format (rule 4): `c1` and the hex digits of
//! version 1 of the binary layout below, with an FNV-1a checksum. Cursors
//! are opaque to clients and short-lived; a server that doesn't know a
//! version refuses it with `invalid_argument`.
//!
//! ```text
//! u8 version (1) | u64 namespace id | [u8; 16] history | u64 seq
//! | u64 request fingerprint | u32 count | count * (u32 len, utf-8 bytes)
//! | u64 FNV-1a of everything before
//! ```

use std::fmt;

use iwdb_storage::HistoryId;
use serde::Serialize;

use crate::Error;

const PREFIX: &str = "c1";
const VERSION: u8 = 1;
/// Longest cursor string accepted (a position holds a few ids).
const MAX_LEN: usize = 1 << 20;

/// An opaque position in a paginated read (`Answer::next`). Pass it back
/// in `QueryOptions::cursor` with the same request to get the next page.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cursor(String);

impl Cursor {
    /// A cursor as received from a client; checked when it is used.
    pub fn new(text: impl Into<String>) -> Self {
        Cursor(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a cursor holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    pub namespace_id: u64,
    pub history: HistoryId,
    pub seq: u64,
    pub request: u64,
    /// The sort key of the last result returned.
    pub after: Vec<String>,
}

impl State {
    pub fn encode(&self) -> Cursor {
        let mut bytes = vec![VERSION];
        bytes.extend_from_slice(&self.namespace_id.to_le_bytes());
        bytes.extend_from_slice(&self.history.0);
        bytes.extend_from_slice(&self.seq.to_le_bytes());
        bytes.extend_from_slice(&self.request.to_le_bytes());
        bytes.extend_from_slice(&(self.after.len() as u32).to_le_bytes());
        for key in &self.after {
            bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
        }
        let sum = fnv(&bytes);
        bytes.extend_from_slice(&sum.to_le_bytes());
        let mut text = String::with_capacity(PREFIX.len() + 2 * bytes.len());
        text.push_str(PREFIX);
        for b in bytes {
            text.push(char::from(HEX[usize::from(b >> 4)]));
            text.push(char::from(HEX[usize::from(b & 0xf)]));
        }
        Cursor(text)
    }

    pub fn decode(cursor: &Cursor) -> Result<State, Error> {
        let invalid = || Error::invalid("the cursor is invalid (not made by this server, or damaged)");
        let text = cursor.0.strip_prefix(PREFIX).ok_or_else(invalid)?;
        if text.len() > MAX_LEN || text.len() % 2 != 0 {
            return Err(invalid());
        }
        let bytes: Vec<u8> = text
            .as_bytes()
            .chunks(2)
            .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
            .collect::<Option<_>>()
            .ok_or_else(invalid)?;
        let (body, sum) = bytes.split_at(bytes.len().checked_sub(8).ok_or_else(invalid)?);
        if fnv(body).to_le_bytes() != sum {
            return Err(invalid());
        }
        let mut r = Reader(body);
        if r.take(1).ok_or_else(invalid)? != [VERSION] {
            return Err(Error::invalid("the cursor has a version this server doesn't know"));
        }
        let namespace_id = r.u64().ok_or_else(invalid)?;
        let history = HistoryId(r.take(16).ok_or_else(invalid)?.try_into().map_err(|_| invalid())?);
        let seq = r.u64().ok_or_else(invalid)?;
        let request = r.u64().ok_or_else(invalid)?;
        let count = r.u32().ok_or_else(invalid)?;
        let mut after = Vec::new();
        for _ in 0..count {
            let len = r.u32().ok_or_else(invalid)? as usize;
            let key = r.take(len).ok_or_else(invalid)?;
            after.push(String::from_utf8(key.to_vec()).map_err(|_| invalid())?);
        }
        if !r.0.is_empty() {
            return Err(invalid());
        }
        Ok(State { namespace_id, history, seq, request, after })
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// 64-bit FNV-1a: a stable hash (unlike std's `DefaultHasher`, which may
/// change between Rust versions), for checksums and fingerprints.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = Fingerprint::new();
    h.bytes(bytes);
    h.finish()
}

/// A stable fingerprint of a request, so that a cursor isn't used with
/// another request. Every field is written with its length, so different
/// requests don't run together.
pub(crate) struct Fingerprint(u64);

impl Fingerprint {
    pub fn new() -> Self {
        Fingerprint(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.u64(s.len() as u64);
        self.bytes(s.as_bytes());
        self
    }

    pub fn u64(&mut self, n: u64) -> &mut Self {
        self.bytes(&n.to_le_bytes());
        self
    }

    /// A value through its serde JSON form (the core's `Expr` and `Pattern`
    /// serialize deterministically: dicts sorted by key).
    pub fn json(&mut self, value: &impl Serialize) -> &mut Self {
        match serde_json::to_vec(value) {
            Ok(bytes) => {
                self.u64(bytes.len() as u64);
                self.bytes(&bytes);
            }
            // Unencodable values fail validation elsewhere; any constant
            // keeps the fingerprint defined
            Err(_) => {
                self.u64(u64::MAX);
            }
        }
        self
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Code;

    fn state() -> State {
        State {
            namespace_id: 7,
            history: HistoryId([3; 16]),
            seq: 42,
            request: 0xdead_beef,
            after: vec!["alice".into(), "".into(), "ü".into()],
        }
    }

    #[test]
    fn a_cursor_round_trips() {
        let cursor = state().encode();
        assert!(cursor.as_str().starts_with("c1"));
        assert_eq!(State::decode(&cursor), Ok(state()));
    }

    #[test]
    fn a_damaged_or_foreign_cursor_is_invalid() {
        let text = state().encode().0;
        let mut damaged = text.clone().into_bytes();
        let last = damaged.len() - 1;
        damaged[last] = if damaged[last] == b'0' { b'1' } else { b'0' };
        for bad in [
            String::new(),
            "c1".into(),
            "c2".to_owned() + &text[2..],
            text[..text.len() - 2].to_owned(),
            String::from_utf8(damaged).expect("ascii"),
            text.to_uppercase(),
        ] {
            let e = State::decode(&Cursor(bad.clone())).expect_err(&bad);
            assert_eq!(e.code(), Code::InvalidArgument);
        }
    }

    #[test]
    fn fingerprints_keep_fields_apart() {
        let a = Fingerprint::new().str("ab").str("c").finish();
        let b = Fingerprint::new().str("a").str("bc").finish();
        assert_ne!(a, b);
        assert_eq!(a, Fingerprint::new().str("ab").str("c").finish());
    }
}
