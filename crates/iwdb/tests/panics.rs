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
use support::{Step, options, reference, state, store_state, workload};

const CHILD: &str = "IWDB_PANIC_CHILD";

fn opts(group: bool) -> StoreOptions {
    let mut opts = options(2);
    if group {
        // A burst (below) must end within max_delay, or a commit pays the
        // fsync instead of the timer: 200 ms leaves room on slow runners
        // (40 ms didn't on a macOS one)
        opts.wal.fsync = FsyncPolicy::Group { max_delay: Duration::from_millis(200), max_batch: 1000 };
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
    for (i, step) in workload(80, 40).into_iter().enumerate() {
        // With group, bursts shorter than max_delay and pauses longer than
        // twice it (the timer wakes every max_delay): the timer does the
        // fsyncs, not the commits
        if group && i % 8 == 7 {
            std::thread::sleep(Duration::from_millis(600));
        }
        let result = match step {
            Step::Tx(m) => store.commit(&m),
            Step::Catalog(c) => store.commit_catalog(c),
        };
        if let Ok(result) = result {
            writeln!(out, "ack {}", result.seq).unwrap();
            out.flush().unwrap();
        }
    }
    println!("survived");
}

/// Run the child with `rule`; returns the last acknowledged seq and the
/// child's stderr.
fn run_child(dir: &std::path::Path, group: bool, rule: &Rule) -> (u64, String) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_panics_in_the_commit_path", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{}|{}|{}", dir.display(), if group { "group" } else { "always" }, rule))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
        text
    });
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
    (acked, stderr.join().unwrap())
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

/// Returns the acknowledged and the recovered seq, and the child's stderr.
fn panic_and_recover(group: bool, rule: Rule) -> (u64, u64, String) {
    let dir = tempfile::tempdir().unwrap();
    let (acked, stderr) = run_child(dir.path(), group, &rule);
    assert!(stderr.contains("injected panic") && stderr.contains("aborting the process"), "{}", stderr);
    let store = Store::open(dir.path(), opts(group)).unwrap();
    let seq = store.seq();
    assert_eq!(store_state(&store), state(&reference_at(seq)), "{}", rule);
    assert!(store.read_only().is_none());
    (acked, seq, stderr)
}

#[test]
fn a_panic_before_a_wal_write_aborts_and_loses_nothing_acknowledged() {
    let (acked, seq, _) = panic_and_recover(false, Rule::new(Call::Write, When::Before, Action::Panic).skip(30));
    assert!(acked > 0);
    assert_eq!(seq, acked, "the commit in flight wasn't written");
}

#[test]
fn a_panic_halfway_through_a_wal_write_leaves_a_torn_tail() {
    let (acked, seq, _) =
        panic_and_recover(false, Rule::new(Call::Write, When::Midway, Action::Panic).skip(30).path("/wal/0"));
    assert_eq!(seq, acked);
}

#[test]
fn a_panic_in_a_wal_fsync_aborts_and_recovers_the_written_record() {
    let (acked, seq, _) = panic_and_recover(false, Rule::new(Call::Sync, When::Before, Action::Panic).skip(30));
    // The record was written before its fsync, so it may be in the log
    assert!(seq == acked || seq == acked + 1, "acked {}, recovered {}", acked, seq);
}

#[test]
fn a_panic_in_the_group_commit_timer_aborts_and_loses_nothing() {
    // The first fsync is the new segment's, at open; the third timer fsync
    // panics
    let (acked, seq, stderr) = panic_and_recover(true, Rule::new(Call::Sync, When::Before, Action::Panic).skip(3));
    assert!(stderr.contains("'iwdb-sync'"), "the timer thread panicked: {}", stderr);
    assert!(acked > 0);
    // Every commit was written and acknowledged before the timer took the
    // lock; the last one may have been acknowledged but not yet reported
    assert!(seq == acked || seq == acked + 1, "acked {}, recovered {}", acked, seq);
}

/// A panic in a read closure changes nothing: the store stays writable
/// (in step 5 it poisoned the store's mutex, which made it read-only).
#[test]
fn a_panic_in_a_read_leaves_the_store_writable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let mut reference = reference();
    support::run(&store, &mut reference, &workload(10, 41));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store.read(|_| panic!("in a read"))));
    assert!(panicked.is_err());
    assert_eq!(store.read_only(), None);
    support::run(&store, &mut reference, &workload(10, 42)[1..]);
    store.close().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store_state(&store), state(&reference));
}
