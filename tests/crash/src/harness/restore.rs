//! Backups, the archive and restores, checked in children.

use std::fs::{self};
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwdb::{Error, RestoreSources, RestoreTarget, Store, verify};
use iwdb_storage::failpoint::{Action, Rule};
use iwdb_storage::layout::{MARKER_NAME, RESTORING_NAME};

use super::plan::restore_points;
use super::{CHILD_TIMEOUT, ChildProcess, Config, Outcome, Summary, Target, open_existing};
use crate::child::RestoreArgs;
use crate::model::{self};
use crate::rng::Rng;
use crate::script::Policy;

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
            return Err(format!("the WAL starts at {} but the archive holds nothing", wal_first));
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
pub(super) fn restore_cycle(
    config: &Config,
    rng: &mut Rng,
    target: &Target,
    summary: &mut Summary,
) -> Result<(), String> {
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
