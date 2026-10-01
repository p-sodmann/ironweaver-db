//! History ids: which history of commits a data directory, a backup or a
//! WAL archive holds (data-dir layout 2, ADR 0009).
//!
//! A history begins when a data directory is created, and a new one begins
//! with every restore: after a restore to seq `N`, the restored store's
//! commits `N + 1, ...` differ from the ones the original store made with
//! the same seqs. The id keeps the two apart: a WAL archive belongs to one
//! history, and a store only archives into, and a restore only combines,
//! files of the same history.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A random 128-bit history id, written as 32 lowercase hex digits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistoryId(pub [u8; 16]);

impl HistoryId {
    /// A new random id. The randomness comes from the standard library's
    /// hash seeds (taken from the OS when a thread first needs one), mixed
    /// with the time, the process id and a counter: unique for our
    /// purposes, not for cryptography. No new dependency for it.
    pub fn random() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let half = |salt: u64| {
            let seed = (salt, count, nanos, std::process::id(), std::thread::current().id());
            RandomState::new().hash_one(seed).to_le_bytes()
        };
        let (a, b) = (half(0x1157), half(0x7031));
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&a);
        id[8..].copy_from_slice(&b);
        HistoryId(id)
    }
}

impl fmt::Display for HistoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

impl FromStr for HistoryId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let invalid = || format!("'{}' is not a history id (32 hex digits)", s);
        if s.len() != 32 || !s.is_ascii() {
            return Err(invalid());
        }
        let mut id = [0u8; 16];
        for (i, byte) in id.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
        }
        Ok(HistoryId(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_differ_and_round_trip() {
        let ids: Vec<HistoryId> = (0..100).map(|_| HistoryId::random()).collect();
        let distinct: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(distinct.len(), ids.len());
        for id in ids {
            assert_eq!(id.to_string().len(), 32);
            assert_eq!(id.to_string().parse::<HistoryId>(), Ok(id));
        }
        assert!("xyz".parse::<HistoryId>().is_err());
        assert!("0123456789abcdef0123456789abcdeg".parse::<HistoryId>().is_err());
    }
}
