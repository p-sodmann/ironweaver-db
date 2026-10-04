//! What a child does: the fsync policy and store options it runs with, and
//! its script of commits, checkpoints, syncs and pauses, generated from a
//! seed. The parent generates the same script to know which commits the
//! child attempted, in which order.

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use iwdb::{CheckpointOptions, FsyncPolicy, IdempotencyKey, StoreOptions, WalOptions};
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
/// segment removal (and archiving, with an `archive`).
pub fn child_options(policy: Policy, keep: usize, archive: Option<&Path>) -> StoreOptions {
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
        archive: archive.map(Path::to_path_buf),
        retention: Default::default(),
    }
}

/// The parent's store for checking recovery: no background threads, the
/// same archive (its checkpoint on close removes segments too).
pub fn check_options(policy: Policy, keep: usize, archive: Option<&Path>) -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync: policy.fsync(), segment_size: MIN_SEGMENT_SIZE },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: true, keep, background: false },
        create_if_missing: true,
        archive: archive.map(Path::to_path_buf),
        retention: Default::default(),
    }
}

/// One thing the child does.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// A commit, with an idempotency key for about a third of them.
    Commit(Step, Option<IdempotencyKey>),
    Checkpoint,
    Sync,
    /// An online backup into a new directory (if the child has one for
    /// backups).
    Backup,
    Sleep(Duration),
}

/// The child's script: endless and deterministic from its seed. Commits
/// take the steps of [`Stream`] in order.
#[derive(Debug)]
pub struct Script {
    seed: u64,
    rng: Rng,
    steps: Stream,
    /// The index of the next act.
    index: usize,
}

impl Script {
    pub fn new(seed: u64) -> Self {
        Script { seed, rng: Rng::new(seed ^ 0x5C21_7A7E), steps: Stream::new(seed), index: 0 }
    }

    /// The commits (with their keys) among the first `acts` acts of the
    /// script `seed`.
    pub fn commits(seed: u64, acts: usize) -> impl Iterator<Item = (Step, Option<IdempotencyKey>)> {
        Script::new(seed).take(acts).filter_map(|act| match act {
            Act::Commit(step, key) => Some((step, key)),
            _ => None,
        })
    }

    /// The idempotency key of act `index` of the script `seed`: both can be
    /// read back from it ([`keyed`](Self::keyed)).
    pub fn key(seed: u64, index: usize) -> IdempotencyKey {
        // 16 hex digits, a dash and a number: always a valid key
        #[allow(clippy::expect_used)]
        IdempotencyKey::new(format!("{:016x}-{}", seed, index)).expect("a valid key")
    }

    /// The keyed commit a key names: the same step, to retry it.
    pub fn keyed(key: &IdempotencyKey) -> Result<Step, String> {
        let (seed, index) = key.as_str().split_once('-').ok_or_else(|| format!("not a script key: {}", key))?;
        let seed = u64::from_str_radix(seed, 16).map_err(|e| e.to_string())?;
        let index: usize = index.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        match Script::new(seed).nth(index) {
            Some(Act::Commit(step, Some(k))) if &k == key => Ok(step),
            other => Err(format!("key {} names {:?}, not a keyed commit", key, other)),
        }
    }
}

impl Iterator for Script {
    type Item = Act;

    fn next(&mut self) -> Option<Act> {
        let index = self.index;
        self.index += 1;
        let r = self.rng.below(100);
        Some(match r {
            0..=73 => {
                let step = self.steps.next()?;
                let key = self.rng.chance(1, 3).then(|| Script::key(self.seed, index));
                Act::Commit(step, key)
            }
            74..=80 => Act::Checkpoint,
            81..=85 => Act::Sync,
            86..=87 => Act::Backup,
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
        assert!(a.iter().any(|x| matches!(x, Act::Backup)));
        assert_eq!(Script::commits(5, 200).count(), a.iter().filter(|x| matches!(x, Act::Commit(..))).count());
        // Keys name their commit
        let keyed: Vec<_> = Script::commits(5, 200).filter_map(|(step, key)| Some((step, key?))).collect();
        assert!(keyed.len() > 20);
        for (step, key) in keyed {
            assert_eq!(Script::keyed(&key), Ok(step));
        }
        assert!(Script::keyed(&IdempotencyKey::new("nope").expect("key")).is_err());
        for policy in Policy::ALL {
            assert_eq!(policy.to_string().parse::<Policy>(), Ok(policy));
        }
    }
}
