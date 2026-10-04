//! Marks (ADR 0032): a namespace's named high-water marks, each the
//! position in an external log up to which its events are committed.
//!
//! - [`MarkName`]: 1 to [`MAX_MARK_NAME_LEN`] bytes of UTF-8 (a
//!   projection's name).
//! - [`MarkUpdate`]: what a commit asks for, compare-and-set: the mark must
//!   be at `expected` (`None`: not set yet), and moves forward to
//!   `position`. Otherwise the commit fails and changes nothing.
//! - [`Mark`]: what the commit's record carries (WAL format 4), the name
//!   and the new position: replaying the record sets the mark again.
//! - [`MarkTable`]: the namespace's marks, part of its state like the key
//!   table: changed only by applying records, saved in checkpoints
//!   (`iwdb.marks`, data-dir layout 5).
//!
//! Because a mark moves in the same commit as the effect of the events up
//! to it, a projector that resumes from the committed mark applies every
//! event at most once, whatever crashes.

use std::collections::BTreeMap;
use std::fmt;

use ironweaver_core::Value;
use serde::{Deserialize, Serialize};

use crate::Error;

/// The longest mark name, in bytes.
pub const MAX_MARK_NAME_LEN: usize = 255;

/// How many marks a namespace holds at most. Marks are never removed, so
/// a commit that would add one more fails with [`Error::TooManyMarks`].
pub const MAX_MARKS: usize = 1024;

/// The largest position (a checkpoint saves positions as JSON numbers,
/// and Postgres ids are `bigint`).
pub const MAX_POSITION: u64 = i64::MAX as u64;

/// A mark's name: 1 to [`MAX_MARK_NAME_LEN`] bytes of UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MarkName(String);

impl MarkName {
    /// A name, or [`Error::InvalidMark`] if it is empty or longer than
    /// [`MAX_MARK_NAME_LEN`] bytes.
    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        if name.is_empty() || name.len() > MAX_MARK_NAME_LEN {
            return Err(Error::InvalidMark {
                reason: format!("a mark name has 1 to {} bytes, this one {}", MAX_MARK_NAME_LEN, name.len()),
            });
        }
        Ok(MarkName(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for MarkName {
    type Error = Error;
    fn try_from(name: String) -> Result<Self, Error> {
        MarkName::new(name)
    }
}

impl From<MarkName> for String {
    fn from(name: MarkName) -> String {
        name.0
    }
}

impl fmt::Display for MarkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// A commit's request to move a mark: from `expected` (`None`: the mark
/// isn't set yet) to `position`, which must be above it and at most
/// [`MAX_POSITION`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkUpdate {
    pub name: MarkName,
    pub expected: Option<u64>,
    pub position: u64,
}

/// What the record of a commit with a mark carries besides its change
/// (WAL format 4): the mark and its new position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub name: MarkName,
    pub position: u64,
}

/// A mark's state: its position, and the seq of the commit that set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkEntry {
    pub position: u64,
    pub seq: u64,
}

/// The marks of a namespace, by name. Deterministic: the same records give
/// the same table, whichever path applied them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkTable {
    marks: BTreeMap<MarkName, MarkEntry>,
}

impl MarkTable {
    pub fn new() -> Self {
        MarkTable::default()
    }

    /// The mark called `name`, if it is set. O(log n).
    pub fn get(&self, name: &MarkName) -> Option<MarkEntry> {
        self.marks.get(name).copied()
    }

    /// The marks, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&MarkName, &MarkEntry)> {
        self.marks.iter()
    }

    pub fn len(&self) -> usize {
        self.marks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// Check that `update` can be committed now: the mark is at
    /// `expected` ([`Error::MarkConflict`] otherwise), the position moves
    /// forward and is at most [`MAX_POSITION`] ([`Error::InvalidMark`]),
    /// and a new mark fits ([`Error::TooManyMarks`]).
    pub fn check(&self, update: &MarkUpdate) -> Result<(), Error> {
        let found = self.get(&update.name).map(|e| e.position);
        if found != update.expected {
            return Err(Error::MarkConflict { name: update.name.clone(), expected: update.expected, found });
        }
        if update.expected.is_some_and(|e| update.position <= e) || update.position > MAX_POSITION {
            return Err(Error::InvalidMark {
                reason: format!(
                    "mark {} can move from {:?} to a position above it and at most {}, not to {}",
                    update.name, update.expected, MAX_POSITION, update.position
                ),
            });
        }
        if found.is_none() && self.marks.len() >= MAX_MARKS {
            return Err(Error::TooManyMarks { name: update.name.clone() });
        }
        Ok(())
    }

    /// Set a mark, as applying the record of commit `seq` does. Replay
    /// doesn't check again. O(log n).
    pub(crate) fn set(&mut self, mark: Mark, seq: u64) {
        self.marks.insert(mark.name, MarkEntry { position: mark.position, seq });
    }

    /// The table as checkpoints save it in graph meta (`iwdb.marks`): a
    /// JSON string, marks by name (`documentation/formats/data-dir.md`).
    pub fn to_meta_value(&self) -> Value {
        let marks =
            self.marks.iter().map(|(name, e)| SavedMark { name: name.clone(), position: e.position, seq: e.seq });
        let saved = SavedTable { format: TABLE_FORMAT, marks: marks.collect() };
        // Plain structs of strings and numbers: serializing can't fail
        Value::String(serde_json::to_string(&saved).unwrap_or_default())
    }

    /// Read the table saved in a checkpoint at `seq`. Fails with
    /// [`Error::InvalidMarkTable`] if it isn't a table of this format, or
    /// is inconsistent: more than [`MAX_MARKS`] marks, names not strictly
    /// increasing, a seq outside `1 ..= seq` or a position above
    /// [`MAX_POSITION`].
    pub fn from_meta_value(value: &Value, seq: u64) -> Result<Self, Error> {
        let invalid = |reason: String| Error::InvalidMarkTable { reason };
        let Value::String(json) = value else {
            return Err(invalid(format!("it is not a JSON string: {:?}", value)));
        };
        let saved: SavedTable = serde_json::from_str(json).map_err(|e| invalid(e.to_string()))?;
        if saved.format != TABLE_FORMAT {
            return Err(invalid(format!("format {} (this version reads {})", saved.format, TABLE_FORMAT)));
        }
        if saved.marks.len() > MAX_MARKS {
            return Err(invalid(format!("{} marks, at most {}", saved.marks.len(), MAX_MARKS)));
        }
        let mut table = MarkTable::new();
        for mark in saved.marks {
            if table.marks.last_key_value().is_some_and(|(last, _)| *last >= mark.name) {
                return Err(invalid(format!("mark {} is out of order (marks are by name, once each)", mark.name)));
            }
            if mark.seq == 0 || mark.seq > seq || mark.position > MAX_POSITION {
                return Err(invalid(format!(
                    "mark {} has position {} set at seq {} (seqs in 1 ..= {}, positions at most {})",
                    mark.name, mark.position, mark.seq, seq, MAX_POSITION
                )));
            }
            table.marks.insert(mark.name, MarkEntry { position: mark.position, seq: mark.seq });
        }
        Ok(table)
    }

    /// Where two tables differ, for `verify` and tests: `None` if equal.
    pub fn difference(&self, other: &MarkTable) -> Option<String> {
        if self == other {
            return None;
        }
        let names = self.marks.keys().chain(other.marks.keys());
        let name = names.into_iter().find(|n| self.marks.get(*n) != other.marks.get(*n));
        Some(match name {
            Some(name) => format!("mark {} is {:?}, not {:?}", name, self.marks.get(name), other.marks.get(name)),
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
    marks: Vec<SavedMark>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedMark {
    name: MarkName,
    position: u64,
    seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> MarkName {
        MarkName::new(s).expect("name")
    }

    fn update(n: &str, expected: Option<u64>, position: u64) -> MarkUpdate {
        MarkUpdate { name: name(n), expected, position }
    }

    #[test]
    fn names_have_one_to_255_bytes() {
        assert!(MarkName::new("").is_err());
        assert!(MarkName::new("x".repeat(255)).is_ok());
        assert!(MarkName::new("x".repeat(256)).is_err());
    }

    #[test]
    fn updates_are_compare_and_set_and_move_forward() {
        let mut table = MarkTable::new();
        table.check(&update("a", None, 1)).expect("new");
        let conflict = table.check(&update("a", Some(3), 4)).unwrap_err();
        assert_eq!(conflict, Error::MarkConflict { name: name("a"), expected: Some(3), found: None });
        table.set(Mark { name: name("a"), position: 3 }, 7);
        table.check(&update("a", Some(3), 4)).expect("forward");
        assert!(matches!(table.check(&update("a", None, 4)), Err(Error::MarkConflict { found: Some(3), .. })));
        assert!(matches!(table.check(&update("a", Some(3), 3)), Err(Error::InvalidMark { .. })));
        assert!(matches!(table.check(&update("b", None, MAX_POSITION + 1)), Err(Error::InvalidMark { .. })));
        assert_eq!(table.get(&name("a")), Some(MarkEntry { position: 3, seq: 7 }));
    }

    #[test]
    fn a_table_holds_at_most_max_marks() {
        let mut table = MarkTable::new();
        for i in 0..MAX_MARKS {
            table.set(Mark { name: name(&format!("m{:05}", i)), position: 1 }, 1);
        }
        assert!(matches!(table.check(&update("new", None, 1)), Err(Error::TooManyMarks { .. })));
        table.check(&update("m00000", Some(1), 2)).expect("an existing mark still moves");
    }

    #[test]
    fn the_saved_table_round_trips_and_is_checked() {
        let mut table = MarkTable::new();
        table.set(Mark { name: name("orders"), position: 42 }, 5);
        table.set(Mark { name: name("customers"), position: 7 }, 9);
        let saved = table.to_meta_value();
        assert_eq!(MarkTable::from_meta_value(&saved, 9), Ok(table.clone()));
        assert!(matches!(MarkTable::from_meta_value(&saved, 8), Err(Error::InvalidMarkTable { .. })));
        for bad in [
            r#"{"format":2,"marks":[]}"#,
            r#"{"format":1,"marks":[{"name":"b","position":1,"seq":1},{"name":"a","position":1,"seq":1}]}"#,
            r#"{"format":1,"marks":[{"name":"a","position":1,"seq":1},{"name":"a","position":2,"seq":1}]}"#,
            r#"{"format":1,"marks":[{"name":"a","position":1,"seq":0}]}"#,
            r#"{"format":1,"marks":[{"name":"","position":1,"seq":1}]}"#,
            r#"{"format":1,"marks":[{"name":"a","position":9223372036854775808,"seq":1}]}"#,
            r#"{"format":1,"marks":[],"more":1}"#,
        ] {
            let value = Value::String(bad.into());
            assert!(matches!(MarkTable::from_meta_value(&value, 9), Err(Error::InvalidMarkTable { .. })), "{}", bad);
        }
        assert!(table.difference(&MarkTable::new()).is_some());
        assert_eq!(table.difference(&table.clone()), None);
    }
}
