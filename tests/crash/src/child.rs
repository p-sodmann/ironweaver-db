//! The child: opens the store through a [`FailFs`] with the parent's
//! rules, runs its script, and reports on stdout, one line per event, each
//! written and flushed after the event happened:
//!
//! | Line | When |
//! |---|---|
//! | `open <seq> <synced_seq> <digest>` | `Store::open` returned (recovery is done) |
//! | `try <key>` | a commit with this idempotency key starts |
//! | `ack <seq> <synced_seq>` | a commit returned `Ok` (acknowledged), with `Store::synced_seq` after it |
//! | `dedup <seq> <key>` | a keyed commit returned the original result of commit `seq`: it committed nothing |
//! | `backup <seq> <path>` | an online backup into `path` returned `Ok`, at `seq` |
//! | `paused <rule> <path>` | a pause rule fired at a call on `path`; the thread then blocks until the parent kills the child |
//! | `done` | the script ended; the child then waits to be killed |
//! | `error <message>` | something failed that shouldn't have; the child exits with status 2 |
//!
//! Before its script, a child retries the keyed commits `--retry <key>`
//! that the previous child tried last (it may have been killed during
//! them): each applies once, from the log or now.
//!
//! The restore child (`iwdb-crash restore ...`, [`RestoreArgs`]) runs one
//! restore through a [`FailFs`] and says `restored <seq>`, then `done`.
//!
//! Before every fsync of a file, the child appends `<path> <length>` to the
//! sync log (`<work>/synclog`). The length is where the file will be
//! durable once that fsync completes. The parent's OS-crash simulation
//! never cuts a file below it. It is written before the fsync, so it is an
//! upper bound of what was synced: the simulation never loses more than a
//! real OS crash could.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use iwdb::{CommitOptions, Error, IdempotencyKey, LogFs, RestoreSources, RestoreTarget, Store};
use iwdb_engine::testutil::workload::Step;
use iwdb_storage::failpoint::{Call, FailFs, Rule};

use crate::model::digest;
use crate::script::{Act, Policy, Script, child_options};

/// The name of the sync log in the work directory.
pub const SYNC_LOG: &str = "synclog";

/// What a child runs: its data directory, work directory (sync log), the
/// script's seed and length, the fsync policy, how many checkpoints to
/// keep, and failpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildArgs {
    pub dir: PathBuf,
    pub work: PathBuf,
    pub seed: u64,
    pub acts: usize,
    pub policy: Policy,
    pub keep: usize,
    /// Background checkpoints (by WAL size and time); without them only
    /// the script's checkpoints run.
    pub background: bool,
    /// The WAL archive, if the store archives.
    pub archive: Option<PathBuf>,
    /// Where the script's backups go (each into a new directory); without
    /// it the script's backups are skipped.
    pub backups: Option<PathBuf>,
    /// Keyed commits to retry first.
    pub retries: Vec<IdempotencyKey>,
    pub rules: Vec<Rule>,
}

impl ChildArgs {
    /// The command line after `child`.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--dir".into(),
            self.dir.display().to_string(),
            "--work".into(),
            self.work.display().to_string(),
            "--seed".into(),
            self.seed.to_string(),
            "--acts".into(),
            self.acts.to_string(),
            "--policy".into(),
            self.policy.to_string(),
            "--keep".into(),
            self.keep.to_string(),
            "--background".into(),
            self.background.to_string(),
        ];
        for (flag, path) in [("--archive", &self.archive), ("--backups", &self.backups)] {
            if let Some(path) = path {
                args.push(flag.into());
                args.push(path.display().to_string());
            }
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
        let mut parsed = ChildArgs {
            dir: PathBuf::new(),
            work: PathBuf::new(),
            seed: 0,
            acts: 0,
            policy: Policy::Always,
            keep: 2,
            background: true,
            archive: None,
            backups: None,
            retries: Vec::new(),
            rules: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{} needs a value", flag))?;
            let number = |v: &str| v.parse::<u64>().map_err(|e| format!("{} {}: {}", flag, v, e));
            match flag.as_str() {
                "--dir" => parsed.dir = value.into(),
                "--work" => parsed.work = value.into(),
                "--seed" => parsed.seed = number(value)?,
                "--acts" => parsed.acts = number(value)? as usize,
                "--policy" => parsed.policy = value.parse()?,
                "--keep" => parsed.keep = number(value)? as usize,
                "--background" => parsed.background = value == "true",
                "--archive" => parsed.archive = Some(value.into()),
                "--backups" => parsed.backups = Some(value.into()),
                "--retry" => parsed.retries.push(IdempotencyKey::new(value.as_str()).map_err(|e| e.to_string())?),
                "--rule" => parsed.rules.push(value.parse()?),
                other => return Err(format!("unknown child option '{}'", other)),
            }
        }
        Ok(parsed)
    }
}

/// Write a protocol line and flush it.
pub(crate) fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    // If the parent is gone there is no one to tell
    let _ = writeln!(out, "{}", line).and_then(|()| out.flush());
}

pub(crate) fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    say(&format!("error {}: {}", what, error));
    eprintln!("iwdb-crash child: {}: {}", what, error);
    std::process::exit(2)
}

/// Block this thread until the process is killed.
pub(crate) fn wait_for_kill() -> ! {
    loop {
        std::thread::park();
    }
}

/// Record, before every fsync, the length the file will be durable at.
fn sync_log_hook(work: &Path) -> Result<iwdb_storage::failpoint::Hook, std::io::Error> {
    let log = Mutex::new(OpenOptions::new().create(true).append(true).open(work.join(SYNC_LOG))?);
    Ok(Arc::new(move |call: Call, path: &Path| {
        if call != Call::Sync {
            return;
        }
        if let Ok(meta) = std::fs::metadata(path) {
            let mut log = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Unbuffered: in the page cache before the fsync starts
            let _ = writeln!(log, "{} {}", path.display(), meta.len());
        }
    }))
}

/// A [`FailFs`] with `rules`, whose pauses say `paused` and wait to be
/// killed.
pub(crate) fn fail_fs(rules: &[Rule]) -> FailFs {
    let fs = FailFs::new();
    for rule in rules {
        fs.add(rule.clone());
    }
    fs.set_pause(Some(Arc::new(|rule: &Rule, path: &Path| {
        say(&format!("paused {} {}", rule, path.display()));
        wait_for_kill()
    })));
    fs
}

/// The child's main: never returns.
pub fn main(args: &ChildArgs) -> ! {
    let fs = fail_fs(&args.rules);
    match sync_log_hook(&args.work) {
        Ok(hook) => fs.set_hook(Some(hook)),
        Err(e) => fail("open the sync log", e),
    }

    let mut options = child_options(args.policy, args.keep, args.archive.as_deref());
    options.checkpoint.background = args.background;
    let store = match Store::open_with(fs, &args.dir, options) {
        Ok(store) => store,
        Err(e) => fail("open", e),
    };
    say(&format!("open {} {} {:016x}", store.seq(), store.synced_seq(), store.read(digest)));

    for key in &args.retries {
        match Script::keyed(key) {
            Ok(step) => commit(&store, step, Some(key.clone())),
            Err(e) => fail("retry", e),
        }
    }
    let mut backups = 0;
    for act in Script::new(args.seed).take(args.acts) {
        match act {
            Act::Commit(step, key) => commit(&store, step, key),
            Act::Checkpoint => {
                if let Err(e) = store.checkpoint() {
                    fail("checkpoint", e);
                }
            }
            Act::Sync => {
                if let Err(e) = store.sync() {
                    fail("sync", e);
                }
            }
            Act::Backup => {
                let Some(dir) = &args.backups else { continue };
                let dest = dir.join(format!("{:016x}-{}", args.seed, backups));
                backups += 1;
                match store.backup(&dest) {
                    Ok(report) => say(&format!("backup {} {}", report.seq, dest.display())),
                    Err(e) => fail("backup", e),
                }
            }
            Act::Sleep(duration) => std::thread::sleep(duration),
        }
    }
    say("done");
    wait_for_kill()
}

/// Commit `step` (with its idempotency key) and say how it went.
fn commit<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, step: Step, key: Option<IdempotencyKey>)
where
    F::File: Send,
{
    if let Some(key) = &key {
        say(&format!("try {}", key.as_str()));
    }
    let options = CommitOptions { idempotency_key: key.clone() };
    let result = match step {
        Step::Tx(mutations) => store.commit_with(&mutations, &options),
        Step::Catalog(change) => store.commit_catalog_with(change, &options),
    };
    match (result, key) {
        (Ok(result), Some(key)) if result.deduplicated => say(&format!("dedup {} {}", result.seq, key.as_str())),
        (Ok(result), _) => say(&format!("ack {} {}", result.seq, store.synced_seq())),
        // Invalid or conflicting: the model rejects it too
        (Err(Error::Engine(_)), _) => {}
        (Err(e), _) => fail("commit", e),
    }
}

/// What the restore child runs: a restore into `dest` from a backup or
/// data directory and/or an archive, to `seq` (or the latest), with
/// failpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreArgs {
    pub backup: Option<PathBuf>,
    pub archive: Option<PathBuf>,
    pub dest: PathBuf,
    pub seq: Option<u64>,
    pub rules: Vec<Rule>,
}

impl RestoreArgs {
    /// The command line after `restore`.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec!["--dest".into(), self.dest.display().to_string()];
        for (flag, path) in [("--backup", &self.backup), ("--archive", &self.archive)] {
            if let Some(path) = path {
                args.push(flag.into());
                args.push(path.display().to_string());
            }
        }
        if let Some(seq) = self.seq {
            args.push("--seq".into());
            args.push(seq.to_string());
        }
        for rule in &self.rules {
            args.push("--rule".into());
            args.push(rule.to_string());
        }
        args
    }

    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed =
            RestoreArgs { backup: None, archive: None, dest: PathBuf::new(), seq: None, rules: Vec::new() };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{} needs a value", flag))?;
            match flag.as_str() {
                "--dest" => parsed.dest = value.into(),
                "--backup" => parsed.backup = Some(value.into()),
                "--archive" => parsed.archive = Some(value.into()),
                "--seq" => parsed.seq = Some(value.parse().map_err(|e| format!("--seq {}: {}", value, e))?),
                "--rule" => parsed.rules.push(value.parse()?),
                other => return Err(format!("unknown restore option '{}'", other)),
            }
        }
        Ok(parsed)
    }
}

/// The restore child's main: never returns.
pub fn restore_main(args: &RestoreArgs) -> ! {
    let fs = fail_fs(&args.rules);
    let sources = RestoreSources { backup: args.backup.clone(), archive: args.archive.clone() };
    let target = args.seq.map_or(RestoreTarget::Latest, RestoreTarget::Seq);
    match iwdb::restore_with(&fs, &args.dest, &sources, target) {
        Ok(report) => say(&format!("restored {}", report.seq)),
        Err(e) => fail("restore", e),
    }
    say("done");
    wait_for_kill()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwdb_storage::failpoint::{Action, When};

    #[test]
    fn arguments_round_trip() {
        let args = ChildArgs {
            dir: "/tmp/a b".into(),
            work: "/tmp/w".into(),
            seed: 42,
            acts: 100,
            policy: Policy::Group,
            keep: 3,
            background: false,
            archive: Some("/tmp/archive".into()),
            backups: None,
            retries: vec![IdempotencyKey::new("00000000000000aa-17").expect("key")],
            rules: vec![Rule::new(Call::RemoveFile, When::After, Action::Pause).skip(2).path("/wal/")],
        };
        assert_eq!(ChildArgs::parse(&args.to_args()), Ok(args));
        assert!(ChildArgs::parse(&["--seed".into()]).is_err());
        let restore = RestoreArgs {
            backup: None,
            archive: Some("/a".into()),
            dest: "/d e".into(),
            seq: Some(17),
            rules: vec![Rule::new(Call::Create, When::Before, Action::Abort).path("RESTORING")],
        };
        assert_eq!(RestoreArgs::parse(&restore.to_args()), Ok(restore));
    }
}
