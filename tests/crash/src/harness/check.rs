//! Checking a recovered store against the model, and the run's summary.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwdb::{verify, Error, Store, StoreRecovery};

use crate::model::{self, Model};
use crate::script::{check_options, Policy};

/// What a run did, to show that it covered what it should.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub policy: Option<Policy>,
    pub seed: u64,
    pub cycles: u64,
    pub elapsed: Duration,
    /// Kills after a random delay.
    pub delay_kills: u64,
    /// Failpoints reached, by rule (without skip): paused and killed,
    /// aborted, panicked.
    pub reached: BTreeMap<String, u64>,
    /// Failpoint plans whose point the child didn't reach.
    pub missed: u64,
    pub aborts: u64,
    pub os_crashes: u64,
    /// Recoveries checked by the parent, and child opens checked by digest.
    pub checked: u64,
    pub child_opens: u64,
    /// Acknowledged commits: total, and those an OS-crash simulation lost
    /// (allowed only after the synced seq).
    pub acknowledged: u64,
    pub lost_unsynced: u64,
    /// Recovered one past the last reported ack (the commit in flight).
    pub in_flight_found: u64,
    pub torn_tails: u64,
    pub discarded_frames: u64,
    pub temp_files_removed: u64,
    pub from_checkpoint: u64,
    /// Opens refused with `LogEndsBefore` (only `off` after an OS crash).
    pub refused: u64,
    /// New data directories.
    pub directories: u64,
    /// `verify` runs before a checked recovery (step 7), and of archives.
    pub verified: u64,
    pub archives_verified: u64,
    /// Keyed commits the children tried (step 8), and those answered from
    /// the key table (retries of commits the store had).
    pub keyed: u64,
    pub deduplicated: u64,
    /// The script's backups found complete (verified and restored), and
    /// found interrupted (refused).
    pub backups_complete: u64,
    pub backups_interrupted: u64,
    /// Restores run in a child: complete (compared with the model at their
    /// seq), and interrupted by a kill (refused); and kills at a restore
    /// failpoint.
    pub restores_complete: u64,
    pub restores_interrupted: u64,
    pub restore_kills: u64,
    /// The catalog scenario: namespaces, indexes and constraints (step 9).
    pub catalog: Option<crate::catalog::CatalogSummary>,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let policy = self.policy.map_or("?".into(), |p| p.to_string());
        writeln!(
            f,
            "policy {} seed {}: {} cycles in {:.1?}, {} recoveries checked ({} more by digest), no lost acknowledged commit, no partial transaction",
            policy, self.seed, self.cycles, self.elapsed, self.checked, self.child_opens
        )?;
        writeln!(
            f,
            "  kills: {} after a random delay, {} at a failpoint ({} aborts), {} plans missed their point",
            self.delay_kills,
            self.reached.values().sum::<u64>(),
            self.aborts,
            self.missed
        )?;
        writeln!(
            f,
            "  {} acknowledged commits; {} OS crashes simulated, losing {} acknowledged but unsynced commits; {} in-flight commits recovered",
            self.acknowledged, self.os_crashes, self.lost_unsynced, self.in_flight_found
        )?;
        writeln!(
            f,
            "  idempotency keys: {} keyed commits tried, {} answered from the key table (each key applied once)",
            self.keyed, self.deduplicated
        )?;
        writeln!(
            f,
            "  recovery: {} from a checkpoint, {} torn tails cut ({} frames discarded), {} temporary files removed, {} refused (off), {} data directories",
            self.from_checkpoint, self.torn_tails, self.discarded_frames, self.temp_files_removed, self.refused, self.directories
        )?;
        writeln!(
            f,
            "  verify: {} runs before recovery, {} of archives; backups: {} complete and restored, {} interrupted and refused; restores in a child: {} complete, {} interrupted and refused ({} killed at a failpoint)",
            self.verified,
            self.archives_verified,
            self.backups_complete,
            self.backups_interrupted,
            self.restores_complete,
            self.restores_interrupted,
            self.restore_kills
        )?;
        write!(f, "  failpoints reached:")?;
        for (rule, n) in &self.reached {
            write!(f, " {}={}", rule, n)?;
        }
        if let Some(catalog) = &self.catalog {
            write!(f, "\n{}", catalog)?;
        }
        Ok(())
    }
}

/// A violated guarantee (or a harness failure), with what reproduces it.
#[derive(Clone, Debug)]
pub struct Failure {
    pub policy: Policy,
    pub seed: u64,
    pub cycle: u64,
    pub message: String,
    pub work: PathBuf,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FAILED: policy {} seed {} cycle {}: {}\n  files kept in {}\n  rerun: iwdb-crash --policy {} --seed {} --cycles {}",
            self.policy,
            self.seed,
            self.cycle,
            self.message,
            self.work.display(),
            self.policy,
            self.seed,
            self.cycle + 1
        )
    }
}

/// The range recovery must land in: every acknowledged commit it must keep
/// (`lo`) up to the commit that may have been in flight (`hi`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub lo: u64,
    pub hi: u64,
}

impl Bounds {
    pub fn exact(seq: u64) -> Self {
        Bounds { lo: seq, hi: seq }
    }

    pub fn check(&self, seq: u64) -> Result<(), String> {
        if seq < self.lo {
            return Err(format!("recovered seq {}: acknowledged commits up to {} are lost", seq, self.lo));
        }
        if seq > self.hi {
            return Err(format!(
                "recovered seq {}, beyond the last commit that could be in the log ({})",
                seq, self.hi
            ));
        }
        Ok(())
    }
}

/// Verify the directory as the crash left it, open the store (the real
/// recovery), check its seq against `bounds` and its state against the
/// model at that seq. Verify must find no problem when recovery succeeds
/// (and must reach the same seq), and must find one when it refuses.
/// Returns the store.
pub fn check_recovery(
    dir: &Path,
    policy: Policy,
    keep: usize,
    archive: Option<&Path>,
    model: &mut Model,
    bounds: Bounds,
) -> Result<Store, CheckError> {
    let verified = verify(dir);
    let store = match Store::open(dir, check_options(policy, keep, archive)) {
        Ok(store) => store,
        Err(e) => {
            if matches!(&verified, Ok(report) if report.is_ok()) {
                return Err(CheckError::Violation(format!("recovery refused ({}), but verify found no problem", e)));
            }
            return Err(CheckError::Open(e));
        }
    };
    let seq = store.seq();
    match verified {
        Ok(report) if report.is_ok() && report.seq == Some(seq) => {}
        Ok(report) => {
            return Err(CheckError::Violation(format!(
                "verify before recovery found problems or another seq ({:?}, recovered {}): {:#?}",
                report.seq, seq, report.problems
            )))
        }
        // An interrupted initialization has no marker yet; the open finished it
        Err(Error::NotADataDir { .. }) if store.recovery().created => {}
        Err(e) => return Err(CheckError::Violation(format!("verify before recovery failed: {}", e))),
    }
    bounds.check(seq).map_err(CheckError::Violation)?;
    let expected = model::state(model.at(seq).map_err(CheckError::Violation)?);
    let actual = store.read(model::state);
    if actual != expected {
        return Err(CheckError::Violation(diff(&expected, &actual)));
    }
    if store.read_only().is_some() {
        return Err(CheckError::Violation("the recovered store is read-only".into()));
    }
    // No crash here damages a checkpoint under its name, so each loads, and
    // its saved indexes match its catalog
    let report = store.recovery();
    if !report.skipped_checkpoints.is_empty() || report.index_changes != Default::default() {
        return Err(CheckError::Violation(format!(
            "recovery skipped checkpoints {:?} or changed indexes {:?}",
            report.skipped_checkpoints, report.index_changes
        )));
    }
    Ok(store)
}

/// Why a recovery check failed.
#[derive(Debug)]
pub enum CheckError {
    Open(Error),
    Violation(String),
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::Open(e) => write!(f, "open failed: {}", e),
            CheckError::Violation(e) => f.write_str(e),
        }
    }
}

fn diff(expected: &model::State, actual: &model::State) -> String {
    let mut out = format!("the recovered state differs from the model at seq {}:", expected.2);
    if expected.2 != actual.2 {
        out += &format!(" seq {} vs {};", expected.2, actual.2);
    }
    if expected.1 != actual.1 {
        out += &format!(" catalog {:?} vs {:?};", expected.1, actual.1);
    }
    for line in expected.0.iter().filter(|l| !actual.0.contains(l)) {
        out += &format!("\n  missing: {}", line);
    }
    for line in actual.0.iter().filter(|l| !expected.0.contains(l)) {
        out += &format!("\n  unexpected: {}", line);
    }
    out
}

pub(super) fn note_report(summary: &mut Summary, report: &StoreRecovery) {
    summary.checked += 1;
    summary.from_checkpoint += u64::from(report.checkpoint.is_some());
    if let Some(tail) = &report.torn_tail {
        summary.torn_tails += 1;
        summary.discarded_frames += tail.discarded_frames;
    }
    summary.temp_files_removed += report.removed_temp_files.len() as u64;
}
