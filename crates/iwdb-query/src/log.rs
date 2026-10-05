//! The log tail (step 16c, ADR 0051): a ring of the last events the
//! server logged, for `GetLog` and the console. The server's subscriber
//! (step 16b, ADR 0042) puts in exactly what it writes to stderr, after
//! the same level filter; nothing else writes to it. So it holds no more
//! than the logs do, and the logs hold no secret (ADR 0044, ADR 0049).
//!
//! Each event is numbered ([`LogEvent::seq`], from 1), so a reader asks
//! for the events after the last one it saw. An event's text (message and
//! fields) is cut at [`MAX_EVENT_BYTES`].

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use iwdb_engine::CommitTime;

/// The default number of events the ring keeps (`[log] tail_events`).
pub const DEFAULT_EVENTS: usize = 1000;
/// The most events a ring may keep.
pub const MAX_EVENTS: usize = 100_000;
/// The most events one read returns.
pub const MAX_READ: usize = 1000;
/// An event's message and field values together are cut at about this many
/// bytes, with [`CUT`] at the end of what was cut.
pub const MAX_EVENT_BYTES: usize = 2048;
/// What ends a cut text.
pub const CUT: &str = "…";

/// An event's level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const ALL: [Level; 5] = [Level::Trace, Level::Debug, Level::Info, Level::Warn, Level::Error];

    /// `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` (as the JSON logs write it).
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    pub fn parse(name: &str) -> Option<Level> {
        Level::ALL.into_iter().find(|l| l.as_str() == name)
    }
}

/// One logged event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    /// Its number in the ring, from 1.
    pub seq: u64,
    pub time: CommitTime,
    pub level: Level,
    /// Where it was logged: a module path, or `iwdb::audit`.
    pub target: String,
    pub message: String,
    /// The event's fields (not its message), in the order logged.
    pub fields: Vec<(String, String)>,
}

/// A read of the ring ([`LogRing::read`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogTail {
    /// The events after the one asked for, oldest first.
    pub events: Vec<LogEvent>,
    /// The seq of the last event logged so far (0: none): ask for the events
    /// after it next.
    pub last_seq: u64,
    /// Whether events after the one asked for fell out of the ring before
    /// they were read (or a limit cut the read: then ask again).
    pub missed: bool,
}

/// The ring of recent events: the last `capacity` (0: none kept).
#[derive(Debug)]
pub struct LogRing {
    capacity: usize,
    state: Mutex<Ring>,
}

#[derive(Debug, Default)]
struct Ring {
    events: VecDeque<LogEvent>,
    last_seq: u64,
}

impl Default for LogRing {
    fn default() -> Self {
        LogRing::new(DEFAULT_EVENTS)
    }
}

impl LogRing {
    /// A ring keeping the last `capacity` events (at most [`MAX_EVENTS`]).
    pub fn new(capacity: usize) -> Self {
        LogRing { capacity: capacity.min(MAX_EVENTS), state: Mutex::new(Ring::default()) }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn state(&self) -> MutexGuard<'_, Ring> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keep an event (its `seq` is set here), cutting its text at
    /// [`MAX_EVENT_BYTES`]. O(1).
    pub fn push(&self, time: CommitTime, level: Level, target: &str, message: &str, fields: Vec<(String, String)>) {
        let mut budget = MAX_EVENT_BYTES;
        let message = cut(message, &mut budget);
        let fields = fields.into_iter().map(|(name, value)| (name, cut(&value, &mut budget))).collect();
        let mut state = self.state();
        state.last_seq += 1;
        if self.capacity == 0 {
            return;
        }
        if state.events.len() == self.capacity {
            state.events.pop_front();
        }
        let seq = state.last_seq;
        state.events.push_back(LogEvent { seq, time, level, target: target.to_owned(), message, fields });
    }

    /// The events after `after`, oldest first, at most `limit` (and
    /// [`MAX_READ`]). O(limit).
    pub fn read(&self, after: u64, limit: usize) -> LogTail {
        let state = self.state();
        let first = state.events.front().map_or(state.last_seq + 1, |e| e.seq);
        let skip = usize::try_from(after.saturating_add(1).saturating_sub(first)).unwrap_or(usize::MAX);
        let limit = limit.min(MAX_READ);
        let events: Vec<LogEvent> = state.events.iter().skip(skip).take(limit).cloned().collect();
        let shown = events.last().map_or(after.max(first.saturating_sub(1)), |e| e.seq);
        LogTail { events, last_seq: state.last_seq, missed: after.saturating_add(1) < first || shown < state.last_seq }
    }
}

/// `text`, cut to what is left of `budget` (which it uses up).
fn cut(text: &str, budget: &mut usize) -> String {
    if text.len() <= *budget {
        *budget -= text.len();
        return text.to_owned();
    }
    let mut end = *budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *budget = 0;
    format!("{}{}", &text[..end], CUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(ring: &LogRing, message: &str) {
        ring.push(CommitTime(0), Level::Info, "t", message, vec![("k".into(), "v".into())]);
    }

    #[test]
    fn the_ring_keeps_the_last_events() {
        let ring = LogRing::new(3);
        assert_eq!(ring.read(0, 10), LogTail::default());
        for m in ["a", "b", "c", "d"] {
            push(&ring, m);
        }
        let all = ring.read(0, 10);
        assert_eq!(all.events.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(), ["b", "c", "d"]);
        assert_eq!(all.events.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3, 4]);
        assert_eq!((all.last_seq, all.missed), (4, true), "event 1 fell out");
        let after = ring.read(2, 10);
        assert_eq!(after.events.len(), 2);
        assert!(!after.missed);
        let limited = ring.read(1, 1);
        assert_eq!((limited.events[0].seq, limited.missed), (2, true));
        assert_eq!(ring.read(4, 10), LogTail { events: vec![], last_seq: 4, missed: false });
        let off = LogRing::new(0);
        push(&off, "x");
        assert_eq!(off.read(0, 10), LogTail { events: vec![], last_seq: 1, missed: true });
    }

    #[test]
    fn long_events_are_cut() {
        let ring = LogRing::new(1);
        let long = "é".repeat(MAX_EVENT_BYTES);
        ring.push(CommitTime(0), Level::Warn, "t", "m", vec![("a".into(), long.clone()), ("b".into(), long)]);
        let e = &ring.read(0, 1).events[0];
        assert!(e.fields[0].1.ends_with(CUT) && e.fields[0].1.len() <= MAX_EVENT_BYTES + CUT.len());
        assert_eq!(e.fields[1].1, CUT, "nothing left for the second field");
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
    }
}
