//! A panic while the store changes its namespace or WAL is a crash (ADR
//! 0008): the process aborts, and the next open recovers every logged
//! commit. The test binary runs itself as a child that panics at a
//! failpoint in the commit path (a WAL write, halfway through one, an
//! fsync, the group commit timer's fsync); the parent checks that the child
//! aborted, reopens, and compares with the reference.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{FsyncPolicy, Namespace, Store, StoreOptions};
use support::{options, reference, state, store_state, workload, Step};

const CHILD: &str = "IWDB_PANIC_CHILD";

fn opts(group: bool) -> StoreOptions {
    let mut opts = options(2);
    if group {
        opts.wal.fsync = FsyncPolicy::Group { max_delay: Duration::from_millis(5), max_batch: 1000 };
        // No rotation, so only the timer fsyncs
        opts.wal.segment_size = iwdb_storage::DEFAULT_SEGMENT_SIZE;
    }
    opts
}

/// Run by the tests below in a child process: open the store with the
/// rule, commit the workload, print every acknowledged seq.
#[test]
fn child_panics_in_the_commit_path() {
    let Ok(args) = std::env::var(CHILD) else { return };
    let mut args = args.splitn(3, '|');
    let (dir, group, rule) = (args.next().unwrap(), args.next().unwrap() == "group", args.next().unwrap());
    let fs = TestFs::default();
    fs.add(rule.parse::<Rule>().unwrap());
    let store = Store::open_with(fs, dir.as_ref(), opts(group)).unwrap();
    let mut out = std::io::stdout();
    for step in workload(80, 40) {
        let result = match step {
            Step::Tx(m) => store.commit(&m),
            Step::Catalog(c) => store.commit_catalog(c),
        };
        if let Ok(result) = result {
            writeln!(out, "ack {}", result.seq).unwrap();
            out.flush().unwrap();
        }
        if group {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    println!("survived");
}

/// Run the child with `rule`; returns the last acknowledged seq.
fn run_child(dir: &std::path::Path, group: bool, rule: &Rule) -> u64 {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_panics_in_the_commit_path", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{}|{}|{}", dir.display(), if group { "group" } else { "always" }, rule))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut acked = 0;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        assert_ne!(line, "survived", "{}: the child didn't reach the failpoint", rule);
        if let Some(seq) = line.strip_prefix("ack ") {
            acked = seq.parse().unwrap();
        }
    }
    let status = child.wait().unwrap();
    assert!(!status.success(), "{}", rule);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "{}: the child aborted (SIGABRT), {:?}", rule, status);
    }
    acked
}

/// The reference after the workload's commits up to `seq`.
fn reference_at(seq: u64) -> Namespace {
    let mut reference = reference();
    for step in workload(80, 40) {
        if reference.seq() == seq {
            break;
        }
        let _ = match step {
            Step::Tx(m) => reference.commit(&m),
            Step::Catalog(c) => reference.commit_catalog(c),
        };
    }
    assert_eq!(reference.seq(), seq, "the workload has {} commits", seq);
    reference
}

/// Returns the acknowledged and the recovered seq.
fn panic_and_recover(group: bool, rule: Rule) -> (u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let acked = run_child(dir.path(), group, &rule);
    let store = Store::open(dir.path(), opts(group)).unwrap();
    let seq = store.seq();
    assert_eq!(store_state(&store), state(&reference_at(seq)), "{}", rule);
    assert!(store.read_only().is_none());
    (acked, seq)
}

#[test]
fn a_panic_before_a_wal_write_aborts_and_loses_nothing_acknowledged() {
    let (acked, seq) = panic_and_recover(false, Rule::new(Call::Write, When::Before, Action::Panic).skip(30));
    assert!(acked > 0);
    assert_eq!(seq, acked, "the commit in flight wasn't written");
}

#[test]
fn a_panic_halfway_through_a_wal_write_leaves_a_torn_tail() {
    let (acked, seq) =
        panic_and_recover(false, Rule::new(Call::Write, When::Midway, Action::Panic).skip(30).path("/wal/0"));
    assert_eq!(seq, acked);
}

#[test]
fn a_panic_in_a_wal_fsync_aborts_and_recovers_the_written_record() {
    let (acked, seq) = panic_and_recover(false, Rule::new(Call::Sync, When::Before, Action::Panic).skip(30));
    // The record was written before its fsync, so it may be in the log
    assert!(seq == acked || seq == acked + 1, "acked {}, recovered {}", acked, seq);
}

#[test]
fn a_panic_in_the_group_commit_timer_aborts_and_loses_nothing() {
    // The first fsyncs are the writer's own at open; skip past them
    let (acked, seq) = panic_and_recover(true, Rule::new(Call::Sync, When::Before, Action::Panic).skip(3));
    assert!(acked > 0);
    assert_eq!(seq, acked, "a process crash loses no group-committed commit");
}
