//! The parent: runs cycles of spawn, kill, (simulated OS crash), recover,
//! check. See the crate docs for the protocol and what is checked.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use iwdb::{verify, Error, IdempotencyKey, RestoreSources, RestoreTarget, Store, StoreOptions, StoreRecovery};
use iwdb_storage::failpoint::{Action, Call, Rule, When};
use iwdb_storage::layout::{MARKER_NAME, RESTORING_NAME};

use crate::child::{ChildArgs, RestoreArgs, SYNC_LOG};
use crate::model::{self, Model};
use crate::os_crash::{self, OsCrash};
use crate::rng::Rng;
use crate::script::{check_options, Policy};

/// How long a child may take to reach a point before the run fails.
pub(crate) const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

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
        Self::spawn_command(exe, "child", args.to_args(), stderr)
    }

    /// Run `exe <command> <args>`, with stderr into `stderr`.
    pub fn spawn_command(exe: &Path, command: &str, args: Vec<String>, stderr: &Path) -> std::io::Result<Self> {
        let mut child = Command::new(exe)
            .arg(command)
            .args(args)
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
    /// No child ran.
    pub fn none() -> Self {
        #[cfg(unix)]
        let status = std::os::unix::process::ExitStatusExt::from_raw(0);
        #[cfg(not(unix))]
        let status = std::os::windows::process::ExitStatusExt::from_raw(0);
        Outcome { lines: Vec::new(), status, killed: false }
    }

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

    /// The backups the child reported: path and seq.
    pub fn backups(&self) -> HashMap<PathBuf, u64> {
        let backups = self.lines.iter().filter_map(|l| l.strip_prefix("backup "));
        backups
            .filter_map(|rest| rest.split_once(' '))
            .filter_map(|(seq, path)| Some((path.into(), seq.parse().ok()?)))
            .collect()
    }

    /// The idempotency keys the child tried, in order.
    pub fn tried(&self) -> Vec<IdempotencyKey> {
        self.words("try").filter_map(|w| IdempotencyKey::new(*w.first()?).ok()).collect()
    }

    /// Keyed commits answered from the key table.
    pub fn deduplicated(&self) -> usize {
        self.words("dedup").count()
    }

    /// The seq the restore child reported.
    pub fn restored(&self) -> Option<u64> {
        self.words("restored").find_map(|w| w.first()?.parse().ok())
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
        // Archiving, before the checkpointer removes segments (step 7)
        (Create, After, "/archive-", 4),
        (Write, Midway, "/archive-", 4),
        (Sync, Before, "/archive-", 4),
        (Rename, Before, "/archive-", 4),
        (Rename, After, "/archive-", 4),
        (SyncDir, Before, "/archive-", 2),
        (SyncDir, After, "/archive-", 2),
        // The script's online backups: files, syncs, manifest, marker
        (Create, After, "/backups-", 4),
        (Write, Midway, "/backups-", 6),
        (Sync, Before, "/backups-", 6),
        (SyncDir, Before, "/backups-", 6),
        (WriteAtomic, Before, "/backups-", 1),
        (WriteAtomic, WriterDone, "/backups-", 1),
        (WriteAtomic, After, "/backups-", 1),
    ]
}

/// The failpoints of a restore (in the restore child), with the largest
/// skip.
fn restore_points() -> Vec<(Call, When, &'static str, u64)> {
    use Call::*;
    use When::*;
    vec![
        (Create, After, RESTORING_NAME, 0),
        (Sync, Before, RESTORING_NAME, 0),
        (SyncDir, Before, "/restored", 8),
        (SyncDir, After, "/restored", 8),
        (WriteAtomic, Midway, "/restored/ns/", 0),
        (WriteAtomic, WriterDone, "/restored/ns/", 0),
        (WriteAtomic, After, "/restored/ns/", 0),
        (RemoveFile, Before, RESTORING_NAME, 0),
        (RemoveFile, After, RESTORING_NAME, 0),
        (WriteAtomic, Before, "restored/IWDB", 0),
        (WriteAtomic, After, "restored/IWDB", 0),
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
        // A panic is only a crash on the commit path (ADR 0008): the WAL's
        // writes and fsyncs (though "/wal/" also matches a backup's
        // segments, see `crash`)
        _ if matches!(call, Call::Write | Call::Sync) && path == "/wal/" => Action::Panic,
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

fn note_report(summary: &mut Summary, report: &StoreRecovery) {
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

/// Check every backup the children left in the target's backup directory,
/// then remove them. A complete one (with a marker) must verify, be at
/// most at the recovered seq (a backup holds only synced commits, which a
/// crash can't lose), match the seq the child reported, and restore to the
/// model's state at its seq. One without a marker (interrupted) must be
/// refused by verify and restore.
pub fn check_backups(work: &Path, target: &Target, outcome: &Outcome, summary: &mut Summary) -> Result<(), String> {
    let Some(dir) = target.backups.as_ref().filter(|d| d.exists()) else { return Ok(()) };
    let reported = outcome.backups();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("list {}: {}", dir.display(), e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    let restored = work.join("restored-backup");
    for backup in entries {
        match verify(&backup) {
            Ok(report) => {
                let seq = report.seq.unwrap_or(0);
                if !report.is_ok() {
                    return Err(format!("backup {}: verify found problems: {:#?}", backup.display(), report.problems));
                }
                if reported.get(&backup).is_some_and(|r| *r != seq) {
                    return Err(format!(
                        "backup {} is at seq {}, the child said {:?}",
                        backup.display(),
                        seq,
                        reported.get(&backup)
                    ));
                }
                let expected = target.model.state_at(seq).map_err(|e| format!("backup {}: {}", backup.display(), e))?;
                let _ = fs::remove_dir_all(&restored);
                let sources = RestoreSources { backup: Some(backup.clone()), archive: None };
                iwdb::restore(&restored, &sources, RestoreTarget::Latest)
                    .map_err(|e| format!("restoring backup {}: {}", backup.display(), e))?;
                let store = Store::open(&restored, open_existing(Policy::Always)).map_err(|e| e.to_string())?;
                if store.read(model::state) != expected {
                    return Err(format!(
                        "backup {} restores to another state than the model's at {}",
                        backup.display(),
                        seq
                    ));
                }
                summary.backups_complete += 1;
            }
            Err(Error::NotADataDir { .. }) => {
                if reported.contains_key(&backup) {
                    return Err(format!("the child reported backup {}, but it has no marker", backup.display()));
                }
                let _ = fs::remove_dir_all(&restored);
                let sources = RestoreSources { backup: Some(backup.clone()), archive: None };
                if iwdb::restore(&restored, &sources, RestoreTarget::Latest).is_ok() {
                    return Err(format!("an interrupted backup {} restored", backup.display()));
                }
                summary.backups_interrupted += 1;
            }
            Err(e) => return Err(format!("verify backup {}: {}", backup.display(), e)),
        }
        let _ = fs::remove_dir_all(&backup);
    }
    let _ = fs::remove_dir_all(&restored);
    Ok(())
}

/// The target's archive: it verifies, it holds the store's history from
/// seq 1 (the store archived from its creation), and together with the
/// WAL it reaches the recovered seq without a gap: no segment the
/// checkpointer removed is lost.
pub fn check_archive(target: &Target, summary: &mut Summary) -> Result<(), String> {
    let Some(archive) = target.archive.as_ref().filter(|a| a.exists()) else { return Ok(()) };
    let report = verify(archive).map_err(|e| format!("verify archive: {}", e))?;
    summary.archives_verified += 1;
    if !report.is_ok() {
        return Err(format!("the archive has problems: {:#?}", report.problems));
    }
    let wal =
        iwdb_storage::list_segments(&target.path.join("ns/00000000000000000001/wal")).map_err(|e| e.to_string())?;
    let wal_first = wal.first().map_or(u64::MAX, |(s, _)| *s);
    match (report.first_seq, report.last_seq) {
        (Some(first), Some(last)) => {
            if first != 1 {
                return Err(format!("the archive starts at seq {}, not 1", first));
            }
            if last + 1 < wal_first {
                return Err(format!(
                    "the archive ends at seq {} and the WAL starts at {}: removed segments were lost",
                    last, wal_first
                ));
            }
        }
        _ if wal_first > 1 && wal_first != u64::MAX => {
            return Err(format!("the WAL starts at {} but the archive holds nothing", wal_first))
        }
        _ => {}
    }
    Ok(())
}

/// How a restore in a child ended, as checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restored {
    /// Complete, and equal to the model at its seq.
    Complete,
    /// Interrupted, and refused by a store (or nothing but an empty or
    /// missing directory).
    Interrupted,
}

/// When the restore child is killed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestorePlan {
    /// Not at all: it finishes.
    Finish,
    /// After this long, counted from the spawn.
    Delay(Duration),
    /// At a failpoint: pause (then kill) or abort.
    At(Rule),
}

/// Restore the target to seq `n` in a child process into `dest`, from its
/// archive and, with `from_data`, its data directory (a cold copy; no
/// store may have it open), killed per `plan`. Then check what is left:
/// complete and equal to the model at `n`, or refused.
pub fn restore_in_child(
    exe: &Path,
    work: &Path,
    target: &Target,
    n: u64,
    from_data: bool,
    plan: &RestorePlan,
    dest: &Path,
) -> Result<(Restored, Outcome), String> {
    let _ = fs::remove_dir_all(dest);
    let rules = match plan {
        RestorePlan::At(rule) => vec![rule.clone()],
        _ => Vec::new(),
    };
    let args = RestoreArgs {
        backup: from_data.then(|| target.path.clone()),
        archive: target.archive.clone(),
        dest: dest.to_path_buf(),
        seq: Some(n),
        rules,
    };
    let stderr = work.join("restore.stderr");
    let mut child = ChildProcess::spawn_command(exe, "restore", args.to_args(), &stderr)
        .map_err(|e| format!("spawn restore: {}", e))?;
    match plan {
        RestorePlan::Delay(delay) => std::thread::sleep(*delay),
        _ => {
            child.wait_for(&["done", "error", "paused"], CHILD_TIMEOUT);
        }
    }
    let outcome = child.kill().map_err(|e| format!("kill restore: {}", e))?;
    let context = format!("restore to {} ({:?}, from data: {}): {:?}", n, plan, from_data, outcome.lines);
    if let Some(error) = outcome.error() {
        return Err(format!("the restore failed: {} ({})", error, context));
    }
    let complete = dest.join(MARKER_NAME).exists() && !dest.join(RESTORING_NAME).exists();
    if outcome.restored().is_some() && !complete {
        return Err(format!("the restore reported success but left no marker ({})", context));
    }
    let restored = match Store::open(dest, open_existing(Policy::Always)) {
        Ok(store) => {
            if !complete {
                return Err(format!("an incomplete restore opened ({})", context));
            }
            if store.read(model::state) != target.model.state_at(n)? {
                return Err(format!("the restore differs from the model at seq {} ({})", n, context));
            }
            Restored::Complete
        }
        Err(Error::InterruptedRestore { .. }) | Err(Error::NotADataDir { .. }) if !complete => Restored::Interrupted,
        Err(e) => return Err(format!("opening the restore: {} ({})", e, context)),
    };
    Ok((restored, outcome))
}

/// Restore the target (its data directory as a cold copy, and its
/// archive; or the archive alone when it reaches the seq) to a random seq
/// in a child, sometimes killed: at a random moment or at a restore
/// failpoint ([`restore_in_child`]).
fn restore_cycle(config: &Config, rng: &mut Rng, target: &Target, summary: &mut Summary) -> Result<(), String> {
    let Some(archive) = target.archive.as_ref() else { return Ok(()) };
    let n = rng.range(0, target.model.seq());
    let archive_end = verify(archive).map_err(|e| e.to_string())?.last_seq.unwrap_or(0);
    let from_data = n > archive_end || rng.chance(1, 2);
    let r = rng.below(100);
    let plan = match r {
        0..=39 => RestorePlan::Finish,
        40..=59 => RestorePlan::Delay(Duration::from_micros(rng.below(30_000))),
        _ => {
            let points = restore_points();
            let &(call, when, path, max_skip) = rng.pick(&points);
            let action = if r >= 95 { Action::Abort } else { Action::Pause };
            RestorePlan::At(Rule::new(call, when, action).path(path).skip(rng.below(max_skip + 1)))
        }
    };
    let dest = config.work.join("restored");
    let (restored, outcome) = restore_in_child(&config.exe, &config.work, target, n, from_data, &plan, &dest)?;
    if matches!(plan, RestorePlan::At(_)) && (outcome.paused().is_some() || !outcome.killed) {
        summary.restore_kills += 1;
    }
    match restored {
        Restored::Complete => summary.restores_complete += 1,
        Restored::Interrupted => summary.restores_interrupted += 1,
    }
    let _ = fs::remove_dir_all(&dest);
    Ok(())
}

/// The seq of the newest checkpoint in a data directory.
pub fn newest_checkpoint(dir: &Path) -> Option<u64> {
    let checkpoints =
        iwdb_storage::checkpoint::list_checkpoints(&dir.join("ns/00000000000000000001/checkpoints")).ok()?;
    checkpoints.last().map(|(seq, _)| *seq)
}
