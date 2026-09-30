//! The child: opens the store through a [`FailFs`] with the parent's
//! rules, runs its script, and reports on stdout, one line per event, each
//! written and flushed after the event happened:
//!
//! | Line | When |
//! |---|---|
//! | `open <seq> <synced_seq> <digest>` | `Store::open` returned (recovery is done) |
//! | `ack <seq> <synced_seq>` | a commit returned `Ok` (acknowledged), with `Store::synced_seq` after it |
//! | `paused <rule> <path>` | a pause rule fired at a call on `path`; the thread then blocks until the parent kills the child |
//! | `done` | the script ended; the child then waits to be killed |
//! | `error <message>` | something failed that shouldn't have; the child exits with status 2 |
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

use iwdb::{Error, Store};
use iwdb_engine::testutil::workload::Step;
use iwdb_storage::failpoint::{Call, FailFs, Rule};

use crate::model::digest;
use crate::script::{child_options, Act, Policy, Script};

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
                "--rule" => parsed.rules.push(value.parse()?),
                other => return Err(format!("unknown child option '{}'", other)),
            }
        }
        Ok(parsed)
    }
}

/// Write a protocol line and flush it.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    // If the parent is gone there is no one to tell
    let _ = writeln!(out, "{}", line).and_then(|()| out.flush());
}

fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    say(&format!("error {}: {}", what, error));
    eprintln!("iwdb-crash child: {}: {}", what, error);
    std::process::exit(2)
}

/// Block this thread until the process is killed.
fn wait_for_kill() -> ! {
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

/// The child's main: never returns.
pub fn main(args: &ChildArgs) -> ! {
    let fs = FailFs::new();
    for rule in &args.rules {
        fs.add(rule.clone());
    }
    fs.set_pause(Some(Arc::new(|rule: &Rule, path: &Path| {
        say(&format!("paused {} {}", rule, path.display()));
        wait_for_kill()
    })));
    match sync_log_hook(&args.work) {
        Ok(hook) => fs.set_hook(Some(hook)),
        Err(e) => fail("open the sync log", e),
    }

    let mut options = child_options(args.policy, args.keep);
    options.checkpoint.background = args.background;
    let store = match Store::open_with(fs, &args.dir, options) {
        Ok(store) => store,
        Err(e) => fail("open", e),
    };
    say(&format!("open {} {} {:016x}", store.seq(), store.synced_seq(), store.read(digest)));

    for act in Script::new(args.seed).take(args.acts) {
        match act {
            Act::Commit(step) => {
                let result = match step {
                    Step::Tx(mutations) => store.commit(&mutations),
                    Step::Catalog(change) => store.commit_catalog(change),
                };
                match result {
                    Ok(result) => say(&format!("ack {} {}", result.seq, store.synced_seq())),
                    // Invalid or conflicting: the model rejects it too
                    Err(Error::Engine(_)) => {}
                    Err(e) => fail("commit", e),
                }
            }
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
            Act::Sleep(duration) => std::thread::sleep(duration),
        }
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
            rules: vec![Rule::new(Call::RemoveFile, When::After, Action::Pause).skip(2).path("/wal/")],
        };
        assert_eq!(ChildArgs::parse(&args.to_args()), Ok(args));
        assert!(ChildArgs::parse(&["--seed".into()]).is_err());
    }
}
