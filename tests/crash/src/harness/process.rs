//! A spawned child and what it reported.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use iwdb::IdempotencyKey;

use crate::child::ChildArgs;

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

    fn words(&self, prefix: &str) -> impl Iterator<Item = Vec<&str>> + use<'_> {
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
