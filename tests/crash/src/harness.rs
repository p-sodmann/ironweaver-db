//! The parent: runs cycles of spawn, kill, (simulated OS crash), recover,
//! check. See the crate docs for the protocol and what is checked.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use iwdb::{Error, RecoveryReport, Store};
use iwdb_storage::failpoint::{Action, Call, Rule, When};

use crate::child::{ChildArgs, SYNC_LOG};
use crate::model::{self, Model};
use crate::os_crash::{self, OsCrash};
use crate::rng::Rng;
use crate::script::{check_options, Policy};

/// How long a child may take to reach a point before the run fails.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// A running child and its protocol lines.
pub struct ChildProcess {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
    eof: bool,
}

/// What a child reported, and how it ended.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub lines: Vec<String>,
    pub status: ExitStatus,
    /// The parent killed it (otherwise it exited by itself).
    pub killed: bool,
}

impl ChildProcess {
    /// Run `exe child <args>`, with stderr into `stderr`.
    pub fn spawn(exe: &Path, args: &ChildArgs, stderr: &Path) -> std::io::Result<Self> {
        let mut child = Command::new(exe)
            .arg("child")
            .args(args.to_args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::create(stderr)?)
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Ok(ChildProcess { child, lines, seen: Vec::new(), eof: false })
    }

    /// Wait for a line that starts with one of `prefixes`, until the child
    /// closes its stdout or `timeout` passes. Returns the line.
    pub fn wait_for(&mut self, prefixes: &[&str], timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while !self.eof {
            match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if prefixes.iter().any(|p| line.starts_with(p)) {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => self.eof = true,
            }
        }
        None
    }

    /// `kill -9` the child (if it still runs), and collect every line it
    /// wrote.
    pub fn kill(mut self) -> std::io::Result<Outcome> {
        // A child that closed its stdout is dying by itself (an abort can
        // take a moment, for the crash report): let it
        if self.eof {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.child.try_wait()?.is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let killed = self.child.try_wait()?.is_none();
        if killed {
            // SIGKILL on Unix: no destructor, no flush, no unwinding
            self.child.kill()?;
        }
        let status = self.child.wait()?;
        self.seen.extend(self.lines.iter());
        Ok(Outcome { lines: self.seen, status, killed })
    }
}

impl Outcome {
    fn words(&self, prefix: &str) -> impl Iterator<Item = Vec<&str>> {
        let prefix = format!("{} ", prefix);
        self.lines.iter().filter_map(move |l| l.strip_prefix(prefix.as_str())).map(|rest| rest.split(' ').collect())
    }

    /// `open`: seq, synced seq, digest.
    pub fn opened(&self) -> Option<(u64, u64, u64)> {
        let w = self.words("open").next()?;
        Some((w.first()?.parse().ok()?, w.get(1)?.parse().ok()?, u64::from_str_radix(w.get(2)?, 16).ok()?))
    }

    /// The last acknowledged seq.
    pub fn acked(&self) -> Option<u64> {
        self.words("ack").filter_map(|w| w.first()?.parse().ok()).last()
    }

    /// The highest synced seq the child reported.
    pub fn synced(&self) -> u64 {
        let acks = self.words("ack").filter_map(|w| w.get(1)?.parse::<u64>().ok());
        acks.chain(self.opened().map(|o| o.1)).max().unwrap_or(0)
    }

    pub fn paused(&self) -> Option<&str> {
        self.lines.iter().find_map(|l| l.strip_prefix("paused "))
    }

    pub fn done(&self) -> bool {
        self.lines.iter().any(|l| l == "done")
    }

    pub fn error(&self) -> Option<&str> {
        self.lines.iter().find_map(|l| l.strip_prefix("error "))
    }

    /// Killed by SIGABRT (`std::process::abort`).
    pub fn aborted(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            self.status.signal() == Some(6)
        }
        #[cfg(not(unix))]
        {
            !self.status.success()
        }
    }
}

/// When the parent kills the child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// After this long, counted from the child's `open` line (or from the
    /// spawn, to kill during recovery).
    Delay { delay: Duration, from_spawn: bool },
    /// At a failpoint: pause (then kill), abort or panic there.
    At(Rule),
    /// When the script is done (to build up history).
    Finish,
}

/// The failpoints the plans pick from, with the largest skip. Paths:
/// `/wal/` matches segment files (and `.tmp` segments), `/wal` also the
/// directory; the same for `/checkpoints`.
fn points() -> Vec<(Call, When, &'static str, u64)> {
    use Call::*;
    use When::*;
    vec![
        // Appends, fsyncs and rotations
        (Write, Midway, "/wal/", 40),
        (Write, Before, "/wal/", 40),
        (Write, After, "/wal/", 40),
        (Sync, Before, "/wal/", 40),
        (Sync, After, "/wal/", 40),
        (Create, After, "/wal/", 8),
        (Rename, Before, "/wal/", 8),
        (Rename, After, "/wal/", 8),
        (SyncDir, Before, "/wal", 8),
        (OpenAppend, Before, "/wal/", 8),
        // Checkpoints: the file, its directory sync, removals
        (WriteAtomic, Before, "/checkpoints/", 3),
        (WriteAtomic, Midway, "/checkpoints/", 3),
        (WriteAtomic, WriterDone, "/checkpoints/", 3),
        (WriteAtomic, After, "/checkpoints/", 3),
        (SyncDir, Before, "/checkpoints", 4),
        (SyncDir, After, "/checkpoints", 4),
        (RemoveFile, Before, "/checkpoints/", 2),
        (RemoveFile, After, "/checkpoints/", 2),
        (RemoveFile, Before, "/wal/", 8),
        (RemoveFile, After, "/wal/", 8),
        // Recovery: the torn tail, temporary files
        (Truncate, Before, "", 0),
        (Truncate, After, "", 0),
    ]
}

/// Points reached only when a directory is initialized.
fn init_points() -> Vec<Rule> {
    vec![
        Rule::new(Call::SyncDir, When::Before, Action::Pause),
        Rule::new(Call::WriteAtomic, When::Before, Action::Pause).path("IWDB"),
        Rule::new(Call::WriteAtomic, When::WriterDone, Action::Pause).path("IWDB"),
        Rule::new(Call::WriteAtomic, When::After, Action::Pause).path("IWDB"),
    ]
}

fn choose_plan(rng: &mut Rng, fresh: bool, max_delay: Duration) -> Plan {
    if fresh && rng.chance(1, 4) {
        return Plan::At(rng.pick(&init_points()).clone());
    }
    let r = rng.below(100);
    if r < 40 {
        let delay = Duration::from_micros(rng.below(max_delay.as_micros() as u64 + 1));
        return Plan::Delay { delay, from_spawn: rng.chance(1, 8) };
    }
    let points = points();
    let &(call, when, path, max_skip) = rng.pick(&points);
    let action = match r {
        40..=87 => Action::Pause,
        88..=95 => Action::Abort,
        // A panic is only a crash on the commit path (ADR 0008)
        _ if matches!(call, Call::Write | Call::Sync) => Action::Panic,
        _ => Action::Abort,
    };
    Plan::At(Rule::new(call, when, action).path(path).skip(rng.below(max_skip + 1)))
}

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
}

impl Config {
    pub fn new(exe: PathBuf, policy: Policy, seed: u64, cycles: u64, work: PathBuf) -> Self {
        Config { exe, policy, seed, cycles, work, acts: 120, max_delay: Duration::from_millis(60), progress: 0 }
    }
}

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
            "  recovery: {} from a checkpoint, {} torn tails cut ({} frames discarded), {} temporary files removed, {} refused (off), {} data directories",
            self.from_checkpoint, self.torn_tails, self.discarded_frames, self.temp_files_removed, self.refused, self.directories
        )?;
        write!(f, "  failpoints reached:")?;
        for (rule, n) in &self.reached {
            write!(f, " {}={}", rule, n)?;
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

/// Open the store (the real recovery), check its seq against `bounds` and
/// its state against the model at that seq. Returns the store.
pub fn check_recovery(
    dir: &Path,
    policy: Policy,
    keep: usize,
    model: &mut Model,
    bounds: Bounds,
) -> Result<Store, CheckError> {
    let store = Store::open(dir, check_options(policy, keep)).map_err(CheckError::Open)?;
    let seq = store.seq();
    bounds.check(seq).map_err(CheckError::Violation)?;
    let expected = model::state(model.at(seq).map_err(CheckError::Violation)?);
    let actual = store.read(model::state);
    if actual != expected {
        return Err(CheckError::Violation(diff(&expected, &actual)));
    }
    if store.read_only().is_some() {
        return Err(CheckError::Violation("the recovered store is read-only".into()));
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

fn note_report(summary: &mut Summary, report: &RecoveryReport) {
    summary.checked += 1;
    summary.from_checkpoint += u64::from(report.checkpoint.is_some());
    if let Some(tail) = &report.torn_tail {
        summary.torn_tails += 1;
        summary.discarded_frames += tail.discarded_frames;
    }
    summary.temp_files_removed += report.removed_temp_files.len() as u64;
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
}

impl Target {
    pub fn new(path: PathBuf, keep: usize) -> Self {
        Target { path, keep, model: Model::default(), bounds: Bounds::exact(0), fresh: true, background: true }
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
        // commit path aborts (ADR 0008), one during open unwinds (101)
        let expected = matches!(plan, Plan::At(rule) if matches!(rule.action, Action::Abort | Action::Panic));
        let unwound = outcome.status.code() == Some(101) && outcome.opened().is_none();
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
        target.model.begin(seed, acts);
        target.fresh = false;
        let acked = outcome.acked().unwrap_or(seq);
        // The commit in flight may be complete in the log, or acknowledged
        // but not yet reported; with the script finished there is none
        let hi = if outcome.done() { acked } else { acked + 1 };
        target.bounds = Bounds { lo: acked, hi };
    }
    // (A child killed before its open changed nothing recovery must keep)
    Ok(Crashed { outcome, reached, context })
}

/// Open the target's store now (the real recovery) and check it; on
/// success the model settles at the recovered seq, which the next open
/// must reach exactly.
pub fn check(target: &mut Target, policy: Policy) -> Result<Store, CheckError> {
    let store = check_recovery(&target.path, policy, target.keep, &mut target.model, target.bounds)?;
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
        note_report(&mut summary, store.recovery());
    }
    summary.elapsed = start.elapsed();
    Ok(summary)
}

fn new_target(config: &Config, rng: &mut Rng, summary: &mut Summary) -> Target {
    summary.directories += 1;
    Target::new(config.work.join(format!("dir-{}", summary.directories)), rng.range(1, 3) as usize)
}

fn cycle_once(config: &Config, rng: &mut Rng, target: &mut Target, summary: &mut Summary) -> Result<(), String> {
    let policy = config.policy;
    // Now and then a new directory, for the initialization points
    if !target.fresh && target.bounds.lo == target.bounds.hi && rng.chance(1, 60) {
        let _ = fs::remove_dir_all(&target.path);
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
    if let Some((seq, _, _)) = outcome.opened() {
        summary.child_opens += 1;
        summary.acknowledged += outcome.acked().unwrap_or(seq) - seq;
    }

    let mut os_crash: Option<OsCrash> = None;
    // An OS crash can lose something only under group and off (and the
    // commit in flight under always)
    let crash_chance = if policy == Policy::Always { 4 } else { 2 };
    if outcome.opened().is_some() && rng.chance(1, crash_chance) {
        os_crash = os_crash::simulate(&target.path.join("wal"), &config.work.join(SYNC_LOG), policy, rng)
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
            let _ = fs::remove_dir_all(&target.path);
            *target = new_target(config, rng, summary);
            Ok(())
        }
        Err(e) => Err(format!("{} ({}; OS crash {:?})", e, crashed.context, os_crash)),
    }
}

/// The seq of the newest checkpoint in a data directory.
pub fn newest_checkpoint(dir: &Path) -> Option<u64> {
    let checkpoints = iwdb_storage::checkpoint::list_checkpoints(&dir.join("checkpoints")).ok()?;
    checkpoints.last().map(|(seq, _)| *seq)
}
