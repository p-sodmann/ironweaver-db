//! The namespace log: which namespaces a store has had, and when they were
//! created and dropped (data-dir layout 4, ADR 0017, ADR 0018;
//! `documentation/formats/data-dir.md`).
//!
//! A store's namespaces are listed in one small append-only file,
//! `NAMESPACES`. Its events are the only truth about which namespaces
//! exist; the directories under `ns/` follow it (a namespace directory is
//! made *before* its create event is logged, and removed *after* its drop
//! event, so recovery only ever has to remove directories that the log
//! doesn't know). Every event is fsynced before it is acknowledged, and
//! carries the idempotency key of the request that made it.
//!
//! ```text
//! header  (16 bytes)   magic `IWDBNSL\n`, version u32, CRC32C of the 12 bytes before
//! frame   (24 + len)   len u32, seq u64, time i64, CRC32C u32, payload (JSON)
//! ```
//!
//! The CRC covers `len`, `seq`, `time` and the payload. Events are numbered
//! 1, 2, ... without gaps. Times are microseconds since 1970, made
//! non-decreasing like the WAL's ([`CommitTime`]).
//!
//! **Torn tail.** Events are fsynced one at a time, so damage can only be
//! at the end: an incomplete last frame, or a complete last frame with a
//! wrong CRC (a partly written page). Recovery cuts it (that event was
//! never acknowledged). Damage followed by more bytes is corruption.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::{CommitTime, IdempotencyKey};
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::io::{LogFile, LogFs};

/// The namespace log's file name.
pub const NAMESPACES_NAME: &str = "NAMESPACES";
/// The directory with a namespace directory for each namespace.
pub const NS_DIR: &str = "ns";
/// The first 8 bytes of the namespace log.
pub const LOG_MAGIC: [u8; 8] = *b"IWDBNSL\n";
/// The log format this version writes and reads.
pub const LOG_VERSION: u32 = 1;
/// Length of the header.
pub const HEADER_LEN: usize = 16;
/// Length of a frame's header.
pub const FRAME_HEADER_LEN: usize = 24;
/// The largest payload of an event.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;
/// The id of the namespace a new store starts with, and the one a layout
/// 1 to 3 store becomes when it is upgraded.
pub const DEFAULT_ID: u64 = 1;
/// The name of that namespace.
pub const DEFAULT_NAME: &str = "default";

/// The directory name of namespace `id`: 20 digits, like seqs.
pub fn ns_dir_name(id: u64) -> String {
    format!("{:020}", id)
}

/// The id of a namespace directory name, if it is one.
pub fn parse_ns_dir_name(name: &str) -> Option<u64> {
    if name.len() != 20 || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok().filter(|id| *id > 0)
}

/// What an event did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Create,
    Drop,
}

impl EventKind {
    fn tag(self) -> u8 {
        match self {
            EventKind::Create => 3,
            EventKind::Drop => 4,
        }
    }
}

/// The request fingerprint of a namespace operation: what a retry under
/// the same key must match (CRC32C of a tag and the name).
pub fn fingerprint(kind: EventKind, name: &NamespaceName) -> u32 {
    let mut bytes = vec![kind.tag()];
    bytes.extend_from_slice(name.as_str().as_bytes());
    crc32c::crc32c(&bytes)
}

/// One event of the namespace log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The event's number, from 1.
    pub seq: u64,
    pub time: CommitTime,
    pub kind: EventKind,
    /// The namespace's id: assigned at creation, never reused.
    pub id: u64,
    pub name: NamespaceName,
    /// The idempotency key of the request, and its fingerprint.
    pub keyed: Option<(IdempotencyKey, u32)>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    op: EventKind,
    id: u64,
    name: NamespaceName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<IdempotencyKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<u32>,
}

impl Event {
    /// The event's frame.
    pub fn encode(&self) -> Vec<u8> {
        let payload = Payload {
            op: self.kind,
            id: self.id,
            name: self.name.clone(),
            key: self.keyed.as_ref().map(|(k, _)| k.clone()),
            fingerprint: self.keyed.as_ref().map(|(_, f)| *f),
        };
        // Structs of strings and numbers always serialize
        let json = serde_json::to_vec(&payload).unwrap_or_default();
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + json.len());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.time.0.to_le_bytes());
        let mut crc = crc32c::crc32c(&out);
        crc = crc32c::crc32c_append(crc, &json);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&json);
        out
    }
}

/// The header of a namespace log.
pub fn header() -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[..8].copy_from_slice(&LOG_MAGIC);
    out[8..12].copy_from_slice(&LOG_VERSION.to_le_bytes());
    let crc = crc32c::crc32c(&out[..12]);
    out[12..].copy_from_slice(&crc.to_le_bytes());
    out
}

/// A whole log: the header and the frames of `events`.
pub fn encode_log(events: &[Event]) -> Vec<u8> {
    let mut out = header().to_vec();
    for event in events {
        out.extend_from_slice(&event.encode());
    }
    out
}

/// What reading a log found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLog {
    pub events: Vec<Event>,
    /// The length of the valid part (header and complete frames).
    pub valid_len: u64,
    /// The file's length.
    pub file_len: u64,
    /// Why the tail after `valid_len` isn't valid, if it isn't: a torn
    /// tail.
    pub torn: Option<String>,
}

/// Parse a namespace log. `Err` is a reason the log is corrupt (damage
/// that isn't a torn tail, a header that isn't ours, a newer version, an
/// event that doesn't follow from the ones before it).
pub fn parse_log(bytes: &[u8]) -> Result<ParsedLog, String> {
    let file_len = bytes.len() as u64;
    if bytes.len() < HEADER_LEN {
        return Err("the header is truncated".into());
    }
    if bytes[..8] != LOG_MAGIC {
        return Err("not a namespace log (wrong magic)".into());
    }
    if crc32c::crc32c(&bytes[..12]).to_le_bytes() != bytes[12..16] {
        return Err("the header's checksum doesn't match".into());
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if version != LOG_VERSION {
        return Err(format!("namespace log version {} (this version reads {})", version, LOG_VERSION));
    }
    let mut events = Vec::new();
    let mut table = NamespaceTable::default();
    let mut at = HEADER_LEN;
    let mut torn = None;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let expected_seq = events.len() as u64 + 1;
        // Damage here is a torn tail unless a valid frame follows it: then
        // the damaged event had been synced, and the log is corrupt
        let damage = |reason: String| -> Result<Option<String>, String> {
            if valid_frame_after(bytes, at + 1, expected_seq + 1) {
                Err(format!("{}, and a valid event follows", reason))
            } else {
                Ok(Some(reason))
            }
        };
        if rest.len() < FRAME_HEADER_LEN {
            torn = damage(format!("an incomplete frame header at offset {}", at))?;
            break;
        }
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let end = FRAME_HEADER_LEN.saturating_add(len);
        if len > MAX_PAYLOAD_LEN || rest.len() < end {
            torn = damage(format!("an incomplete frame at offset {}", at))?;
            break;
        }
        let seq = u64::from_le_bytes(rest[4..12].try_into().unwrap_or([0; 8]));
        let time = i64::from_le_bytes(rest[12..20].try_into().unwrap_or([0; 8]));
        let crc = u32::from_le_bytes(rest[20..24].try_into().unwrap_or([0; 4]));
        let mut expected = crc32c::crc32c(&rest[..20]);
        expected = crc32c::crc32c_append(expected, &rest[FRAME_HEADER_LEN..end]);
        if expected != crc {
            torn = damage(format!("a frame at offset {} with a wrong checksum", at))?;
            break;
        }
        let payload: Payload = serde_json::from_slice(&rest[FRAME_HEADER_LEN..end])
            .map_err(|e| format!("the frame at offset {} holds an invalid event: {}", at, e))?;
        let keyed = match (payload.key, payload.fingerprint) {
            (Some(key), Some(fingerprint)) => Some((key, fingerprint)),
            (None, None) => None,
            _ => return Err(format!("the event at offset {} has a key without a fingerprint or the reverse", at)),
        };
        let event = Event { seq, time: CommitTime(time), kind: payload.op, id: payload.id, name: payload.name, keyed };
        table.push(event.clone()).map_err(|reason| format!("event {} (offset {}): {}", seq, at, reason))?;
        events.push(event);
        at += end;
    }
    Ok(ParsedLog { events, valid_len: at as u64, file_len, torn })
}

/// Whether a frame with a valid CRC and a seq of at least `min_seq` starts
/// at some offset from `from` on.
fn valid_frame_after(bytes: &[u8], from: usize, min_seq: u64) -> bool {
    (from..bytes.len().saturating_sub(FRAME_HEADER_LEN - 1)).any(|at| {
        let rest = &bytes[at..];
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let end = FRAME_HEADER_LEN.saturating_add(len);
        if len > MAX_PAYLOAD_LEN || rest.len() < end {
            return false;
        }
        let seq = u64::from_le_bytes(rest[4..12].try_into().unwrap_or([0; 8]));
        let crc = u32::from_le_bytes(rest[20..24].try_into().unwrap_or([0; 4]));
        let mut expected = crc32c::crc32c(&rest[..20]);
        expected = crc32c::crc32c_append(expected, &rest[FRAME_HEADER_LEN..end]);
        seq >= min_seq && expected == crc
    })
}

/// What a namespace is: an entry of the table of live namespaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceInfo {
    pub id: u64,
    pub name: NamespaceName,
    /// The time of its create event.
    pub created: CommitTime,
    /// The number of its create event.
    pub created_seq: u64,
}

/// What a namespace operation made: the event, and whether it was
/// deduplicated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceResult {
    pub event: Event,
    /// The request was a retry of a request that had been applied: this is
    /// the original event, nothing was logged now.
    pub deduplicated: bool,
}

/// The namespaces the events leave, and the idempotency keys they hold.
#[derive(Clone, Debug, Default)]
pub struct NamespaceTable {
    events: Vec<Event>,
    live: BTreeMap<u64, NamespaceInfo>,
    by_name: BTreeMap<NamespaceName, u64>,
    next_id: u64,
    keys: HashMap<IdempotencyKey, usize>,
}

/// What a create or drop request needs, after the table has been asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// A new event: create this id / drop this one.
    New { id: u64 },
    /// The key's event, with the same request.
    Duplicate(Event),
}

impl NamespaceTable {
    /// The table after `events`, which must follow from each other.
    pub fn from_events(events: Vec<Event>) -> Result<Self, String> {
        let mut table = NamespaceTable::default();
        for event in events {
            let seq = event.seq;
            table.push(event).map_err(|reason| format!("event {}: {}", seq, reason))?;
        }
        Ok(table)
    }

    /// Add the next event; an error says why it doesn't follow.
    pub(crate) fn push(&mut self, event: Event) -> Result<(), String> {
        if event.seq != self.events.len() as u64 + 1 {
            return Err(format!("it has seq {}, expected {}", event.seq, self.events.len() + 1));
        }
        if let Some(last) = self.events.last()
            && event.time < last.time
        {
            return Err(format!("its time {} is before the previous event's {}", event.time, last.time));
        }
        match event.kind {
            EventKind::Create => {
                if event.id == 0 || event.id < self.next_id {
                    return Err(format!("it creates id {}, which is not above every earlier id", event.id));
                }
                if self.by_name.contains_key(&event.name) {
                    return Err(format!("it creates '{}', which exists", event.name));
                }
                self.next_id = event.id.saturating_add(1);
                self.by_name.insert(event.name.clone(), event.id);
                self.live.insert(
                    event.id,
                    NamespaceInfo {
                        id: event.id,
                        name: event.name.clone(),
                        created: event.time,
                        created_seq: event.seq,
                    },
                );
            }
            EventKind::Drop => match self.live.get(&event.id) {
                Some(info) if info.name == event.name => {
                    self.live.remove(&event.id);
                    self.by_name.remove(&event.name);
                }
                _ => return Err(format!("it drops namespace {} ('{}'), which doesn't exist", event.id, event.name)),
            },
        }
        if let Some((key, _)) = &event.keyed
            && self.keys.insert(key.clone(), self.events.len()).is_some()
        {
            return Err(format!("it reuses the idempotency key {}", key));
        }
        self.events.push(event);
        Ok(())
    }

    /// Every event, in order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The live namespaces, by id.
    pub fn live(&self) -> impl Iterator<Item = &NamespaceInfo> {
        self.live.values()
    }

    pub fn get(&self, name: &NamespaceName) -> Option<&NamespaceInfo> {
        self.by_name.get(name).and_then(|id| self.live.get(id))
    }

    pub fn get_id(&self, id: u64) -> Option<&NamespaceInfo> {
        self.live.get(&id)
    }

    /// The id the next created namespace gets.
    pub fn next_id(&self) -> u64 {
        self.next_id.max(1)
    }

    /// The event the key made, if the table holds it.
    pub fn by_key(&self, key: &IdempotencyKey) -> Option<&Event> {
        self.keys.get(key).map(|i| &self.events[*i])
    }

    /// The namespaces alive after the events with `time` at or before
    /// `at` (restore to a time). Events are in time order.
    pub fn live_at(&self, at: CommitTime) -> Vec<NamespaceInfo> {
        let upto = self.events.partition_point(|e| e.time <= at);
        // Events are valid by construction, so replaying a prefix can't fail
        let prefix = NamespaceTable::from_events(self.events[..upto].to_vec()).unwrap_or_default();
        prefix.live.into_values().collect()
    }

    /// Ask what creating (or dropping) `name` under `key` needs: the
    /// original event if the key was used for the same request
    /// ([`Plan::Duplicate`]); an error if the key was used for another
    /// request, or the request can't be done now.
    pub fn plan(&self, kind: EventKind, name: &NamespaceName, key: Option<&IdempotencyKey>) -> Result<Plan, Error> {
        if let Some(key) = key
            && let Some(event) = self.by_key(key)
        {
            let (_, found) = event.keyed.clone().unwrap_or((key.clone(), 0));
            if found == fingerprint(kind, name) {
                return Ok(Plan::Duplicate(event.clone()));
            }
            return Err(iwdb_engine::Error::IdempotencyKeyReused { key: key.clone(), seq: event.seq }.into());
        }
        match (kind, self.get(name)) {
            (EventKind::Create, Some(_)) => Err(Error::NamespaceExists { name: name.to_string() }),
            (EventKind::Create, None) => Ok(Plan::New { id: self.next_id() }),
            (EventKind::Drop, Some(info)) => Ok(Plan::New { id: info.id }),
            (EventKind::Drop, None) => Err(Error::NoSuchNamespace { name: name.to_string() }),
        }
    }
}

/// The torn tail cut off the namespace log by recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutLog {
    pub file_len: u64,
    pub valid_len: u64,
    pub reason: String,
}

/// The open namespace log: its events and the file they are appended to.
///
/// **Failure.** If appending or fsyncing an event fails, the log is
/// *failed*: its outcome is unknown (the event may be on disk), so no
/// further event is logged until the store is reopened, which reads the
/// log and tells.
pub struct NamespaceLog<F: LogFs> {
    fs: F,
    path: PathBuf,
    file: F::File,
    table: NamespaceTable,
    failed: Option<String>,
}

impl<F: LogFs> std::fmt::Debug for NamespaceLog<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamespaceLog")
            .field("path", &self.path)
            .field("events", &self.table.events().len())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl<F: LogFs> NamespaceLog<F> {
    /// Create the log of a new store in `root`: the header and `events`,
    /// written with `write_atomic`, and the directory synced. Returns it
    /// open.
    pub fn create(fs: F, root: &Path, events: Vec<Event>) -> Result<Self, Error> {
        let path = root.join(NAMESPACES_NAME);
        let table = NamespaceTable::from_events(events)
            .map_err(|reason| Error::InvalidNamespaceLog { path: path.clone(), reason })?;
        write_whole(&fs, &path, table.events())?;
        fs.sync_dir(root).map_err(|e| Error::io("sync directory", root, e))?;
        let file = fs.open_append(&path).map_err(|e| Error::io("open", &path, e))?;
        Ok(NamespaceLog { fs, path, file, table, failed: None })
    }

    /// Open the log in `root`, cutting a torn tail (truncated, fsynced).
    /// Errors: [`Error::InvalidNamespaceLog`] (missing, corrupt, newer),
    /// [`Error::Io`].
    pub fn open(fs: F, root: &Path) -> Result<(Self, Option<CutLog>), Error> {
        let path = root.join(NAMESPACES_NAME);
        let (parsed, table) = read_log(&path)?;
        let mut cut = None;
        if let Some(reason) = parsed.torn {
            fs.truncate(&path, parsed.valid_len).map_err(|e| Error::io("truncate", &path, e))?;
            cut = Some(CutLog { file_len: parsed.file_len, valid_len: parsed.valid_len, reason });
        }
        let file = fs.open_append(&path).map_err(|e| Error::io("open", &path, e))?;
        Ok((NamespaceLog { fs, path, file, table, failed: None }, cut))
    }

    pub fn table(&self) -> &NamespaceTable {
        &self.table
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Why the log accepts no more events, if it doesn't.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Fail the log (see the type docs) because a step that goes with an
    /// event failed after the event was made durable.
    pub fn fail(&mut self, cause: String) {
        self.failed.get_or_insert(cause);
    }

    /// The file operations, for the caller's own steps.
    pub fn fs(&self) -> &F {
        &self.fs
    }

    /// Append an event and fsync it: the event is durable when this
    /// returns. The caller has asked [`NamespaceTable::plan`] first.
    ///
    /// On an I/O error the log is failed (see the type docs): the event may
    /// or may not be on disk.
    pub fn append(
        &mut self,
        kind: EventKind,
        id: u64,
        name: &NamespaceName,
        key: Option<&IdempotencyKey>,
    ) -> Result<Event, Error> {
        if let Some(cause) = &self.failed {
            return Err(Error::ReadOnly { cause: cause.clone() });
        }
        let last = self.table.events().last().map_or(CommitTime(i64::MIN), |e| e.time);
        let event = Event {
            seq: self.table.events().len() as u64 + 1,
            time: CommitTime::now().max(last),
            kind,
            id,
            name: name.clone(),
            keyed: key.map(|k| (k.clone(), fingerprint(kind, name))),
        };
        let mut checked = self.table.clone();
        checked.push(event.clone()).map_err(|reason| Error::InvalidNamespaceLog { path: self.path.clone(), reason })?;
        let frame = event.encode();
        let written = self
            .file
            .write_all(&frame)
            .map_err(|e| Error::io("write", &self.path, e))
            .and_then(|()| self.file.sync().map_err(|e| Error::io("fsync", &self.path, e)));
        if let Err(e) = written {
            self.failed = Some(e.to_string());
            return Err(e);
        }
        self.table = checked;
        Ok(event)
    }
}

/// Write a whole namespace log atomically.
pub(crate) fn write_whole<F: LogFs>(fs: &F, path: &Path, events: &[Event]) -> Result<(), Error> {
    let bytes = encode_log(events);
    fs.write_atomic(path, &mut |out| out.write_all(&bytes)).map_err(|e| Error::io("write", path, e))
}

/// Read and parse the log at `path`, with its table. A torn tail is
/// reported, not an error.
pub fn read_log(path: &Path) -> Result<(ParsedLog, NamespaceTable), Error> {
    let invalid = |reason: String| Error::InvalidNamespaceLog { path: path.to_path_buf(), reason };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(invalid("the file is missing".into())),
        Err(e) => return Err(Error::io("read", path, e)),
    };
    let parsed = parse_log(&bytes).map_err(invalid)?;
    let table = NamespaceTable::from_events(parsed.events.clone()).map_err(invalid)?;
    Ok((parsed, table))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn name(s: &str) -> NamespaceName {
        NamespaceName::new(s).expect("name")
    }

    fn key(s: &str) -> IdempotencyKey {
        IdempotencyKey::new(s).expect("key")
    }

    fn event(seq: u64, kind: EventKind, id: u64, n: &str) -> Event {
        Event { seq, time: CommitTime(seq as i64 * 10), kind, id, name: name(n), keyed: None }
    }

    #[test]
    fn logs_round_trip_and_the_table_follows() {
        let mut events = vec![event(1, EventKind::Create, 1, "default"), event(2, EventKind::Create, 2, "b")];
        events.push(Event { keyed: Some((key("k"), 7)), ..event(3, EventKind::Drop, 2, "b") });
        events.push(event(4, EventKind::Create, 3, "b"));
        let bytes = encode_log(&events);
        let parsed = parse_log(&bytes).expect("parse");
        assert_eq!(parsed.events, events);
        assert_eq!((parsed.valid_len, parsed.torn), (bytes.len() as u64, None));
        let table = NamespaceTable::from_events(events).expect("table");
        let live: Vec<_> = table.live().map(|n| (n.id, n.name.to_string())).collect();
        assert_eq!(live, [(1, "default".to_owned()), (3, "b".to_owned())]);
        assert_eq!(table.next_id(), 4);
        assert_eq!(table.by_key(&key("k")).map(|e| e.seq), Some(3));
        // As of time 25: ids 1 and 2 (2 is dropped at 30)
        let at: Vec<_> = table.live_at(CommitTime(25)).iter().map(|n| n.id).collect();
        assert_eq!(at, [1, 2]);
        assert!(table.live_at(CommitTime(5)).is_empty());
    }

    #[test]
    fn events_must_follow_from_each_other() {
        let bad = |events: Vec<Event>| NamespaceTable::from_events(events).expect_err("invalid");
        assert!(bad(vec![event(2, EventKind::Create, 1, "a")]).contains("expected 1"));
        assert!(bad(vec![event(1, EventKind::Drop, 1, "a")]).contains("doesn't exist"));
        assert!(bad(vec![event(1, EventKind::Create, 1, "a"), event(2, EventKind::Create, 2, "a")]).contains("exists"));
        assert!(
            bad(vec![
                event(1, EventKind::Create, 2, "a"),
                event(2, EventKind::Drop, 2, "a"),
                event(3, EventKind::Create, 2, "a")
            ])
            .contains("not above")
        );
        assert!(
            bad(vec![event(1, EventKind::Create, 1, "a"), event(2, EventKind::Drop, 1, "b")]).contains("doesn't exist")
        );
        let mut early = event(2, EventKind::Create, 2, "b");
        early.time = CommitTime(1);
        assert!(bad(vec![event(1, EventKind::Create, 1, "a"), early]).contains("before"));
    }

    #[test]
    fn plans_follow_the_keys() {
        let k = key("k");
        let mut keyed = event(1, EventKind::Create, 1, "a");
        keyed.keyed = Some((k.clone(), fingerprint(EventKind::Create, &name("a"))));
        let table = NamespaceTable::from_events(vec![keyed.clone()]).expect("table");
        assert_eq!(table.plan(EventKind::Create, &name("a"), Some(&k)).expect("plan"), Plan::Duplicate(keyed));
        assert_matches!(table.plan(EventKind::Create, &name("b"), Some(&k)), Err(Error::Engine(_)));
        assert_matches!(table.plan(EventKind::Drop, &name("a"), Some(&k)), Err(Error::Engine(_)));
        assert_matches!(table.plan(EventKind::Create, &name("a"), None), Err(Error::NamespaceExists { .. }));
        assert_eq!(table.plan(EventKind::Create, &name("b"), None).expect("plan"), Plan::New { id: 2 });
        assert_eq!(table.plan(EventKind::Drop, &name("a"), None).expect("plan"), Plan::New { id: 1 });
        assert_matches!(table.plan(EventKind::Drop, &name("b"), None), Err(Error::NoSuchNamespace { .. }));
    }

    #[test]
    fn damage_is_a_torn_tail_only_at_the_end() {
        let events = vec![event(1, EventKind::Create, 1, "a"), event(2, EventKind::Create, 2, "b")];
        let bytes = encode_log(&events);
        let first_end = HEADER_LEN + events[0].encode().len();
        // Every truncation inside the last frame is a torn tail
        for cut in first_end..bytes.len() {
            let parsed = parse_log(&bytes[..cut]).expect("torn");
            assert_eq!(parsed.events.len(), 1, "cut {}", cut);
            assert_eq!(parsed.valid_len as usize, first_end);
            assert_eq!(parsed.torn.is_some(), cut > first_end);
        }
        // A flipped bit in the last frame: torn; in the first: corruption
        for at in first_end..bytes.len() {
            let mut bad = bytes.clone();
            bad[at] ^= 1;
            match parse_log(&bad) {
                Ok(parsed) => assert!(parsed.torn.is_some() && parsed.events.len() == 1, "byte {}", at),
                Err(reason) => panic!("byte {}: {}", at, reason),
            }
        }
        for at in 0..first_end {
            let mut bad = bytes.clone();
            bad[at] ^= 1;
            assert!(parse_log(&bad).is_err(), "byte {}", at);
        }
        assert!(parse_log(&bytes[..HEADER_LEN - 1]).is_err());
        assert!(parse_log(b"not a log at all, but long enough").is_err());
    }

    #[test]
    fn dir_names_round_trip() {
        assert_eq!(ns_dir_name(7), "00000000000000000007");
        assert_eq!(parse_ns_dir_name(&ns_dir_name(u64::MAX)), Some(u64::MAX));
        for bad in ["7", "0000000000000000000x", "00000000000000000000", "00000000000000000007.tmp"] {
            assert_eq!(parse_ns_dir_name(bad), None, "{}", bad);
        }
    }
}
