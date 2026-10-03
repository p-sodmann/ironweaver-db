//! Where and when the parent kills a child.

use std::time::Duration;

use iwdb_storage::failpoint::{Action, Call, Rule, When};
use iwdb_storage::layout::RESTORING_NAME;

use crate::rng::Rng;

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
pub(super) fn points() -> Vec<(Call, When, &'static str, u64)> {
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
pub(super) fn restore_points() -> Vec<(Call, When, &'static str, u64)> {
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
pub(super) fn init_points() -> Vec<Rule> {
    vec![
        Rule::new(Call::SyncDir, When::Before, Action::Pause),
        Rule::new(Call::WriteAtomic, When::Before, Action::Pause).path("IWDB"),
        Rule::new(Call::WriteAtomic, When::WriterDone, Action::Pause).path("IWDB"),
        Rule::new(Call::WriteAtomic, When::After, Action::Pause).path("IWDB"),
    ]
}

pub(super) fn choose_plan(rng: &mut Rng, fresh: bool, max_delay: Duration) -> Plan {
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
