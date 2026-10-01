//! Idempotency keys (step 8, ADR 0015): a commit made with a key is
//! applied once; a retry with the same key returns the original
//! [`CommitResult`] instead of committing again.
//!
//! - [`IdempotencyKey`]: 1 to [`MAX_KEY_LEN`] bytes of UTF-8, chosen by the
//!   client (a UUID, a request id).
//! - [`Keyed`]: what a keyed commit's record carries besides its change
//!   (WAL format 3): the key, the request's fingerprint and the result, so
//!   that replaying the log rebuilds the table exactly.
//! - [`KeyTable`]: the namespace's most recent [`KEY_TABLE_CAPACITY`]
//!   keyed commits, part of its state like the catalog: changed only by
//!   applying records, saved in checkpoints (`iwdb.keys`, data-dir layout
//!   3), so it survives restarts, checkpoints, backups and restores.
//!
//! A retry with the same key and the **same request** (equal
//! [fingerprints](fingerprint_data)) returns the original result with
//! [`CommitResult::deduplicated`] set, and commits nothing. A retry with
//! the same key and a **different request** fails with
//! [`Error::IdempotencyKeyReused`]: the key names one request. Once a key
//! is evicted (more than [`KEY_TABLE_CAPACITY`] keyed commits later), it is
//! unknown again and a commit with it applies.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use ironweaver_core::{EdgeId, Value};
use serde::{Deserialize, Serialize};

use crate::mutation::{CatalogChange, CommitResult, Mutation, Target};
use crate::{CommitTime, Error};

/// The longest idempotency key, in bytes.
pub const MAX_KEY_LEN: usize = 255;

/// How many keyed commits a namespace remembers. When a keyed commit
/// would make the table larger, the entry with the lowest seq is evicted.
/// Part of the data-dir layout: changing it changes what replay produces,
/// so it is a constant, not an option (step 9 may make it a catalog
/// setting, logged like any catalog change).
pub const KEY_TABLE_CAPACITY: usize = 10_000;

/// A client's name for one request: 1 to [`MAX_KEY_LEN`] bytes of UTF-8.
/// Equal keys are equal byte strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// A key, or [`Error::InvalidIdempotencyKey`] if it is empty or longer
    /// than [`MAX_KEY_LEN`] bytes.
    pub fn new(key: impl Into<String>) -> Result<Self, Error> {
        let key = key.into();
        if key.is_empty() || key.len() > MAX_KEY_LEN {
            return Err(Error::InvalidIdempotencyKey {
                reason: format!("a key has 1 to {} bytes, this one {}", MAX_KEY_LEN, key.len()),
            });
        }
        Ok(IdempotencyKey(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IdempotencyKey {
    type Error = Error;
    fn try_from(key: String) -> Result<Self, Error> {
        IdempotencyKey::new(key)
    }
}

impl From<IdempotencyKey> for String {
    fn from(key: IdempotencyKey) -> String {
        key.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// What the record of a commit made with an idempotency key carries
/// besides its change (WAL format 3): replaying it puts the same entry
/// into the [`KeyTable`] that committing it did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keyed {
    pub key: IdempotencyKey,
    /// The request's [fingerprint](fingerprint_data).
    pub fingerprint: u32,
    /// The result's edge ids and versions (its seq is the record's, its
    /// time the frame's).
    pub edge_ids: Vec<EdgeId>,
    pub versions: Vec<(Target, u64)>,
}

/// A keyed commit the namespace remembers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEntry {
    pub key: IdempotencyKey,
    pub fingerprint: u32,
    /// The original result (`deduplicated` false).
    pub result: CommitResult,
}

/// The namespace's recent keyed commits: at most [`KEY_TABLE_CAPACITY`],
/// the ones with the highest seqs. Deterministic: the same records give
/// the same table, whichever path applied them (commit, recovery,
/// checkpointer, restore, verify).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyTable {
    by_seq: BTreeMap<u64, KeyEntry>,
    by_key: HashMap<IdempotencyKey, u64>,
}

impl KeyTable {
    pub fn new() -> Self {
        KeyTable::default()
    }

    /// The entry of `key`, if the table holds it. O(1).
    pub fn get(&self, key: &IdempotencyKey) -> Option<&KeyEntry> {
        self.by_key.get(key).and_then(|seq| self.by_seq.get(seq))
    }

    /// The entries, by seq (oldest first).
    pub fn entries(&self) -> impl Iterator<Item = &KeyEntry> {
        self.by_seq.values()
    }

    pub fn len(&self) -> usize {
        self.by_seq.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_seq.is_empty()
    }

    /// Remember a keyed commit (its result's seq is above every entry's),
    /// replacing an entry of the same key, and evict the oldest entries
    /// beyond [`KEY_TABLE_CAPACITY`]. O(log n).
    pub(crate) fn insert(&mut self, entry: KeyEntry) {
        let seq = entry.result.seq;
        if let Some(old) = self.by_key.insert(entry.key.clone(), seq) {
            self.by_seq.remove(&old);
        }
        self.by_seq.insert(seq, entry);
        while self.by_seq.len() > KEY_TABLE_CAPACITY {
            if let Some((_, evicted)) = self.by_seq.pop_first() {
                self.by_key.remove(&evicted.key);
            }
        }
    }

    /// The table as checkpoints save it in graph meta (`iwdb.keys`): a JSON
    /// string, entries by seq (`documentation/formats/data-dir.md`).
    pub fn to_meta_value(&self) -> Value {
        let entries = self.by_seq.values().map(SavedEntry::from).collect();
        // Plain structs of strings and numbers: serializing can't fail
        let json = serde_json::to_string(&SavedTable { format: TABLE_FORMAT, entries }).unwrap_or_default();
        Value::String(json)
    }

    /// Read the table saved in a checkpoint at `seq`. Fails with
    /// [`Error::InvalidKeyTable`] if it isn't a table of this format, or
    /// is inconsistent: more than [`KEY_TABLE_CAPACITY`] entries, seqs not
    /// strictly increasing or outside `1 ..= seq`, a key twice.
    pub fn from_meta_value(value: &Value, seq: u64) -> Result<Self, Error> {
        let invalid = |reason: String| Error::InvalidKeyTable { reason };
        let Value::String(json) = value else {
            return Err(invalid(format!("it is not a JSON string: {:?}", value)));
        };
        let saved: SavedTable = serde_json::from_str(json).map_err(|e| invalid(e.to_string()))?;
        if saved.format != TABLE_FORMAT {
            return Err(invalid(format!("format {} (this version reads {})", saved.format, TABLE_FORMAT)));
        }
        if saved.entries.len() > KEY_TABLE_CAPACITY {
            return Err(invalid(format!("{} entries, at most {}", saved.entries.len(), KEY_TABLE_CAPACITY)));
        }
        let mut table = KeyTable::new();
        let mut previous = 0;
        for entry in saved.entries {
            if entry.seq <= previous || entry.seq > seq {
                return Err(invalid(format!(
                    "entry at seq {} after seq {} (entries are by seq, within 1 ..= {})",
                    entry.seq, previous, seq
                )));
            }
            previous = entry.seq;
            if table.by_key.contains_key(&entry.key) {
                return Err(invalid(format!("the key {} appears twice", entry.key)));
            }
            table.insert(entry.into());
        }
        Ok(table)
    }

    /// Where two tables differ, for `verify` and tests: `None` if equal.
    pub fn difference(&self, other: &KeyTable) -> Option<String> {
        if self == other {
            return None;
        }
        if self.len() != other.len() {
            return Some(format!("{} entries differ from {}", self.len(), other.len()));
        }
        let first = self.entries().zip(other.entries()).find(|(a, b)| a != b);
        Some(match first {
            Some((a, b)) => {
                format!("entry {} at seq {} differs from {} at seq {}", a.key, a.result.seq, b.key, b.result.seq)
            }
            None => "the tables differ".into(),
        })
    }
}

/// The version of the saved table's JSON.
const TABLE_FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedTable {
    format: u32,
    entries: Vec<SavedEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedEntry {
    seq: u64,
    key: IdempotencyKey,
    fingerprint: u32,
    /// Microseconds since 1970 (UTC), `null` for none.
    time: Option<i64>,
    edge_ids: Vec<u64>,
    /// `[["n", "<node id>"] | ["e", <edge id>], version]`
    versions: Vec<(SavedTarget, u64)>,
}

#[derive(Serialize, Deserialize)]
enum SavedTarget {
    #[serde(rename = "n")]
    Node(String),
    #[serde(rename = "e")]
    Edge(u64),
}

impl From<&KeyEntry> for SavedEntry {
    fn from(entry: &KeyEntry) -> Self {
        let result = &entry.result;
        SavedEntry {
            seq: result.seq,
            key: entry.key.clone(),
            fingerprint: entry.fingerprint,
            time: result.time.map(CommitTime::micros),
            edge_ids: result.edge_ids.iter().map(|e| e.0).collect(),
            versions: result
                .versions
                .iter()
                .map(|(target, v)| {
                    let target = match target {
                        Target::Node(id) => SavedTarget::Node(id.clone()),
                        Target::Edge(id) => SavedTarget::Edge(id.0),
                    };
                    (target, *v)
                })
                .collect(),
        }
    }
}

impl From<SavedEntry> for KeyEntry {
    fn from(saved: SavedEntry) -> Self {
        let versions = saved
            .versions
            .into_iter()
            .map(|(target, v)| {
                let target = match target {
                    SavedTarget::Node(id) => Target::Node(id),
                    SavedTarget::Edge(id) => Target::Edge(EdgeId(id)),
                };
                (target, v)
            })
            .collect();
        KeyEntry {
            key: saved.key,
            fingerprint: saved.fingerprint,
            result: CommitResult {
                seq: saved.seq,
                edge_ids: saved.edge_ids.into_iter().map(EdgeId).collect(),
                versions,
                time: saved.time.map(CommitTime),
                deduplicated: false,
            },
        }
    }
}

/// The fingerprint of a data transaction: CRC32C of the byte 1 followed by
/// the postcard encoding of the mutations (maps sorted by key). Equal
/// mutations in the same order have equal fingerprints; a different
/// request has another one except with probability about 2^-32. Part of
/// the WAL and checkpoint formats (`documentation/formats/wal.md`).
///
/// Fails with [`Error::Unencodable`] for values nested deeper than the
/// encoding allows (such a transaction fails validation anyway).
pub fn fingerprint_data(mutations: &[Mutation]) -> Result<u32, Error> {
    fingerprint(1, mutations)
}

/// The fingerprint of a catalog change: like [`fingerprint_data`], with the
/// byte 2 and the encoding of the change.
pub fn fingerprint_catalog(change: &CatalogChange) -> Result<u32, Error> {
    fingerprint(2, change)
}

fn fingerprint<T: Serialize + ?Sized>(kind: u8, request: &T) -> Result<u32, Error> {
    let bytes = postcard::to_allocvec(request).map_err(|e| Error::Unencodable { message: e.to_string() })?;
    Ok(crc32c::crc32c_append(crc32c::crc32c(&[kind]), &bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironweaver_core::Attrs;

    fn key(s: &str) -> IdempotencyKey {
        IdempotencyKey::new(s).expect("key")
    }

    fn entry(k: &str, seq: u64) -> KeyEntry {
        KeyEntry {
            key: key(k),
            fingerprint: seq as u32,
            result: CommitResult {
                seq,
                edge_ids: vec![EdgeId(seq)],
                versions: vec![(Target::Node(k.into()), 1), (Target::Edge(EdgeId(seq)), 2)],
                time: (seq % 2 == 0).then_some(CommitTime(seq as i64 * 1000)),
                deduplicated: false,
            },
        }
    }

    #[test]
    fn keys_have_1_to_255_bytes() {
        assert!(IdempotencyKey::new("").is_err());
        assert!(IdempotencyKey::new("x".repeat(MAX_KEY_LEN)).is_ok());
        assert!(IdempotencyKey::new("x".repeat(MAX_KEY_LEN + 1)).is_err());
        assert!(serde_json::from_str::<IdempotencyKey>("\"\"").is_err());
        assert_eq!(serde_json::from_str::<IdempotencyKey>("\"é\"").expect("json"), key("é"));
    }

    #[test]
    fn the_table_keeps_the_newest_entries_and_replaces_a_key() {
        let mut table = KeyTable::new();
        for seq in 1..=(KEY_TABLE_CAPACITY as u64 + 5) {
            table.insert(entry(&format!("k{}", seq), seq));
        }
        assert_eq!(table.len(), KEY_TABLE_CAPACITY);
        assert!(table.get(&key("k5")).is_none());
        assert_eq!(table.get(&key("k6")).map(|e| e.result.seq), Some(6));
        assert_eq!(table.entries().next().map(|e| e.result.seq), Some(6));
        // The same key again: one entry, at the new seq
        table.insert(entry("k6", 20_000));
        assert_eq!(table.len(), KEY_TABLE_CAPACITY);
        assert_eq!(table.get(&key("k6")).map(|e| e.result.seq), Some(20_000));
        assert_eq!(table.entries().next().map(|e| e.result.seq), Some(7));
    }

    #[test]
    fn the_saved_table_round_trips_and_is_checked() {
        let mut table = KeyTable::new();
        for (k, seq) in [("a", 2), ("b", 5), ("c", 9)] {
            table.insert(entry(k, seq));
        }
        let saved = table.to_meta_value();
        assert_eq!(KeyTable::from_meta_value(&saved, 9).expect("load"), table);
        assert_eq!(KeyTable::from_meta_value(&KeyTable::new().to_meta_value(), 0).expect("empty"), KeyTable::new());
        // An entry above the checkpoint's seq
        assert!(KeyTable::from_meta_value(&saved, 8).is_err());
        let Value::String(json) = saved else { panic!("a string") };
        for bad in [
            json.replace("\"seq\":5", "\"seq\":1"),         // not by seq
            json.replace("\"key\":\"b\"", "\"key\":\"a\""), // a key twice
            json.replace("\"key\":\"b\"", "\"key\":\"\""),  // an invalid key
            json.replace("\"format\":1", "\"format\":2"),
            json.replace("\"fingerprint\"", "\"other\""),
            "{}".into(),
        ] {
            assert!(KeyTable::from_meta_value(&Value::String(bad.clone()), 9).is_err(), "{}", bad);
        }
        assert!(KeyTable::from_meta_value(&Value::Int(1), 9).is_err());
    }

    #[test]
    fn fingerprints_depend_on_the_request_only() {
        let upsert = |attr: Attrs| Mutation::UpsertNode {
            id: "a".into(),
            labels: vec![],
            attr,
            meta: Attrs::new(),
            expected_version: None,
        };
        // Insertion order of the attribute map doesn't matter
        let mut one = Attrs::new();
        let mut two = Attrs::new();
        for i in 0..50 {
            one.insert(format!("k{}", i), Value::Int(i));
        }
        for i in (0..50).rev() {
            two.insert(format!("k{}", i), Value::Int(i));
        }
        let a = fingerprint_data(&[upsert(one.clone())]).expect("fingerprint");
        assert_eq!(a, fingerprint_data(&[upsert(two)]).expect("fingerprint"));
        one.insert("k0".into(), Value::Int(1));
        assert_ne!(a, fingerprint_data(&[upsert(one)]).expect("fingerprint"));
        let index = crate::catalog::IndexDef { path: crate::catalog::AttrPath::new(["x"]).expect("path") };
        assert_ne!(
            fingerprint_catalog(&CatalogChange::CreateIndex(index.clone())).expect("fingerprint"),
            fingerprint_catalog(&CatalogChange::DropIndex(index)).expect("fingerprint")
        );
    }
}
