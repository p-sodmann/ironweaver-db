//! What a child does: the fsync policy and store options it runs with, and
//! its script of commits, checkpoints, syncs and pauses, generated from a
//! seed. The parent generates the same script to know which commits the
//! child attempted, in which order.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use iwdb::{CheckpointOptions, FsyncPolicy, StoreOptions, WalOptions};
use iwdb_engine::testutil::workload::{Step, Stream};
use iwdb_storage::MIN_SEGMENT_SIZE;

use crate::rng::Rng;

/// The fsync policy under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Policy {
    Always,
    Group,
    Off,
}

impl Policy {
    pub const ALL: [Policy; 3] = [Policy::Always, Policy::Group, Policy::Off];

    /// `group` syncs every 8 records or 5 ms, so kills land both before
    /// and after group fsyncs.
    pub fn fsync(self) -> FsyncPolicy {
        match self {
            Policy::Always => FsyncPolicy::Always,
            Policy::Group => FsyncPolicy::Group { max_delay: Duration::from_millis(5), max_batch: 8 },
            Policy::Off => FsyncPolicy::Off,
        }
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Policy::Always => "always",
            Policy::Group => "group",
            Policy::Off => "off",
        })
    }
}

impl FromStr for Policy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Policy::ALL.into_iter().find(|p| p.to_string() == s).ok_or_else(|| format!("unknown policy '{}'", s))
    }
}

/// The child's store: 1 KiB segments (a rotation every few commits), a
/// background checkpoint every 8 KiB of WAL or 20 ms, keeping `keep`
/// checkpoints, so that kills land in rotations, checkpoints and WAL
/// segment removal.
pub fn child_options(policy: Policy, keep: usize) -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync: policy.fsync(), segment_size: MIN_SEGMENT_SIZE },
        checkpoint: CheckpointOptions {
            wal_size: Some(8 << 10),
            interval: Some(Duration::from_millis(20)),
            on_close: false,
            keep,
            background: true,
        },
        create_if_missing: true,
    }
}

/// The parent's store for checking recovery: no background threads.
pub fn check_options(policy: Policy, keep: usize) -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync: policy.fsync(), segment_size: MIN_SEGMENT_SIZE },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: true, keep, background: false },
        create_if_missing: true,
    }
}

/// One thing the child does.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Commit(Step),
    Checkpoint,
    Sync,
    Sleep(Duration),
}

/// The child's script: endless and deterministic from its seed. Commits
/// take the steps of [`Stream`] in order.
#[derive(Debug)]
pub struct Script {
    rng: Rng,
    steps: Stream,
}

impl Script {
    pub fn new(seed: u64) -> Self {
        Script { rng: Rng::new(seed ^ 0x5C21_7A7E), steps: Stream::new(seed) }
    }

    /// The commit steps among the first `acts` acts of the script `seed`.
    pub fn commits(seed: u64, acts: usize) -> impl Iterator<Item = Step> {
        Script::new(seed).take(acts).filter_map(|act| match act {
            Act::Commit(step) => Some(step),
            _ => None,
        })
    }
}

impl Iterator for Script {
    type Item = Act;

    fn next(&mut self) -> Option<Act> {
        let r = self.rng.below(100);
        Some(match r {
            0..=73 => Act::Commit(self.steps.next()?),
            74..=80 => Act::Checkpoint,
            81..=85 => Act::Sync,
            _ => Act::Sleep(Duration::from_micros(self.rng.below(3000))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_depend_only_on_the_seed() {
        let a: Vec<Act> = Script::new(5).take(200).collect();
        assert_eq!(a, Script::new(5).take(200).collect::<Vec<_>>());
        assert!(a.iter().any(|x| matches!(x, Act::Checkpoint)));
        assert_eq!(Script::commits(5, 200).count(), a.iter().filter(|x| matches!(x, Act::Commit(_))).count());
        for policy in Policy::ALL {
            assert_eq!(policy.to_string().parse::<Policy>(), Ok(policy));
        }
    }
}
