//! The crash and fault-injection harness (ADR 0007).
//!
//! One binary, `iwdb-crash`, is both sides. The **parent** runs cycles:
//!
//! 1. spawn itself as a **child** (`iwdb-crash child ...`) on a data
//!    directory, with a script seed and, per the cycle's plan, a failpoint
//!    ([`iwdb_storage::failpoint`]);
//! 2. the child opens the store (recovery), runs a random workload of
//!    commits, checkpoints (explicit and background, by WAL size and
//!    time), fsyncs and pauses, and reports every acknowledged commit;
//! 3. the parent kills it with SIGKILL: after a random delay, or when it
//!    pauses at the failpoint; or the child aborts or panics there itself;
//! 4. sometimes the parent simulates an OS crash on top ([`os_crash`]):
//!    it cuts or zeroes the unsynced end of the last WAL segment;
//! 5. the parent (or, sometimes, the next child) opens the store, and the
//!    recovered seq and state are checked against the reference [`Model`].
//!
//! **The protocol** is in [`child`]: `open`, `ack`, `paused`, `done` and
//! `error` lines on the child's stdout, written after the event, and a
//! sync log of the length every fsync makes durable.
//!
//! **What is checked** after every recovery, for the last acknowledged
//! seq `A` the child reported:
//!
//! - `always`: the recovered seq is `A` or `A + 1` (the commit in flight may
//!   be complete in the log, or acknowledged just before the kill and not
//!   reported), also after a simulated OS crash;
//! - `group` and `off`, process kill: the same, because the page cache
//!   survives;
//! - `group`, OS crash: at least the highest synced seq the child reported,
//!   at most `A + 1`;
//! - `off`, OS crash: at most `A + 1`. Recovery may refuse with
//!   `LogEndsBefore`, but only when the newest checkpoint is past the end
//!   of what is left of the log;
//! - in every case the canonical graph, catalog and seq equal the model's
//!   at the recovered seq: no partial transaction, nothing out of order.
//!
//! **Also checked:**
//!
//! - `verify` before every recovery the parent checks, on the directory as
//!   the crash left it: no problem when recovery succeeds, and the same
//!   seq; a problem when it refuses;
//! - a WAL archive for most data directories, checked after every checked
//!   recovery: it verifies, starts at seq 1, and reaches the WAL's first
//!   segment, so no segment the checkpointer removed was lost;
//! - online backups in the child's script, checked after the next checked
//!   recovery: complete ones verify and restore to the model's state at
//!   their seq; interrupted ones are refused by verify and restore;
//! - a restore child (`iwdb-crash restore`) in a quarter of the cycles: a
//!   restore to a random seq from the archive (and the data directory as a
//!   cold copy), killed at a random moment or at a restore failpoint; the
//!   result is refused or equals the model at that seq;
//! - failpoints in archiving and backups among the plans.
//!
//! The parent's choices come from its seed, and so does each child's
//! workload. Kill timing is real time, so a rerun with the same seed makes
//! the same plans but kills at slightly different moments.

pub mod catalog;
pub mod child;
pub mod harness;
pub mod model;
pub mod os_crash;
pub mod rng;
pub mod script;

pub use child::ChildArgs;
pub use harness::{
    Bounds, CheckError, ChildProcess, Config, Crashed, Failure, Outcome, Plan, RestorePlan, Restored, Summary, Target,
    check, check_archive, check_backups, check_recovery, crash, restore_in_child, run,
};
pub use model::Model;
pub use script::Policy;
