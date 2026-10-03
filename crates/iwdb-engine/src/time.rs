//! Commit times: when a commit was appended to the WAL (format 2,
//! `documentation/formats/wal.md`; ADR 0010). The WAL writer sets them;
//! the engine only carries them in [`CommitResult`](crate::CommitResult)
//! and the idempotency key table.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A commit time: microseconds since 1970-01-01T00:00:00 UTC.
///
/// The WAL writer takes it from the system clock of the process that
/// appends the record, and makes it non-decreasing in seq order: a record
/// gets `max(now, time of the previous record)`, so a clock that goes
/// backwards makes time stand still until it catches up. Records of WAL
/// format 1 have no time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommitTime(pub i64);

impl CommitTime {
    /// The system clock now.
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// A system time, saturating at the ends of the `i64` range (about
    /// 292,000 years either side of 1970).
    pub fn from_system_time(time: SystemTime) -> Self {
        let micros = |d: Duration| i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
        CommitTime(match time.duration_since(UNIX_EPOCH) {
            Ok(after) => micros(after),
            Err(before) => micros(before.duration()).saturating_neg(),
        })
    }

    pub fn to_system_time(self) -> SystemTime {
        let magnitude = Duration::from_micros(self.0.unsigned_abs());
        if self.0 >= 0 {
            UNIX_EPOCH + magnitude
        } else {
            UNIX_EPOCH - magnitude
        }
    }

    /// Microseconds since 1970-01-01 UTC.
    pub fn micros(self) -> i64 {
        self.0
    }
}

impl fmt::Display for CommitTime {
    /// RFC 3339 in UTC, with microseconds if there are any:
    /// `2026-10-01T12:30:00.250000+00:00`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ironweaver_core::DateTime { micros: self.0, offset: Some(0) }.fmt(f)
    }
}

impl FromStr for CommitTime {
    type Err = String;

    /// RFC 3339 / ISO 8601 with a UTC offset (`Z` or `±HH:MM`), as the
    /// core parses date-times: `2026-10-01T12:30:00Z`,
    /// `2026-10-01T14:30:00.5+02:00`. A time without an offset is refused:
    /// it names no instant.
    fn from_str(s: &str) -> Result<Self, String> {
        let time = ironweaver_core::DateTime::from_str(s).map_err(|e| e.to_string())?;
        if time.offset.is_none() {
            return Err(format!("'{}' has no UTC offset; add 'Z' or '+HH:MM'", s));
        }
        Ok(CommitTime(time.micros))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_and_prints() {
        let t: CommitTime = "2026-10-01T14:30:00.25+02:00".parse().expect("parse");
        assert_eq!(t.to_string(), "2026-10-01T12:30:00.250000+00:00");
        assert_eq!("2026-10-01T12:30:00.25Z".parse::<CommitTime>(), Ok(t));
        assert!("2026-10-01T12:30:00".parse::<CommitTime>().is_err());
        assert!("yesterday".parse::<CommitTime>().is_err());
        for micros in [0, 1, -1, 1_759_321_800_250_000, -86_400_000_001] {
            let t = CommitTime(micros);
            assert_eq!(CommitTime::from_system_time(t.to_system_time()), t);
            assert_eq!(t.to_string().parse::<CommitTime>(), Ok(t));
        }
        assert!(CommitTime::now() > CommitTime(1_700_000_000_000_000));
    }
}
