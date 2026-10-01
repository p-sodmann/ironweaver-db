//! The catalog scenario (step 9, ADR 0017): kill -9 a store while it
//! creates and drops namespaces, commits into several of them (data,
//! indexes and constraints) and checkpoints them, then check that what
//! recovery returns is a state the acts could have produced.
//!
//! The child (`iwdb-crash catalog ...`) runs a script of [`NsAct`]s over a
//! pool of namespace names, one at a time. Creates and drops always carry
//! an idempotency key; a third of the commits do. After each act it says
//! `ack`, so the act in flight at the kill is the one after the last `ack`.
//! Every act is atomic: a create at its event in the namespace log, a drop
//! at its event, a commit at its WAL record. So the recovered store is the
//! [`World`] after the acknowledged acts, or after one more (the one in
//! flight), and the next child retries that act by its key: it applies
//! once, whether the kill came before or after its commit point.
//!
//! The world is checked as a whole: the set of live namespaces, each id
//! (ids are never reused), and each namespace's canonical graph, catalog,
//! seq and key table. A process kill keeps the page cache, so the fsync
//! policy doesn't change what must survive; it changes where the kills land.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iwdb::{CommitOptions, Error, IdempotencyKey, Namespace, Store};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::testutil::workload::{Step, Stream};
use iwdb_engine::Prepare;
use iwdb_storage::failpoint::{Action, Call, Rule, When};

use crate::child::{fail, fail_fs, say, wait_for_kill};
use crate::harness::{ChildProcess, Plan, CHILD_TIMEOUT};
use crate::model::{self, State};
use crate::rng::Rng;
use crate::script::{check_options, child_options, Policy};

/// The namespace names the script picks from (`default` can't be dropped,
/// which the script tries now and then).
pub const NAMES: [&str; 5] = ["default", "a", "b", "c", "d"];

/// One thing the catalog child does.
#[derive(Clone, Debug, PartialEq)]
pub enum NsAct {
    Create(String, IdempotencyKey),
    Drop(String, IdempotencyKey),
    /// A commit into a namespace (which may not exist at the time).
    Commit(String, Step, Option<IdempotencyKey>),
    Checkpoint(String),
    Sleep(Duration),
}

/// The child's script: endless and deterministic from its seed.
pub struct NsScript {
    seed: u64,
    rng: Rng,
    streams: HashMap<String, Stream>,
    index: usize,
}

impl NsScript {
    pub fn new(seed: u64) -> Self {
        NsScript { seed, rng: Rng::new(seed ^ 0x4E53_5343), streams: HashMap::new(), index: 0 }
    }

    /// The key of act `index` of the script `seed`.
    pub fn key(seed: u64, index: usize) -> IdempotencyKey {
        // 16 hex digits, a dash and a number: always a valid key
        #[allow(clippy::expect_used)]
        IdempotencyKey::new(format!("{:016x}-{}", seed, index)).expect("a valid key")
    }

    /// The act a key names (to retry it).
    pub fn keyed(key: &IdempotencyKey) -> Result<NsAct, String> {
        let (seed, index) = key.as_str().split_once('-').ok_or_else(|| format!("not a script key: {}", key))?;
        let seed = u64::from_str_radix(seed, 16).map_err(|e| e.to_string())?;
        let index: usize = index.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        match NsScript::new(seed).nth(index) {
            Some(act) if act_key(&act) == Some(key) => Ok(act),
            other => Err(format!("key {} names {:?}, not a keyed act", key, other)),
        }
    }
}

fn act_key(act: &NsAct) -> Option<&IdempotencyKey> {
    match act {
        NsAct::Create(_, key) | NsAct::Drop(_, key) => Some(key),
        NsAct::Commit(_, _, key) => key.as_ref(),
        NsAct::Checkpoint(_) | NsAct::Sleep(_) => None,
    }
}

impl Iterator for NsScript {
    type Item = NsAct;

    fn next(&mut self) -> Option<NsAct> {
        let index = self.index;
        self.index += 1;
        let r = self.rng.below(100);
        let name = (*self.rng.pick(&NAMES)).to_owned();
        Some(match r {
            0..=7 => NsAct::Create(name, NsScript::key(self.seed, index)),
            8..=13 => NsAct::Drop(name, NsScript::key(self.seed, index)),
            14..=79 => {
                let stream_seed =
                    self.seed ^ name.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)));
                let step = self.streams.entry(name.clone()).or_insert_with(|| Stream::new(stream_seed)).next()?;
                let key = self.rng.chance(1, 3).then(|| NsScript::key(self.seed, index));
                NsAct::Commit(name, step, key)
            }
            80..=87 => NsAct::Checkpoint(name),
            _ => NsAct::Sleep(Duration::from_micros(self.rng.below(3000))),
        })
    }
}

/// Every namespace of a store: name to id and state.
pub type Snapshot = BTreeMap<String, (u64, State)>;

/// A digest of a snapshot, for the child to report what it opened.
pub fn digest(snapshot: &Snapshot) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("{:?}", snapshot).hash(&mut hasher);
    hasher.finish()
}

/// A short description of a snapshot, for messages.
pub fn describe(snapshot: &Snapshot) -> String {
    snapshot.iter().map(|(name, (id, state))| format!("{}#{}@{}", name, id, state.2)).collect::<Vec<_>>().join(" ")
}

/// The snapshot of an open store.
pub fn snapshot<F: iwdb::LogFs + Clone + Send + Sync + 'static>(store: &Store<F>) -> Result<Snapshot, String>
where
    F::File: Send,
{
    let mut all = Snapshot::new();
    for info in store.namespaces() {
        let ns = store.namespace(info.name.as_str()).map_err(|e| e.to_string())?;
        all.insert(info.name.to_string(), (info.id, ns.read(model::state)));
    }
    Ok(all)
}

/// What an act did to the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Created,
    Dropped,
    Committed,
    /// Nothing: it fails, or it is a retry of an act the world has, or it
    /// changes no state.
    Nothing,
}

/// The reference: the namespaces the acts have made, each an engine
/// namespace with its id, and the keys of creates and drops.
pub struct World {
    live: BTreeMap<String, (u64, Namespace)>,
    next_id: u64,
    /// Keys of creates and drops (never evicted, as in the log).
    keys: HashMap<String, (bool, String)>,
}

impl Default for World {
    fn default() -> Self {
        World {
            live: BTreeMap::from([("default".to_owned(), (1, empty("default")))]),
            next_id: 2,
            keys: HashMap::new(),
        }
    }
}

fn empty(name: &str) -> Namespace {
    // The script's names are valid
    #[allow(clippy::expect_used)]
    Namespace::new(NamespaceName::new(name).expect("a valid name"))
}

impl World {
    pub fn snapshot(&self) -> Snapshot {
        self.live.iter().map(|(name, (id, ns))| (name.clone(), (*id, model::state(ns)))).collect()
    }

    pub fn apply(&mut self, act: &NsAct) -> Result<Effect, String> {
        Ok(match act {
            NsAct::Create(name, key) => {
                if self.keys.contains_key(key.as_str()) || self.live.contains_key(name) {
                    return Ok(Effect::Nothing);
                }
                self.live.insert(name.clone(), (self.next_id, empty(name)));
                self.next_id += 1;
                self.keys.insert(key.as_str().to_owned(), (true, name.clone()));
                Effect::Created
            }
            NsAct::Drop(name, key) => {
                if self.keys.contains_key(key.as_str()) || name == "default" || !self.live.contains_key(name) {
                    return Ok(Effect::Nothing);
                }
                self.live.remove(name);
                self.keys.insert(key.as_str().to_owned(), (false, name.clone()));
                Effect::Dropped
            }
            NsAct::Commit(name, step, key) => {
                let Some((_, ns)) = self.live.get_mut(name) else { return Ok(Effect::Nothing) };
                let prepared = match step {
                    Step::Tx(mutations) => ns.prepare_keyed(mutations, key.as_ref()),
                    Step::Catalog(change) => ns.prepare_catalog_keyed(change.clone(), key.as_ref()),
                };
                // A step that fails validation fails in the child too, and a
                // retry of a commit the world has commits nothing in either
                match prepared {
                    Ok(Prepare::New(prepared)) => {
                        ns.apply(prepared, None).map_err(|e| format!("the world failed to apply: {}", e))?;
                        Effect::Committed
                    }
                    _ => Effect::Nothing,
                }
            }
            NsAct::Checkpoint(_) | NsAct::Sleep(_) => Effect::Nothing,
        })
    }
}

/// What a catalog child runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogArgs {
    pub dir: PathBuf,
    pub seed: u64,
    pub acts: usize,
    pub policy: Policy,
    pub keep: usize,
    pub archive: Option<PathBuf>,
    /// Keyed acts to retry first.
    pub retries: Vec<IdempotencyKey>,
    pub rules: Vec<Rule>,
}

impl CatalogArgs {
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--dir".into(),
            self.dir.display().to_string(),
            "--seed".into(),
            self.seed.to_string(),
            "--acts".into(),
            self.acts.to_string(),
            "--policy".into(),
            self.policy.to_string(),
            "--keep".into(),
            self.keep.to_string(),
        ];
        if let Some(archive) = &self.archive {
            args.push("--archive".into());
            args.push(archive.display().to_string());
        }
        for key in &self.retries {
            args.push("--retry".into());
            args.push(key.as_str().to_owned());
        }
        for rule in &self.rules {
            args.push("--rule".into());
            args.push(rule.to_string());
        }
        args
    }

    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed = CatalogArgs {
            dir: PathBuf::new(),
            seed: 0,
            acts: 0,
            policy: Policy::Always,
            keep: 2,
            archive: None,
            retries: Vec::new(),
            rules: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{} needs a value", flag))?;
            let number = |v: &str| v.parse::<u64>().map_err(|e| format!("{} {}: {}", flag, v, e));
            match flag.as_str() {
                "--dir" => parsed.dir = value.into(),
                "--seed" => parsed.seed = number(value)?,
                "--acts" => parsed.acts = number(value)? as usize,
                "--policy" => parsed.policy = value.parse()?,
                "--keep" => parsed.keep = number(value)? as usize,
                "--archive" => parsed.archive = Some(value.into()),
                "--retry" => parsed.retries.push(IdempotencyKey::new(value.as_str()).map_err(|e| e.to_string())?),
                "--rule" => parsed.rules.push(value.parse()?),
                other => return Err(format!("unknown catalog option '{}'", other)),
            }
        }
        Ok(parsed)
    }
}

/// The catalog child's main: never returns. Protocol lines: `open
/// <digest>`, `ack` (an act finished, whatever its outcome), `dedup` (a
/// keyed act answered from its key), `paused <rule> <path>`, `done`,
/// `error <message>`.
pub fn main(args: &CatalogArgs) -> ! {
    let fs = fail_fs(&args.rules);
    let mut options = child_options(args.policy, args.keep, args.archive.as_deref());
    options.checkpoint.background = true;
    let store = match Store::open_with(fs, &args.dir, options) {
        Ok(store) => store,
        Err(e) => fail("open", e),
    };
    match snapshot(&store) {
        Ok(snapshot) => say(&format!("open {:016x}", digest(&snapshot))),
        Err(e) => fail("snapshot", e),
    }
    for key in &args.retries {
        match NsScript::keyed(key) {
            Ok(act) => run_act(&store, act),
            Err(e) => fail("retry", e),
        }
    }
    for act in NsScript::new(args.seed).take(args.acts) {
        run_act(&store, act);
    }
    say("done");
    wait_for_kill()
}

fn run_act<F: iwdb::LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, act: NsAct)
where
    F::File: Send,
{
    match act {
        NsAct::Create(name, key) => match store.create_namespace(&name, Some(&key)) {
            Ok(result) if result.deduplicated => say("dedup"),
            Ok(_) | Err(Error::NamespaceExists { .. }) => {}
            Err(e) => fail("create", e),
        },
        NsAct::Drop(name, key) => match store.drop_namespace(&name, Some(&key)) {
            Ok(result) if result.deduplicated => say("dedup"),
            // `default` can't be dropped
            Ok(_) | Err(Error::NoSuchNamespace { .. } | Error::InvalidOptions(_)) => {}
            Err(e) => fail("drop", e),
        },
        NsAct::Commit(name, step, key) => match store.namespace(&name) {
            Err(Error::NoSuchNamespace { .. }) => {}
            Err(e) => fail("namespace", e),
            Ok(ns) => {
                let options = CommitOptions { idempotency_key: key.clone() };
                let result = match step {
                    Step::Tx(mutations) => ns.commit_with(&mutations, &options),
                    Step::Catalog(change) => ns.commit_catalog_with(change, &options),
                };
                match result {
                    Ok(result) if key.is_some() && result.deduplicated => say("dedup"),
                    Ok(_) | Err(Error::Engine(_)) => {}
                    Err(e) => fail("commit", e),
                }
            }
        },
        NsAct::Checkpoint(name) => match store.namespace(&name) {
            Err(Error::NoSuchNamespace { .. }) => {}
            Err(e) => fail("namespace", e),
            Ok(ns) => {
                if let Err(e) = ns.checkpoint() {
                    fail("checkpoint", e);
                }
            }
        },
        NsAct::Sleep(duration) => std::thread::sleep(duration),
    }
    say("ack");
}

/// The failpoints of the catalog scenario, with the largest skip: the
/// namespace log, the namespace directories, and the per-namespace WAL,
/// checkpoint and archive files.
fn points() -> Vec<(Call, When, &'static str, u64)> {
    use Call::*;
    use When::*;
    vec![
        (Write, Midway, "/wal/", 40),
        (Write, After, "/wal/", 40),
        (Sync, Before, "/wal/", 40),
        (Sync, After, "/wal/", 40),
        (Create, After, "/wal/", 8),
        (Rename, After, "/wal/", 8),
        (WriteAtomic, Midway, "/checkpoints/", 6),
        (WriteAtomic, WriterDone, "/checkpoints/", 6),
        (WriteAtomic, After, "/checkpoints/", 6),
        (RemoveFile, Before, "/wal/", 8),
        (RemoveFile, After, "/wal/", 8),
        (RemoveFile, After, "/checkpoints/", 4),
        // The namespace log: every kind of write
        (Write, Before, "NAMESPACES", 8),
        (Write, Midway, "NAMESPACES", 8),
        (Write, After, "NAMESPACES", 8),
        (Sync, Before, "NAMESPACES", 8),
        (Sync, After, "NAMESPACES", 8),
        (OpenAppend, Before, "NAMESPACES", 3),
        (Truncate, Before, "NAMESPACES", 2),
        // The namespace directories: create, remove, and their syncs
        (CreateDir, Before, "/ns/", 12),
        (CreateDir, After, "/ns/", 12),
        (RemoveDirAll, Before, "/ns/", 6),
        (RemoveDirAll, After, "/ns/", 6),
        (SyncDir, Before, "/ns", 24),
        (SyncDir, After, "/ns", 24),
        // Archiving, including a drop's final segments and the archive's copy of the log
        (Create, After, "/archive-", 8),
        (Write, Midway, "/archive-", 8),
        (Rename, After, "/archive-", 8),
        (CreateDir, After, "/archive-", 4),
        (SyncDir, After, "/archive-", 8),
    ]
}

fn choose_plan(rng: &mut Rng, max_delay: Duration) -> Plan {
    let r = rng.below(100);
    if r < 30 {
        let delay = Duration::from_micros(rng.below(max_delay.as_micros() as u64 + 1));
        return Plan::Delay { delay, from_spawn: rng.chance(1, 8) };
    }
    let points = points();
    let &(call, when, path, max_skip) = rng.pick(&points);
    let action = if r < 90 { Action::Pause } else { Action::Abort };
    Plan::At(Rule::new(call, when, action).path(path).skip(rng.below(max_skip + 1)))
}

/// What the scenario did.
#[derive(Clone, Debug, Default)]
pub struct CatalogSummary {
    pub cycles: u64,
    pub elapsed: Duration,
    pub delay_kills: u64,
    pub reached: BTreeMap<String, u64>,
    pub missed: u64,
    pub aborts: u64,
    /// Recoveries checked by the parent, and child opens checked by digest.
    pub checked: u64,
    pub child_opens: u64,
    /// Acknowledged acts, and what they did.
    pub acks: u64,
    pub creates: u64,
    pub drops: u64,
    pub commits: u64,
    /// Keyed acts answered from their key (retries of acts that had
    /// happened), and unacknowledged acts found complete.
    pub deduplicated: u64,
    pub in_flight_found: u64,
    /// The most namespaces live at once, and orphan directories recovery removed.
    pub max_namespaces: usize,
}

impl std::fmt::Display for CatalogSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "  catalog scenario: {} cycles in {:.1?}, {} recoveries checked ({} more by digest); kills: {} after a delay, {} at a failpoint ({} aborts), {} missed",
            self.cycles,
            self.elapsed,
            self.checked,
            self.child_opens,
            self.delay_kills,
            self.reached.values().sum::<u64>(),
            self.aborts,
            self.missed
        )?;
        writeln!(
            f,
            "    {} acts acknowledged: {} namespaces created, {} dropped, {} commits; {} keyed acts answered from their key, {} in-flight acts found complete; up to {} namespaces at once",
            self.acks, self.creates, self.drops, self.commits, self.deduplicated, self.in_flight_found, self.max_namespaces
        )?;
        write!(f, "    failpoints reached:")?;
        for (rule, n) in &self.reached {
            write!(f, " {}={}", rule, n)?;
        }
        Ok(())
    }
}

/// The act in flight at the last kill, and the keys the next child retries.
#[derive(Default)]
struct Pending {
    in_flight: Option<NsAct>,
    retries: Vec<IdempotencyKey>,
}

/// The store recovered to `actual`: the world after the acknowledged acts
/// or after the one in flight; the world moves there.
fn resolve(
    world: &mut World,
    pending: &mut Pending,
    actual: &Snapshot,
    summary: &mut CatalogSummary,
) -> Result<(), String> {
    if world.snapshot() == *actual {
        pending.in_flight = None;
        return Ok(());
    }
    if let Some(act) = pending.in_flight.take() {
        world.apply(&act)?;
        if world.snapshot() == *actual {
            summary.in_flight_found += 1;
            return Ok(());
        }
        return Err(format!(
            "recovered [{}], which is neither the acknowledged world [before {:?}] nor the one with the act in flight",
            describe(actual),
            act
        ));
    }
    Err(format!("recovered [{}], expected [{}] (no act in flight)", describe(actual), describe(&world.snapshot())))
}

/// Open the directory as the parent (the real recovery) and check it.
fn check_dir(
    dir: &Path,
    archive: Option<&Path>,
    policy: Policy,
    keep: usize,
    world: &mut World,
    pending: &mut Pending,
    summary: &mut CatalogSummary,
) -> Result<(), String> {
    if dir.join("IWDB").exists() {
        let report = iwdb::verify(dir).map_err(|e| format!("verify: {}", e))?;
        if !report.is_ok() {
            return Err(format!("verify before recovery found damage: {:#?}", report.problems));
        }
    }
    let store =
        Store::open(dir, check_options(policy, keep, archive)).map_err(|e| format!("recovery failed: {}", e))?;
    let actual = snapshot(&store)?;
    resolve(world, pending, &actual, summary)?;
    for info in store.namespaces() {
        let violations =
            store.namespace(info.name.as_str()).map_err(|e| e.to_string())?.read(iwdb_engine::invariants::check);
        if !violations.is_empty() {
            return Err(format!("namespace '{}' violates invariants: {:?}", info.name, violations));
        }
    }
    summary.checked += 1;
    summary.max_namespaces = summary.max_namespaces.max(actual.len());
    store.close().map_err(|e| format!("close: {}", e))?;
    let report = iwdb::verify(dir).map_err(|e| format!("verify: {}", e))?;
    if !report.is_ok() {
        return Err(format!("verify after recovery found damage: {:#?}", report.problems));
    }
    if let Some(archive) = archive {
        let report = iwdb::verify(archive).map_err(|e| format!("verify archive: {}", e))?;
        if !report.is_ok() {
            return Err(format!("the archive has damage: {:#?}", report.problems));
        }
    }
    Ok(())
}

/// Run `cycles` kill/recover cycles of the catalog scenario in `work`.
pub fn run(
    exe: &Path,
    policy: Policy,
    seed: u64,
    cycles: u64,
    acts: usize,
    max_delay: Duration,
    work: &Path,
) -> Result<CatalogSummary, String> {
    run_with(exe, policy, seed, cycles, acts, max_delay, work, &[])
}

/// [`run`] with the plans of the cycles given (cycle `i` takes plan `i`
/// modulo their number, and every cycle is checked by the parent); no
/// plans: random ones.
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    exe: &Path,
    policy: Policy,
    seed: u64,
    cycles: u64,
    acts: usize,
    max_delay: Duration,
    work: &Path,
    plans: &[Plan],
) -> Result<CatalogSummary, String> {
    let start = Instant::now();
    let mut rng = Rng::new(seed ^ 0x0CA7_A106);
    let mut summary = CatalogSummary::default();
    fs::create_dir_all(work).map_err(|e| format!("create {}: {}", work.display(), e))?;
    let mut directories = 0;
    let mut dir = PathBuf::new();
    let mut archive: Option<PathBuf> = None;
    let mut keep = 1;
    let mut world = World::default();
    let mut pending = Pending::default();
    let mut fresh = true;

    for cycle in 0..cycles {
        let failure = |message: String| format!("catalog cycle {}: {}", cycle, message);
        if fresh || (pending.in_flight.is_none() && rng.chance(1, 50)) {
            directories += 1;
            dir = work.join(format!("ns-dir-{}", directories));
            archive = rng.chance(3, 4).then(|| work.join(format!("ns-archive-{}", directories)));
            keep = rng.range(1, 3) as usize;
            world = World::default();
            pending = Pending::default();
            fresh = false;
        }
        let child_seed = rng.next_u64() % 1_000_000_000_000;
        let plan = if plans.is_empty() {
            choose_plan(&mut rng, max_delay)
        } else {
            plans[cycle as usize % plans.len()].clone()
        };
        let rules = match &plan {
            Plan::At(rule) => vec![rule.clone()],
            Plan::Delay { .. } | Plan::Finish => vec![],
        };
        let args = CatalogArgs {
            dir: dir.clone(),
            seed: child_seed,
            acts,
            policy,
            keep,
            archive: archive.clone(),
            retries: pending.retries.clone(),
            rules,
        };
        let stderr = work.join("catalog.stderr");
        let mut child = ChildProcess::spawn_command(exe, "catalog", args.to_args(), &stderr)
            .map_err(|e| failure(format!("spawn: {}", e)))?;
        match &plan {
            Plan::Delay { delay, from_spawn } => {
                if !from_spawn {
                    child.wait_for(&["open", "error", "done"], CHILD_TIMEOUT);
                }
                std::thread::sleep(*delay);
            }
            Plan::At(rule) if rule.action == Action::Pause => {
                if child.wait_for(&["paused", "done", "error"], CHILD_TIMEOUT).is_none() {
                    let _ = child.kill();
                    return Err(failure(format!("the child neither paused nor finished (plan {:?})", plan)));
                }
            }
            Plan::At(_) | Plan::Finish => {
                child.wait_for(&["done", "error"], CHILD_TIMEOUT);
            }
        }
        let outcome = child.kill().map_err(|e| failure(format!("kill: {}", e)))?;
        let context = format!(
            "plan {:?}, child seed {}; child output (tail): {:?}; stderr: {}",
            plan,
            child_seed,
            &outcome.lines[outcome.lines.len().saturating_sub(6)..],
            fs::read_to_string(&stderr).unwrap_or_default().trim()
        );
        if let Some(line) = outcome.lines.iter().find_map(|l| l.strip_prefix("error ")) {
            return Err(failure(format!("the child failed: {} ({})", line, context)));
        }
        let paused = outcome.lines.iter().any(|l| l.starts_with("paused "));
        let done = outcome.lines.iter().any(|l| l == "done");
        match &plan {
            Plan::Delay { .. } => summary.delay_kills += 1,
            Plan::At(rule) => {
                let mut key = rule.clone();
                key.skip = 0;
                if paused || (!outcome.killed && outcome.aborted()) {
                    *summary.reached.entry(key.to_string()).or_default() += 1;
                    summary.aborts += u64::from(!paused);
                } else {
                    summary.missed += 1;
                }
            }
            Plan::Finish => {}
        }
        if !outcome.killed && !outcome.aborted() && !done {
            return Err(failure(format!("the child exited by itself with {} ({})", outcome.status, context)));
        }

        // What the child's open recovered, by digest: it settles the act in flight at the last kill
        let opened = outcome
            .lines
            .iter()
            .find_map(|l| l.strip_prefix("open "))
            .and_then(|hex| u64::from_str_radix(hex, 16).ok());
        if let Some(opened) = opened {
            if pending.in_flight.is_some() && world.snapshot() != Snapshot::new() && digest(&world.snapshot()) != opened
            {
                // It must be the world with the act in flight
                let act = pending.in_flight.take().ok_or_else(|| failure("no act in flight".into()))?;
                world.apply(&act).map_err(failure)?;
                summary.in_flight_found += 1;
            } else {
                pending.in_flight = None;
            }
            if digest(&world.snapshot()) != opened {
                return Err(failure(format!(
                    "the child opened a state that differs from the world [{}] ({})",
                    describe(&world.snapshot()),
                    context
                )));
            }
            summary.child_opens += 1;
            // The acknowledged acts: the retries, then the script's
            let acked = outcome.lines.iter().filter(|l| *l == "ack").count();
            summary.deduplicated += outcome.lines.iter().filter(|l| *l == "dedup").count() as u64;
            let mut sequence: Vec<NsAct> = Vec::new();
            for key in &pending.retries {
                sequence.push(NsScript::keyed(key).map_err(failure)?);
            }
            sequence.extend(NsScript::new(child_seed).take(acts));
            for act in sequence.iter().take(acked) {
                match world.apply(act).map_err(failure)? {
                    Effect::Created => summary.creates += 1,
                    Effect::Dropped => summary.drops += 1,
                    Effect::Committed => summary.commits += 1,
                    Effect::Nothing => {}
                }
            }
            summary.acks += acked as u64;
            summary.max_namespaces = summary.max_namespaces.max(world.live.len());
            // The next child retries the act in flight, if keyed
            pending.in_flight = if done { None } else { sequence.get(acked).cloned() };
            pending.retries = pending.in_flight.as_ref().and_then(act_key).cloned().into_iter().collect();
        }

        if !plans.is_empty() || rng.chance(3, 4) {
            check_dir(&dir, archive.as_deref(), policy, keep, &mut world, &mut pending, &mut summary)
                .map_err(|e| failure(format!("{} ({})", e, context)))?;
        }
        summary.cycles += 1;
    }
    check_dir(&dir, archive.as_deref(), policy, keep, &mut world, &mut pending, &mut summary)
        .map_err(|e| format!("catalog, final check: {}", e))?;
    summary.elapsed = start.elapsed();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_depend_only_on_the_seed_and_keys_name_their_act() {
        let a: Vec<NsAct> = NsScript::new(9).take(300).collect();
        assert_eq!(a, NsScript::new(9).take(300).collect::<Vec<_>>());
        assert!(a.iter().any(|x| matches!(x, NsAct::Create(..))));
        assert!(a.iter().any(|x| matches!(x, NsAct::Drop(..))));
        assert!(a.iter().any(|x| matches!(x, NsAct::Commit(_, Step::Catalog(_), _))));
        for act in &a {
            if let Some(key) = act_key(act) {
                assert_eq!(NsScript::keyed(key).as_ref(), Ok(act));
            }
        }
        let args = CatalogArgs {
            dir: "/tmp/a b".into(),
            seed: 5,
            acts: 10,
            policy: Policy::Group,
            keep: 2,
            archive: Some("/tmp/x".into()),
            retries: vec![NsScript::key(5, 3)],
            rules: vec![Rule::new(Call::CreateDir, When::After, Action::Pause).skip(2).path("/ns/")],
        };
        assert_eq!(CatalogArgs::parse(&args.to_args()), Ok(args));
    }

    #[test]
    fn the_world_applies_each_key_once() {
        let mut world = World::default();
        let key = NsScript::key(1, 0);
        assert_eq!(world.apply(&NsAct::Create("a".into(), key.clone())), Ok(Effect::Created));
        assert_eq!(world.apply(&NsAct::Create("a".into(), key.clone())), Ok(Effect::Nothing));
        assert_eq!(world.apply(&NsAct::Drop("a".into(), NsScript::key(1, 1))), Ok(Effect::Dropped));
        // The create's key still answers: no second namespace
        assert_eq!(world.apply(&NsAct::Create("a".into(), key)), Ok(Effect::Nothing));
        assert_eq!(world.apply(&NsAct::Create("a".into(), NsScript::key(1, 2))), Ok(Effect::Created));
        assert_eq!(world.snapshot()["a"].0, 3, "ids are never reused");
        assert_eq!(world.apply(&NsAct::Drop("default".into(), NsScript::key(1, 3))), Ok(Effect::Nothing));
    }
}
