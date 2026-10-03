//! The parent: runs cycles of spawn, kill, (simulated OS crash), recover,
//! check. See the crate docs for the protocol and what is checked.

use std::fs::{self};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iwdb::{Error, IdempotencyKey, Store, StoreOptions};
use iwdb_storage::failpoint::{Action, Rule};

use crate::child::{ChildArgs, SYNC_LOG};
use crate::model::{self, Model};
use crate::os_crash::{self, OsCrash};
use crate::rng::Rng;
use crate::script::{check_options, Policy};

mod check;
mod plan;
mod process;
mod restore;

pub use check::*;
pub use plan::*;
pub use process::*;
pub use restore::*;

/// How long a child may take to reach a point before the run fails.
pub(crate) const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// A run of the harness.
#[derive(Clone, Debug)]
pub struct Config {
    /// The `iwdb-crash` binary, which runs the children.
    pub exe: PathBuf,
    pub policy: Policy,
    pub seed: u64,
    pub cycles: u64,
    /// Where data directories and child logs go.
    pub work: PathBuf,
    /// Acts in each child's script.
    pub acts: usize,
    /// The longest random kill delay.
    pub max_delay: Duration,
    /// Print a line every this many cycles (0: never).
    pub progress: u64,
    /// Cycles of the catalog scenario (step 9) run after the others.
    pub catalog_cycles: u64,
}

impl Config {
    pub fn new(exe: PathBuf, policy: Policy, seed: u64, cycles: u64, work: PathBuf) -> Self {
        Config {
            exe,
            policy,
            seed,
            cycles,
            work,
            acts: 120,
            max_delay: Duration::from_millis(60),
            progress: 0,
            catalog_cycles: cycles / 3,
        }
    }
}

/// A data directory under test: the model of what it holds, and what the
/// next open must recover.
#[derive(Debug)]
pub struct Target {
    pub path: PathBuf,
    /// Checkpoints to keep.
    pub keep: usize,
    pub model: Model,
    pub bounds: Bounds,
    /// No open has completed on it yet (initialization points apply).
    pub fresh: bool,
    /// Children run background checkpoints (the harness always does).
    pub background: bool,
    /// The store's WAL archive, if it archives.
    pub archive: Option<PathBuf>,
    /// Where the children's online backups go, if they take any.
    pub backups: Option<PathBuf>,
    /// The keyed commits the last child that tried any tried last: the
    /// next child retries them first (step 8).
    pub retries: Vec<IdempotencyKey>,
}

impl Target {
    pub fn new(path: PathBuf, keep: usize) -> Self {
        Target {
            path,
            keep,
            model: Model::default(),
            bounds: Bounds::exact(0),
            fresh: true,
            background: true,
            archive: None,
            backups: None,
            retries: Vec::new(),
        }
    }

    /// Remove the data directory, its archive and its backups.
    pub fn remove(&self) {
        for dir in [Some(&self.path), self.archive.as_ref(), self.backups.as_ref()].into_iter().flatten() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// A child that ran to its plan's point and was killed there (or died
/// there by itself).
#[derive(Clone, Debug)]
pub struct Crashed {
    pub outcome: Outcome,
    /// The plan's failpoint was reached (always true for a delay).
    pub reached: bool,
    /// The plan, the child's seed, its output and stderr, for messages.
    pub context: String,
}

/// Run a child with script `seed` on `target` under `plan`, and kill it
/// at the planned point. Checks how it ended and, by digest, the state its
/// open recovered, then moves the target's model and bounds past its
/// acknowledged commits.
pub fn crash(
    exe: &Path,
    work: &Path,
    policy: Policy,
    acts: usize,
    target: &mut Target,
    seed: u64,
    plan: &Plan,
) -> Result<Crashed, String> {
    let rules = match plan {
        Plan::At(rule) => vec![rule.clone()],
        Plan::Delay { .. } | Plan::Finish => vec![],
    };
    let _ = fs::remove_file(work.join(SYNC_LOG));
    let args = ChildArgs {
        dir: target.path.clone(),
        work: work.to_path_buf(),
        seed,
        acts,
        policy,
        keep: target.keep,
        background: target.background,
        archive: target.archive.clone(),
        backups: target.backups.clone(),
        retries: target.retries.clone(),
        rules,
    };
    let stderr = work.join("child.stderr");
    let mut child = ChildProcess::spawn(exe, &args, &stderr).map_err(|e| format!("spawn: {}", e))?;
    match plan {
        Plan::Delay { delay, from_spawn } => {
            if !from_spawn {
                child.wait_for(&["open", "error", "done"], CHILD_TIMEOUT);
            }
            std::thread::sleep(*delay);
        }
        Plan::At(rule) if rule.action == Action::Pause => {
            if child.wait_for(&["paused", "done", "error"], CHILD_TIMEOUT).is_none() {
                let _ = child.kill();
                return Err(format!("the child neither paused nor finished (plan {:?}, seed {})", plan, seed));
            }
        }
        // Abort or panic: wait until it dies (EOF) or finishes
        Plan::At(_) | Plan::Finish => {
            child.wait_for(&["done", "error"], CHILD_TIMEOUT);
        }
    }
    let outcome = child.kill().map_err(|e| format!("kill: {}", e))?;
    let context = format!(
        "plan {:?}, child seed {}; child output: {:?}; stderr: {}",
        plan,
        seed,
        outcome.lines,
        fs::read_to_string(&stderr).unwrap_or_default().trim()
    );
    if let Some(error) = outcome.error() {
        return Err(format!("the child failed: {} ({})", error, context));
    }
    let reached = match plan {
        Plan::Delay { .. } => true,
        Plan::Finish => outcome.done(),
        Plan::At(rule) if rule.action == Action::Pause => outcome.paused().is_some(),
        Plan::At(_) => !outcome.killed,
    };
    if !outcome.killed {
        // Only an abort or a panic ends a child by itself: a panic in the
        // commit path aborts (ADR 0008); one during open, or in a backup
        // (a file outside the data directory), unwinds (101)
        let expected = matches!(plan, Plan::At(rule) if matches!(rule.action, Action::Abort | Action::Panic));
        let outside = panicked_at(&stderr).is_some_and(|path| !path.starts_with(&target.path));
        let unwound = outcome.status.code() == Some(101) && (outcome.opened().is_none() || outside);
        if !expected || !(outcome.aborted() || unwound) {
            return Err(format!("the child exited by itself with {} ({})", outcome.status, context));
        }
    }

    // What the child's open recovered, checked by digest
    if let Some((seq, _, digest)) = outcome.opened() {
        target.bounds.check(seq).map_err(|e| format!("child open: {} ({})", e, context))?;
        if model::digest(target.model.at(seq)?) != digest {
            return Err(format!("the child opened a state at seq {} that differs from the model ({})", seq, context));
        }
        target.model.settle(seq)?;
        target.model.begin(seed, acts, &target.retries)?;
        target.fresh = false;
        // The next child retries the last two keyed commits this one tried:
        // the one in flight at the kill, if any, and one before it
        let tried = outcome.tried();
        if !tried.is_empty() {
            target.retries = tried[tried.len().saturating_sub(2)..].to_vec();
        }
        let acked = outcome.acked().unwrap_or(seq);
        // The commit in flight may be complete in the log, or acknowledged
        // but not yet reported; with the script finished there is none
        let hi = if outcome.done() { acked } else { acked + 1 };
        target.bounds = Bounds { lo: acked, hi };
    }
    // (A child killed before its open changed nothing recovery must keep)
    Ok(Crashed { outcome, reached, context })
}

/// The file an injected panic hit, from the child's stderr
/// (`injected panic at <rule> (<path>)`).
fn panicked_at(stderr: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(stderr).ok()?;
    let rest = text.split("injected panic at ").nth(1)?;
    let (_, path) = rest.split_once(" (")?;
    let end = path.find(")\n").unwrap_or(path.len());
    Some(PathBuf::from(&path[..end]))
}

/// Open the target's store now (the real recovery) and check it; on
/// success the model settles at the recovered seq, which the next open
/// must reach exactly.
pub fn check(target: &mut Target, policy: Policy) -> Result<Store, CheckError> {
    let archive = target.archive.clone();
    let store =
        check_recovery(&target.path, policy, target.keep, archive.as_deref(), &mut target.model, target.bounds)?;
    let seq = store.seq();
    target.model.settle(seq).map_err(CheckError::Violation)?;
    target.bounds = Bounds::exact(seq);
    target.fresh = false;
    Ok(store)
}

/// Run `config.cycles` kill/recover cycles. Stops at the first violation.
pub fn run(config: &Config) -> Result<Summary, Failure> {
    let start = Instant::now();
    let mut rng = Rng::new(config.seed);
    let mut summary = Summary { policy: Some(config.policy), seed: config.seed, ..Summary::default() };
    let failure = |cycle: u64, message: String| Failure {
        policy: config.policy,
        seed: config.seed,
        cycle,
        message,
        work: config.work.clone(),
    };
    fs::create_dir_all(&config.work).map_err(|e| failure(0, format!("create {}: {}", config.work.display(), e)))?;
    let mut target = new_target(config, &mut rng, &mut summary);

    for cycle in 0..config.cycles {
        let result = cycle_once(config, &mut rng, &mut target, &mut summary);
        if let Err(message) = result {
            return Err(failure(cycle, message));
        }
        summary.cycles += 1;
        if config.progress > 0 && (cycle + 1) % config.progress == 0 {
            eprintln!("  {} {}: {} cycles, {:.1?}", config.policy, config.seed, cycle + 1, start.elapsed());
        }
    }
    // Leave the directory checked
    if !target.fresh || target.bounds != Bounds::exact(0) {
        let store = check(&mut target, config.policy).map_err(|e| failure(config.cycles, e.to_string()))?;
        summary.verified += 1;
        note_report(&mut summary, store.recovery());
        drop(store);
        check_backups(&config.work, &target, &Outcome::none(), &mut summary).map_err(|e| failure(config.cycles, e))?;
        check_archive(&target, &mut summary).map_err(|e| failure(config.cycles, e))?;
    }
    if config.catalog_cycles > 0 {
        let work = config.work.join("catalog");
        let catalog = crate::catalog::run(
            &config.exe,
            config.policy,
            config.seed,
            config.catalog_cycles,
            config.acts,
            config.max_delay,
            &work,
        )
        .map_err(|e| failure(config.cycles, e))?;
        summary.catalog = Some(catalog);
    }
    summary.elapsed = start.elapsed();
    Ok(summary)
}

/// A new data directory, with an archive (most of the time) and a place
/// for backups.
fn new_target(config: &Config, rng: &mut Rng, summary: &mut Summary) -> Target {
    summary.directories += 1;
    let n = summary.directories;
    let mut target = Target::new(config.work.join(format!("dir-{}", n)), rng.range(1, 3) as usize);
    if rng.chance(3, 4) {
        target.archive = Some(config.work.join(format!("archive-{}", n)));
    }
    target.backups = Some(config.work.join(format!("backups-{}", n)));
    target
}

fn cycle_once(config: &Config, rng: &mut Rng, target: &mut Target, summary: &mut Summary) -> Result<(), String> {
    let policy = config.policy;
    // Now and then a new directory, for the initialization points
    if !target.fresh && target.bounds.lo == target.bounds.hi && rng.chance(1, 60) {
        target.remove();
        *target = new_target(config, rng, summary);
    }
    let seed = rng.next_u64();
    let plan = choose_plan(rng, target.fresh, config.max_delay);
    let crashed = crash(&config.exe, &config.work, policy, config.acts, target, seed, &plan)?;
    let outcome = &crashed.outcome;
    match &plan {
        Plan::Delay { .. } | Plan::Finish => summary.delay_kills += 1,
        Plan::At(rule) if crashed.reached => {
            let key = Rule { skip: 0, action: Action::Pause, ..rule.clone() };
            *summary.reached.entry(key.to_string()).or_default() += 1;
            summary.aborts += u64::from(rule.action != Action::Pause);
        }
        Plan::At(_) => summary.missed += 1,
    }
    summary.keyed += outcome.tried().len() as u64;
    summary.deduplicated += outcome.deduplicated() as u64;
    if let Some((seq, _, _)) = outcome.opened() {
        summary.child_opens += 1;
        summary.acknowledged += outcome.acked().unwrap_or(seq) - seq;
    }

    let mut os_crash: Option<OsCrash> = None;
    // An OS crash can lose something only under group and off (and the
    // commit in flight under always)
    let crash_chance = if policy == Policy::Always { 4 } else { 2 };
    if outcome.opened().is_some() && rng.chance(1, crash_chance) {
        os_crash = os_crash::simulate(
            &target.path.join("ns/00000000000000000001/wal"),
            &config.work.join(SYNC_LOG),
            policy,
            rng,
        )
        .map_err(|e| format!("OS crash simulation: {}", e))?;
        if os_crash.is_some() {
            summary.os_crashes += 1;
            // Acknowledged commits may be lost only after the synced seq
            target.bounds.lo = match policy {
                Policy::Always => target.bounds.lo,
                Policy::Group => outcome.synced(),
                Policy::Off => 0,
            };
        }
    }

    // Check the recovery now, or let the next child's open do it
    if os_crash.is_none() && rng.chance(1, 4) {
        return Ok(());
    }
    let hi = target.bounds.hi;
    summary.verified += 1;
    match check(target, policy) {
        Ok(store) => {
            let seq = store.seq();
            note_report(summary, store.recovery());
            if seq == hi && !outcome.done() && outcome.opened().is_some() {
                summary.in_flight_found += 1;
            }
            if os_crash.is_some() {
                summary.lost_unsynced += outcome.acked().unwrap_or(seq).saturating_sub(seq);
            }
            if rng.chance(1, 8) {
                store.close().map_err(|e| format!("close after recovery: {}", e))?;
            } else {
                drop(store);
            }
            let context = |e: String| format!("{} ({})", e, crashed.context);
            check_backups(&config.work, target, outcome, summary).map_err(context)?;
            check_archive(target, summary).map_err(context)?;
            if target.archive.is_some() && rng.chance(1, 4) {
                restore_cycle(config, rng, target, summary).map_err(context)?;
            }
            Ok(())
        }
        // `off` may refuse after an OS crash, exactly when a checkpoint is
        // newer than what is left of the log
        Err(CheckError::Open(Error::LogEndsBefore { from, next_seq }))
            if policy == Policy::Off && os_crash.is_some() && newest_checkpoint(&target.path) == Some(from - 1) =>
        {
            if from <= next_seq {
                return Err(format!("LogEndsBefore, but the log reaches the checkpoint ({})", crashed.context));
            }
            summary.refused += 1;
            target.remove();
            *target = new_target(config, rng, summary);
            Ok(())
        }
        Err(e) => Err(format!("{} ({}; OS crash {:?})", e, crashed.context, os_crash)),
    }
}

/// Options for opening a directory the harness checks: never create one.
fn open_existing(policy: Policy) -> StoreOptions {
    StoreOptions { create_if_missing: false, ..check_options(policy, 2, None) }
}

/// The seq of the newest checkpoint in a data directory.
pub fn newest_checkpoint(dir: &Path) -> Option<u64> {
    let checkpoints =
        iwdb_storage::checkpoint::list_checkpoints(&dir.join("ns/00000000000000000001/checkpoints")).ok()?;
    checkpoints.last().map(|(seq, _)| *seq)
}
