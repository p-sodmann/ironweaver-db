//! The crash points step 5 left for step 6, each hit exactly: a child
//! pauses at the failpoint, the parent kills it with SIGKILL, then checks
//! what is on disk, recovers, and compares with the model (and, where the
//! point leaves work unfinished, that the next open or checkpoint finishes
//! it). Every point runs under each fsync policy.
//!
//! - during an append (a torn frame);
//! - inside a checkpoint's `write_atomic`: halfway, and with the temporary
//!   file complete but not renamed (for a process crash the same state as
//!   between its fsync and its rename);
//! - between a checkpoint's rename and its directory sync;
//! - between removing old checkpoints and removing WAL segments, and in
//!   the middle of removing segments;
//! - between a segment's rename and its directory sync (a rotation);
//! - during recovery's truncation of a torn tail, before and after it;
//! - during initialization, before the marker is written and after it;
//! - an abort and a panic in the commit path;
//! - (step 7) during an online backup, at its manifest and its marker;
//!   during archiving, before and after a segment is durable in the
//!   archive; and during a restore, at each of its writes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use iwdb::Store;
use iwdb_crash::harness::newest_checkpoint;
use iwdb_crash::{
    check, check_archive, check_backups, crash, restore_in_child, Bounds, Crashed, Outcome, Plan, Policy, RestorePlan,
    Restored, Summary, Target,
};
use iwdb_engine::testutil::workload::{pad, Step};
use iwdb_storage::failpoint::{Action, Call, Rule, When};
use iwdb_storage::layout::{MARKER_NAME, RESTORING_NAME};
use tempfile::TempDir;

const ACTS: usize = 120;

struct Env {
    work: TempDir,
    target: Target,
    policy: Policy,
    seed: u64,
}

impl Env {
    fn new(policy: Policy, keep: usize) -> Env {
        let work = tempfile::tempdir().unwrap();
        let target = Target::new(work.path().join("data"), keep);
        Env { work, target, policy, seed: 100 }
    }

    /// With a WAL archive, and a directory for the script's backups.
    fn with_archive(policy: Policy, keep: usize) -> Env {
        let mut env = Env::new(policy, keep);
        env.target.archive = Some(env.work.path().join("archive-1"));
        env.target.backups = Some(env.work.path().join("backups-1"));
        env
    }

    fn exe() -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_iwdb-crash"))
    }

    /// Kill a child where `rule` pauses it, trying new children (new
    /// scripts) until one reaches it: backups are rare in a script.
    fn crash_when_reached(&mut self, rule: Rule) -> Crashed {
        for _ in 0..30 {
            self.seed += 1;
            let plan = Plan::At(rule.clone());
            let crashed = crash(&Self::exe(), self.work.path(), self.policy, ACTS, &mut self.target, self.seed, &plan)
                .unwrap_or_else(|e| panic!("{}: {}", self.policy, e));
            if crashed.reached {
                return crashed;
            }
            self.check();
        }
        panic!("{} {}: no child reached the point", self.policy, rule)
    }

    fn dir(&self) -> &Path {
        &self.target.path
    }

    fn run(&mut self, plan: Plan) -> Outcome {
        self.seed += 1;
        let exe = PathBuf::from(env!("CARGO_BIN_EXE_iwdb-crash"));
        let crashed = crash(&exe, self.work.path(), self.policy, ACTS, &mut self.target, self.seed, &plan)
            .unwrap_or_else(|e| panic!("{}: {}", self.policy, e));
        assert!(crashed.reached, "{}: the child didn't reach the point: {}", self.policy, crashed.context);
        crashed.outcome
    }

    /// Kill the child where `rule` pauses it; returns the path it paused on.
    fn crash_at(&mut self, rule: Rule) -> PathBuf {
        let outcome = self.run(Plan::At(rule));
        let paused = outcome.paused().expect("paused");
        PathBuf::from(paused.split_once(' ').expect("a rule and a path").1)
    }

    /// History: a child runs its whole script; then recovery, and a
    /// checkpoint that finishes whatever the kill interrupted, so that the
    /// directory holds no temporary file and only kept checkpoints.
    fn setup(&mut self) {
        self.run(Plan::Finish);
        let store = self.check();
        finish_cleanup(&store, &mut self.target);
    }

    /// Recover now and check against the model.
    fn check(&mut self) -> Store {
        check(&mut self.target, self.policy).unwrap_or_else(|e| panic!("{}: {}", self.policy, e))
    }
}

/// A commit (in the store and the model) and a checkpoint, which then has
/// something new to write and removes what an interrupted checkpoint left.
/// (A checkpoint run with nothing new to write removes nothing.)
fn finish_cleanup(store: &Store, target: &mut Target) {
    let step = pad(0);
    let Step::Tx(mutations) = &step else { unreachable!() };
    let seq = store.commit(mutations).unwrap().seq;
    assert_eq!(target.model.commit(&step).unwrap(), Some(seq));
    target.bounds = Bounds::exact(seq);
    store.checkpoint().unwrap();
}

fn pause(call: Call, when: When) -> Rule {
    Rule::new(call, when, Action::Pause)
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

fn segments(dir: &Path) -> Vec<u64> {
    iwdb_storage::list_segments(&dir.join("ns/00000000000000000001/wal")).unwrap().into_iter().map(|(s, _)| s).collect()
}

fn checkpoints(dir: &Path) -> Vec<u64> {
    iwdb_storage::checkpoint::list_checkpoints(&dir.join("ns/00000000000000000001/checkpoints"))
        .unwrap()
        .into_iter()
        .map(|(s, _)| s)
        .collect()
}

#[test]
fn during_an_append() {
    for policy in Policy::ALL {
        let mut env = Env::new(policy, 2);
        // A frame (not a segment header, whose file ends in .tmp)
        let mut skip = 20;
        let path = loop {
            let path = env.crash_at(pause(Call::Write, When::Midway).path("/wal/").skip(skip));
            if path.extension().is_some_and(|e| e == "wal") {
                break path;
            }
            env.check();
            skip += 1;
        };
        let store = env.check();
        let tail = store.recovery().torn_tail.clone().expect("a torn tail");
        assert_eq!(tail.path, path, "{}", policy);
        assert!(tail.valid_len < tail.file_len);
    }
}

#[test]
fn inside_a_checkpoint_write() {
    for policy in Policy::ALL {
        for when in [When::Midway, When::WriterDone] {
            let mut env = Env::new(policy, 2);
            env.setup();
            let before = newest_checkpoint(env.dir());
            env.crash_at(pause(Call::WriteAtomic, when).path("/checkpoints/"));
            // The temporary file is there, the checkpoint isn't
            let temp: Vec<String> = files(&env.dir().join("ns/00000000000000000001/checkpoints"))
                .into_iter()
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert_eq!(temp.len(), 1, "{} {:?}: {:?}", policy, when, temp);
            let temp_len =
                fs::metadata(env.dir().join("ns/00000000000000000001/checkpoints").join(&temp[0])).unwrap().len();
            assert!(temp_len > 0, "{} {:?}: flushed into the temporary file", policy, when);
            let store = env.check();
            // (The main thread goes on committing until the kill, which can
            // land in a rotation: a segment's temporary file too)
            let removed: Vec<_> = store
                .recovery()
                .removed_temp_files
                .iter()
                .filter(|p| p.to_string_lossy().contains("/checkpoints/"))
                .collect();
            assert_eq!(removed.len(), 1, "{} {:?}: {:?}", policy, when, store.recovery().removed_temp_files);
            assert_eq!(store.recovery().checkpoint, before, "{} {:?}", policy, when);
        }
    }
}

#[test]
fn between_a_checkpoints_rename_and_its_directory_sync() {
    for policy in Policy::ALL {
        for rule in [
            pause(Call::WriteAtomic, When::After).path("/checkpoints/"),
            pause(Call::SyncDir, When::Before).path("/checkpoints"),
        ] {
            let mut env = Env::new(policy, 2);
            env.setup();
            let before = checkpoints(env.dir());
            let segments_before = segments(env.dir());
            env.crash_at(rule.clone());
            let after = checkpoints(env.dir());
            assert_eq!(after.len(), before.len() + 1, "{} {}: the new checkpoint is in place", policy, rule);
            assert!(segments(env.dir()).starts_with(&segments_before[..1]), "{} {}: no segment removed", policy, rule);
            let store = env.check();
            // It is valid, and recovery starts from it
            assert_eq!(store.recovery().checkpoint, after.last().copied(), "{} {}", policy, rule);
        }
    }
}

#[test]
fn between_removing_old_checkpoints_and_removing_wal_segments() {
    for policy in Policy::ALL {
        let mut env = Env::new(policy, 1);
        env.setup();
        assert_eq!(checkpoints(env.dir()).len(), 1);
        // The next checkpoint removes the old one, syncs, then pauses
        // before its first segment removal
        env.crash_at(pause(Call::RemoveFile, When::Before).path("/wal/"));
        let kept = checkpoints(env.dir());
        assert_eq!(kept.len(), 1, "{}: the old checkpoint is gone", policy);
        assert!(covered(env.dir()) > 0, "{}: segments the checkpoint covers are still there", policy);
        let store = env.check();
        assert_eq!(store.recovery().checkpoint, Some(kept[0]));
        // The next checkpoint cuts them
        finish_cleanup(&store, &mut env.target);
        drop(store);
        assert_eq!(covered(env.dir()), 0, "{}", policy);
    }
}

/// Segments the newest checkpoint covers: all their records are at or
/// below it (a segment ends where the next one begins).
fn covered(dir: &Path) -> usize {
    let Some(newest) = newest_checkpoint(dir) else { return 0 };
    segments(dir).windows(2).filter(|w| w[1] <= newest + 1).count()
}

#[test]
fn in_the_middle_of_removing_segments() {
    for policy in Policy::ALL {
        let mut env = Env::new(policy, 1);
        env.setup();
        // Only the script's checkpoints, each after several segments (the
        // background interval could come after every commit or two)
        env.target.background = false;
        // Pause after a removal that leaves more of the same checkpoint's
        // segments to remove (a new child, with a new script, each time)
        let mut tries = 0;
        loop {
            env.crash_at(pause(Call::RemoveFile, When::After).path("/wal/"));
            if covered(env.dir()) > 0 {
                break;
            }
            let store = env.check();
            finish_cleanup(&store, &mut env.target);
            tries += 1;
            assert!(tries < 20, "{}: no checkpoint removed two segments", policy);
        }
        let newest = newest_checkpoint(env.dir());
        // What is left of the log after the checkpoint is complete, and
        // recovery checks that
        let store = env.check();
        assert_eq!(store.recovery().checkpoint, newest, "{}", policy);
        finish_cleanup(&store, &mut env.target);
        drop(store);
        assert_eq!(covered(env.dir()), 0, "{}: the next checkpoint finishes the removal", policy);
    }
}

#[test]
fn between_a_rotations_rename_and_its_directory_sync() {
    for policy in Policy::ALL {
        let mut env = Env::new(policy, 2);
        let path = env.crash_at(pause(Call::Rename, When::After).path("/wal/").skip(3));
        // The new segment has only its header
        assert_eq!(fs::metadata(&path).unwrap().len(), iwdb_storage::format::SEGMENT_HEADER_LEN as u64);
        env.check();
    }
}

#[test]
fn during_recoverys_truncation() {
    for policy in Policy::ALL {
        let mut env = Env::new(policy, 2);
        let torn = loop {
            let path = env.crash_at(pause(Call::Write, When::Midway).path("/wal/").skip(10));
            if path.extension().is_some_and(|e| e == "wal") {
                break path;
            }
            env.check();
        };
        let torn_len = fs::metadata(&torn).unwrap().len();

        // Killed before the cut: the tail is still there
        env.crash_at(pause(Call::Truncate, When::Before));
        assert_eq!(fs::metadata(&torn).unwrap().len(), torn_len, "{}", policy);
        // Killed after the cut (and its fsync), before the writer started
        env.crash_at(pause(Call::Truncate, When::After));
        assert!(fs::metadata(&torn).unwrap().len() < torn_len, "{}", policy);
        // The next open finds a clean end
        let store = env.check();
        assert!(store.recovery().torn_tail.is_none(), "{}", policy);
    }
}

#[test]
fn during_initialization() {
    for policy in Policy::ALL {
        for (rule, marker) in [
            (pause(Call::SyncDir, When::Before), false),
            (pause(Call::WriteAtomic, When::Before).path(MARKER_NAME), false),
            (pause(Call::WriteAtomic, When::WriterDone).path(MARKER_NAME), false),
            (pause(Call::WriteAtomic, When::After).path(MARKER_NAME), true),
        ] {
            let mut env = Env::new(policy, 2);
            env.crash_at(rule.clone());
            assert_eq!(env.dir().join(MARKER_NAME).exists(), marker, "{} {}", policy, rule);
            let store = env.check();
            assert_eq!(store.recovery().created, !marker, "{} {}: the next open finishes it", policy, rule);
            assert_eq!(store.seq(), 0);
            drop(store);
            env.run(Plan::Finish);
            env.check();
        }
    }
}

#[test]
fn an_abort_or_a_panic_in_the_commit_path() {
    for policy in Policy::ALL {
        for rule in [
            Rule::new(Call::Write, When::Before, Action::Abort).path("/wal/").skip(30),
            Rule::new(Call::Write, When::Midway, Action::Panic).path("/wal/").skip(30),
            Rule::new(Call::Sync, When::Before, Action::Panic).path("/wal/").skip(5),
        ] {
            let mut env = Env::new(policy, 2);
            // (With off, fsyncs come only from the script's syncs and
            // checkpoints)
            let outcome = env.run(Plan::At(rule.clone()));
            assert!(!outcome.killed && outcome.aborted(), "{} {}: {:?}", policy, rule, outcome.status);
            env.check();
        }
    }
}

/// Kills during an online backup: at its first file, halfway through a
/// copy, at its manifest (written but not renamed, renamed) and at its
/// marker (renamed: complete). Whatever is left is refused by verify and
/// restore, or complete and equal to the model at its seq.
#[test]
fn during_a_backup() {
    // One policy: a backup does the same under each (it syncs first),
    // and the random runs cover all three
    for policy in [Policy::Group] {
        for (rule, complete) in [
            (pause(Call::Create, When::After).path("/backups-"), false),
            (pause(Call::Write, When::Midway).path("/backups-").skip(1), false),
            (pause(Call::WriteAtomic, When::WriterDone).path("/backups-"), false),
            (pause(Call::WriteAtomic, When::After).path("/backups-"), false),
            (pause(Call::WriteAtomic, When::After).path("/backups-").skip(1), true),
        ] {
            let mut env = Env::with_archive(policy, 2);
            env.setup();
            let crashed = env.crash_when_reached(rule.clone());
            let paused = PathBuf::from(crashed.outcome.paused().unwrap().split_once(' ').unwrap().1);
            let backup =
                paused.ancestors().find(|p| p.parent() == env.target.backups.as_deref()).unwrap().to_path_buf();
            assert_eq!(backup.join(MARKER_NAME).exists(), complete, "{} {}", policy, rule);
            env.check();
            let mut summary = Summary::default();
            check_backups(env.work.path(), &env.target, &crashed.outcome, &mut summary)
                .unwrap_or_else(|e| panic!("{} {}: {}", policy, rule, e));
            if complete {
                assert_eq!(summary.backups_interrupted, 0, "{} {}", policy, rule);
            } else {
                assert!(summary.backups_interrupted >= 1, "{} {}: {:?}", policy, rule, summary);
            }
        }
    }
}

/// Kills during archiving: halfway through a segment's copy, after its
/// rename (before the archive directory is synced), and after that sync
/// (before the segment leaves the WAL). Recovery then finds the archive
/// valid and no removed segment missing, and the next checkpoint archives
/// again what both hold and removes it.
#[test]
fn during_archiving() {
    // One policy: archiving does the same under each, and the random runs
    // cover all three
    for policy in [Policy::Always] {
        for rule in [
            pause(Call::Write, When::Midway).path("/archive-"),
            pause(Call::Rename, When::After).path("/archive-"),
            pause(Call::SyncDir, When::After).path("/archive-"),
        ] {
            let mut env = Env::with_archive(policy, 1);
            env.setup();
            env.crash_when_reached(rule.clone());
            let store = env.check();
            let mut summary = Summary::default();
            check_archive(&env.target, &mut summary).unwrap_or_else(|e| panic!("{} {}: {}", policy, rule, e));
            finish_cleanup(&store, &mut env.target);
            drop(store);
            check_archive(&env.target, &mut summary).unwrap_or_else(|e| panic!("{} {}: {}", policy, rule, e));
            // Everything removed from the WAL is in the archive, from seq 1
            let n = env.target.model.seq();
            let dest = env.work.path().join("restored");
            let (restored, _) =
                restore_in_child(&Env::exe(), env.work.path(), &env.target, n, true, &RestorePlan::Finish, &dest)
                    .unwrap();
            assert_eq!(restored, Restored::Complete);
        }
    }
}

/// Kills during a restore, at each of its writes: what is left is refused
/// (an interrupted restore), or complete once its marker is in place. A
/// restore that finishes equals the model at its seq.
#[test]
fn during_a_restore() {
    let mut env = Env::with_archive(Policy::Always, 1);
    env.setup();
    let n = env.target.model.seq() - 3;
    let dest = env.work.path().join("restored");
    for (rule, complete) in [
        (pause(Call::Create, When::After).path(RESTORING_NAME), false),
        (pause(Call::Sync, When::Before).path(RESTORING_NAME), false),
        (pause(Call::SyncDir, When::Before).path("/restored"), false),
        (pause(Call::WriteAtomic, When::Midway).path("/restored/checkpoints/"), false),
        (pause(Call::WriteAtomic, When::WriterDone).path("/restored/checkpoints/"), false),
        (pause(Call::WriteAtomic, When::After).path("/restored/checkpoints/"), false),
        (pause(Call::RemoveFile, When::Before).path(RESTORING_NAME), false),
        (pause(Call::RemoveFile, When::After).path(RESTORING_NAME), false),
        (pause(Call::WriteAtomic, When::Before).path("restored/IWDB"), false),
        (pause(Call::WriteAtomic, When::After).path("restored/IWDB"), true),
        (pause(Call::SyncDir, When::Before).path("/restored").skip(3), true),
        (Rule::new(Call::WriteAtomic, When::WriterDone, Action::Abort).path("/restored/checkpoints/"), false),
    ] {
        for from_data in [true, false] {
            let plan = RestorePlan::At(rule.clone());
            let (restored, outcome) =
                restore_in_child(&Env::exe(), env.work.path(), &env.target, n, from_data, &plan, &dest)
                    .unwrap_or_else(|e| panic!("{}: {}", rule, e));
            assert!(outcome.paused().is_some() || outcome.aborted(), "{}: {:?}", rule, outcome.lines);
            let expected = if complete { Restored::Complete } else { Restored::Interrupted };
            assert_eq!(restored, expected, "{} (from data: {})", rule, from_data);
        }
    }
    let (restored, _) =
        restore_in_child(&Env::exe(), env.work.path(), &env.target, n, true, &RestorePlan::Finish, &dest).unwrap();
    assert_eq!(restored, Restored::Complete);
}
