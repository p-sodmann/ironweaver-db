//! What the server's memory holds, and the limit that refuses writes before
//! it runs out (ADR 0054).
//!
//! A store owns one [`Memory`]. The parts that hold memory report to it
//! through [`Charge`]s: each live namespace its graph and payloads, each
//! checkpointer its copy, each analytics projection and index build its
//! working memory. A charge is a number of bytes the holder sets as it
//! changes and that is released when the charge is dropped. Reading the
//! totals takes no lock.
//!
//! With a limit, every change re-evaluates the state ([`MemoryState`]):
//! `warn` at `warn_at` of the limit, `refusing writes` at
//! `refuse_writes_at`, each left only 5 % of the limit below its line
//! (hysteresis). While refusing, [`Memory::check_write`] fails with
//! [`Error::MemoryLimit`]; the commit pipeline asks it before a write is
//! logged. Each change of state is logged once.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::Error;

/// The band below each threshold that `used` must fall under before the
/// state falls, as a fraction of the limit.
pub const HYSTERESIS: f64 = 0.05;

/// Defaults of [`MemoryOptions`].
pub const DEFAULT_WARN_AT: f64 = 0.80;
pub const DEFAULT_REFUSE_WRITES_AT: f64 = 0.90;

/// cgroup v1 reports "no limit" as a page-aligned `i64::MAX`; anything this
/// large is no limit.
const V1_UNLIMITED: u64 = 1 << 60;

/// What holds memory: the parts of [`Memory::used`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// The live graphs: the core's `Graph::memory_usage` (structure and
    /// indexes).
    Graph,
    /// The live graphs' payloads, estimated (`Namespace::payload_bytes`).
    Payload,
    /// The checkpointers' copies of the namespaces, graph and payloads.
    Checkpoint,
    /// Analytics projections and index builds while they run.
    Working,
}

impl Part {
    pub const ALL: [Part; 4] = [Part::Graph, Part::Payload, Part::Checkpoint, Part::Working];

    pub fn as_str(self) -> &'static str {
        match self {
            Part::Graph => "graph",
            Part::Payload => "payload",
            Part::Checkpoint => "checkpoint",
            Part::Working => "working",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The memory state (ADR 0054).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemoryState {
    /// Below `warn_at` (or no limit).
    #[default]
    Normal,
    /// At or above `warn_at`: writes are accepted.
    Warn,
    /// At or above `refuse_writes_at`: writes that add are refused.
    RefusingWrites,
}

impl MemoryState {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryState::Normal => "normal",
            MemoryState::Warn => "warn",
            MemoryState::RefusingWrites => "refusing_writes",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => MemoryState::Warn,
            2 => MemoryState::RefusingWrites,
            _ => MemoryState::Normal,
        }
    }
}

/// Where the limit came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LimitSource {
    /// `[memory] limit_bytes` (or [`MemoryOptions::limit_bytes`]).
    Config,
    /// The cgroup v2 `memory.max` of the process.
    CgroupV2,
    /// The cgroup v1 `memory.limit_in_bytes` of the process.
    CgroupV1,
}

impl LimitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitSource::Config => "config",
            LimitSource::CgroupV2 => "cgroup v2",
            LimitSource::CgroupV1 => "cgroup v1",
        }
    }
}

/// The memory limit as configured: `limit_bytes` (`None`: no limit) and
/// the thresholds as fractions of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemoryOptions {
    pub limit_bytes: Option<u64>,
    pub source: LimitSource,
    /// Warn from this fraction of the limit on (default 0.80).
    pub warn_at: f64,
    /// Refuse writes from this fraction of the limit on (default 0.90).
    pub refuse_writes_at: f64,
}

impl Default for MemoryOptions {
    /// No limit.
    fn default() -> Self {
        MemoryOptions {
            limit_bytes: None,
            source: LimitSource::Config,
            warn_at: DEFAULT_WARN_AT,
            refuse_writes_at: DEFAULT_REFUSE_WRITES_AT,
        }
    }
}

impl MemoryOptions {
    /// Check the thresholds: `HYSTERESIS < warn_at <= refuse_writes_at <= 1`
    /// (each must leave room for its band), and a limit above 0.
    pub fn check(&self) -> Result<(), String> {
        let (w, r) = (self.warn_at, self.refuse_writes_at);
        if !(w > HYSTERESIS && w <= r && r <= 1.0) {
            return Err(format!(
                "memory thresholds must satisfy {} < warn_at <= refuse_writes_at <= 1, not warn_at = {}, refuse_writes_at = {}",
                HYSTERESIS, w, r
            ));
        }
        if self.limit_bytes == Some(0) {
            return Err("memory limit_bytes must be above 0 (leave it out for no limit)".into());
        }
        Ok(())
    }
}

/// The limit in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub bytes: u64,
    pub source: LimitSource,
    /// `warn_at` of the limit.
    pub warn: u64,
    /// `refuse_writes_at` of the limit.
    pub refuse_writes: u64,
    band: u64,
}

/// A snapshot of [`Memory`] for the status and the metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemorySnapshot {
    pub graph: u64,
    pub payload: u64,
    pub checkpoint: u64,
    pub working: u64,
    pub limit: Option<Limit>,
    pub state: MemoryState,
}

impl MemorySnapshot {
    /// Every part: what the limit counts.
    pub fn used(&self) -> u64 {
        self.graph.saturating_add(self.payload).saturating_add(self.checkpoint).saturating_add(self.working)
    }
}

/// The store's memory accounting and limit (see the module docs). Shared
/// as `Arc<Memory>`; every method takes `&self` and no lock except a
/// change of state.
#[derive(Debug)]
pub struct Memory {
    parts: [AtomicU64; 4],
    limit: Option<Limit>,
    state: AtomicU8,
    /// Serializes changes of state, so that each is logged once.
    transition: Mutex<()>,
}

impl Memory {
    /// Accounting without a limit: never refuses.
    pub fn unlimited() -> Arc<Self> {
        Self::new(&MemoryOptions::default())
    }

    /// Accounting with `options`' limit. Thresholds are expected to be
    /// [checked](MemoryOptions::check); out-of-range ones are clamped.
    pub fn new(options: &MemoryOptions) -> Arc<Self> {
        let limit = options.limit_bytes.filter(|&b| b > 0).map(|bytes| {
            let at = |f: f64| (bytes as f64 * f.clamp(0.0, 1.0)) as u64;
            Limit {
                bytes,
                source: options.source,
                warn: at(options.warn_at),
                refuse_writes: at(options.refuse_writes_at),
                band: at(HYSTERESIS),
            }
        });
        Arc::new(Memory {
            parts: Default::default(),
            limit,
            state: AtomicU8::new(MemoryState::Normal as u8),
            transition: Mutex::new(()),
        })
    }

    /// A new charge on `part`, of 0 bytes.
    pub fn charge(self: &Arc<Self>, part: Part) -> Charge {
        Charge { memory: self.clone(), part, bytes: AtomicU64::new(0) }
    }

    pub fn limit(&self) -> Option<Limit> {
        self.limit
    }

    pub fn state(&self) -> MemoryState {
        MemoryState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn part(&self, part: Part) -> u64 {
        self.parts[part.index()].load(Ordering::Relaxed)
    }

    /// Every part's bytes: what the limit counts.
    pub fn used(&self) -> u64 {
        self.snapshot().used()
    }

    pub fn snapshot(&self) -> MemorySnapshot {
        MemorySnapshot {
            graph: self.part(Part::Graph),
            payload: self.part(Part::Payload),
            checkpoint: self.part(Part::Checkpoint),
            working: self.part(Part::Working),
            limit: self.limit,
            state: self.state(),
        }
    }

    /// `Ok` unless writes are refused now: [`Error::MemoryLimit`] then.
    /// O(1), no lock.
    pub fn check_write(&self) -> Result<(), Error> {
        match (self.state(), self.limit) {
            (MemoryState::RefusingWrites, Some(limit)) => {
                Err(Error::MemoryLimit { used: self.used(), refuse_at: limit.refuse_writes, limit: limit.bytes })
            }
            _ => Ok(()),
        }
    }

    /// Re-evaluate the state after a part changed.
    fn changed(&self) {
        let Some(limit) = self.limit else { return };
        let used = self.used();
        let current = self.state();
        if next_state(current, used, &limit) == current {
            return;
        }
        let _guard = self.transition.lock().unwrap_or_else(PoisonError::into_inner);
        // Again under the lock: another thread may have moved it already
        let (current, used) = (self.state(), self.used());
        let next = next_state(current, used, &limit);
        if next == current {
            return;
        }
        self.state.store(next as u8, Ordering::Release);
        let pct = |b: u64| 100.0 * b as f64 / limit.bytes as f64;
        let (used_pct, limit_bytes) = (pct(used), limit.bytes);
        match next {
            MemoryState::RefusingWrites => log::error!(
                "memory at {used} bytes, {used_pct:.1} % of the limit of {limit_bytes} bytes: refusing writes until it falls below {:.0} % (deletes and drops are accepted)",
                pct(limit.refuse_writes.saturating_sub(limit.band))
            ),
            MemoryState::Warn if current == MemoryState::Normal => log::warn!(
                "memory at {used} bytes, {used_pct:.1} % of the limit of {limit_bytes} bytes: above the warning line of {:.0} %",
                pct(limit.warn)
            ),
            MemoryState::Warn => log::info!(
                "memory at {used} bytes, {used_pct:.1} % of the limit of {limit_bytes} bytes: accepting writes again"
            ),
            MemoryState::Normal => {
                log::info!(
                    "memory at {used} bytes, {used_pct:.1} % of the limit of {limit_bytes} bytes: back to normal"
                )
            }
        }
    }
}

/// The state after `current` with `used` bytes: it rises as soon as a line
/// is reached, and falls only below a line minus the band.
fn next_state(current: MemoryState, used: u64, limit: &Limit) -> MemoryState {
    let above = |line: u64| used >= line;
    let below = |line: u64| used < line.saturating_sub(limit.band);
    let rising = if above(limit.refuse_writes) {
        MemoryState::RefusingWrites
    } else if above(limit.warn) {
        MemoryState::Warn
    } else {
        MemoryState::Normal
    };
    if rising >= current {
        return rising;
    }
    // Falling: one line at a time, each with its band
    match current {
        MemoryState::RefusingWrites if !below(limit.refuse_writes) => MemoryState::RefusingWrites,
        MemoryState::RefusingWrites | MemoryState::Warn if !below(limit.warn) => MemoryState::Warn,
        _ => MemoryState::Normal,
    }
}

/// Bytes held by one holder of memory, counted in its [`Part`] until the
/// charge is dropped. [`set`](Self::set) it as the holder grows and shrinks.
#[derive(Debug)]
pub struct Charge {
    memory: Arc<Memory>,
    part: Part,
    bytes: AtomicU64,
}

impl Charge {
    /// The holder holds `bytes` now. O(1).
    pub fn set(&self, bytes: u64) {
        let old = self.bytes.swap(bytes, Ordering::Relaxed);
        if old != bytes {
            // Modular: adds the difference, whichever way it goes
            self.memory.parts[self.part.index()].fetch_add(bytes.wrapping_sub(old), Ordering::Relaxed);
            self.memory.changed();
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn memory(&self) -> &Arc<Memory> {
        &self.memory
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.set(0);
    }
}

/// The memory limit of the process's cgroup, on Linux (ADR 0054): v2's
/// `memory.max`, else v1's `memory.limit_in_bytes`. `None` without a cgroup
/// limit, and on other systems.
pub fn cgroup_limit() -> Option<(u64, LimitSource)> {
    if cfg!(target_os = "linux") { cgroup_limit_with(|path| std::fs::read_to_string(path).ok()) } else { None }
}

/// [`cgroup_limit`] reading files through `read` (tests give it a map).
pub fn cgroup_limit_with(read: impl Fn(&str) -> Option<String>) -> Option<(u64, LimitSource)> {
    let own = read("/proc/self/cgroup").unwrap_or_default();
    // v2: the unified hierarchy, "0::<path>"
    let v2 = own.lines().find_map(|line| line.strip_prefix("0::"));
    let mut v2_files = Vec::new();
    if let Some(path) = v2.map(str::trim).filter(|p| *p != "/") {
        v2_files.push(format!("/sys/fs/cgroup{}/memory.max", path));
    }
    v2_files.push("/sys/fs/cgroup/memory.max".to_owned());
    for file in v2_files {
        if let Some(text) = read(&file) {
            return match text.trim() {
                "max" => None,
                value => value.parse().ok().filter(|&b| b > 0).map(|b| (b, LimitSource::CgroupV2)),
            };
        }
    }
    // v1: "<id>:<controllers>:<path>", one of them "memory"
    let v1 = own.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let (_, controllers, path) = (fields.next()?, fields.next()?, fields.next()?);
        controllers.split(',').any(|c| c == "memory").then_some(path.trim())
    });
    let mut v1_files = Vec::new();
    if let Some(path) = v1.filter(|p| *p != "/") {
        v1_files.push(format!("/sys/fs/cgroup/memory{}/memory.limit_in_bytes", path));
    }
    v1_files.push("/sys/fs/cgroup/memory/memory.limit_in_bytes".to_owned());
    let text = v1_files.iter().find_map(|file| read(file))?;
    text.trim().parse::<u64>().ok().filter(|&b| b > 0 && b < V1_UNLIMITED).map(|b| (b, LimitSource::CgroupV1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn limited(bytes: u64) -> Arc<Memory> {
        Memory::new(&MemoryOptions { limit_bytes: Some(bytes), ..MemoryOptions::default() })
    }

    #[test]
    fn charges_add_up_per_part_and_are_released_when_dropped() {
        let memory = Memory::unlimited();
        let a = memory.charge(Part::Graph);
        let b = memory.charge(Part::Graph);
        let c = memory.charge(Part::Working);
        a.set(100);
        b.set(50);
        c.set(7);
        assert_eq!((memory.part(Part::Graph), memory.part(Part::Working), memory.used()), (150, 7, 157));
        a.set(30);
        assert_eq!(memory.used(), 87);
        drop(b);
        drop(c);
        assert_eq!(memory.used(), 30);
        assert_eq!(memory.state(), MemoryState::Normal);
        assert!(memory.check_write().is_ok());
    }

    #[test]
    fn the_state_rises_at_each_line_and_falls_only_below_its_band() {
        // Lines at 800 (warn) and 900 (refuse), band 50
        let memory = limited(1000);
        let c = memory.charge(Part::Payload);
        let at = |bytes: u64| {
            c.set(bytes);
            memory.state()
        };
        use MemoryState::*;
        assert_eq!(at(799), Normal);
        assert_eq!(at(800), Warn);
        assert_eq!(at(760), Warn, "inside the warning band");
        assert_eq!(at(749), Normal);
        assert_eq!(at(900), RefusingWrites, "straight past both lines");
        assert_eq!(at(851), RefusingWrites, "inside the refusal band");
        assert_eq!(at(899), RefusingWrites);
        assert_eq!(at(849), Warn);
        assert_eq!(at(880), Warn, "below the refusal line again: rising needs 900");
        assert_eq!(at(950), RefusingWrites);
        assert_eq!(at(10), Normal, "falling past both bands at once");
        assert_eq!(at(1500), RefusingWrites);
    }

    #[test]
    fn refusing_writes_fails_check_write_with_the_numbers() {
        let memory = limited(1000);
        let c = memory.charge(Part::Graph);
        c.set(950);
        assert!(matches!(memory.check_write(), Err(Error::MemoryLimit { used: 950, refuse_at: 900, limit: 1000 })));
        c.set(840);
        assert!(memory.check_write().is_ok());
    }

    #[test]
    fn options_are_checked() {
        let with = |w, r| MemoryOptions { warn_at: w, refuse_writes_at: r, ..MemoryOptions::default() }.check();
        assert!(with(0.8, 0.9).is_ok());
        assert!(with(0.9, 0.9).is_ok());
        assert!(with(0.9, 0.8).is_err());
        assert!(with(0.05, 0.9).is_err());
        assert!(with(0.8, 1.1).is_err());
        assert!(with(f64::NAN, 0.9).is_err());
        assert!(MemoryOptions { limit_bytes: Some(0), ..MemoryOptions::default() }.check().is_err());
    }

    fn files(entries: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = entries.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |path| map.get(path).cloned()
    }

    #[test]
    fn the_cgroup_limit_is_read_from_v2_then_v1() {
        let v2 = files(&[
            ("/proc/self/cgroup", "0::/system.slice/iwdb.service\n"),
            ("/sys/fs/cgroup/system.slice/iwdb.service/memory.max", "2147483648\n"),
        ]);
        assert_eq!(cgroup_limit_with(v2), Some((2 << 30, LimitSource::CgroupV2)));
        // In a container the path is "/", and the file is at the root
        let root = files(&[("/proc/self/cgroup", "0::/\n"), ("/sys/fs/cgroup/memory.max", "536870912")]);
        assert_eq!(cgroup_limit_with(root), Some((512 << 20, LimitSource::CgroupV2)));
        let unlimited = files(&[("/proc/self/cgroup", "0::/\n"), ("/sys/fs/cgroup/memory.max", "max\n")]);
        assert_eq!(cgroup_limit_with(unlimited), None);

        let v1 = files(&[
            ("/proc/self/cgroup", "12:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n"),
            ("/sys/fs/cgroup/memory/docker/abc/memory.limit_in_bytes", "1073741824\n"),
        ]);
        assert_eq!(cgroup_limit_with(v1), Some((1 << 30, LimitSource::CgroupV1)));
        let v1_unlimited = files(&[
            ("/proc/self/cgroup", "4:memory:/\n"),
            ("/sys/fs/cgroup/memory/memory.limit_in_bytes", "9223372036854771712\n"),
        ]);
        assert_eq!(cgroup_limit_with(v1_unlimited), None);
        assert_eq!(cgroup_limit_with(files(&[])), None);
    }
}
