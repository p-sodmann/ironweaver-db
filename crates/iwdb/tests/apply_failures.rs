//! Apply failures after the record is in the WAL (ADR 0028). An apply that
//! fails with `GraphError::Internal` may leave part of the transaction in
//! the graph, so the store aborts the process as it does on a panic
//! (ADR 0008); recovery restores every logged commit. Any other apply error
//! was rolled back by the core: the namespace becomes read-only and stays
//! readable. The failpoint (`iwdb_engine::failpoint`) stands in for a bug
//! the tests can't reach otherwise.
//!
//! A test binary of its own: its child process would load the machine
//! while `panics.rs` measures group commit timing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use iwdb::{Namespace, Store};
use support::{Step, options, reference, state, store_state, workload};

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

const INTERNAL_CHILD: &str = "IWDB_INTERNAL_APPLY_CHILD";

/// Run by the test below in a child process: commit the workload, and make
/// the 31st data commit's apply fail with `GraphError::Internal` after the
/// graph applied its ops (ADR 0028). Prints every acknowledged seq.
#[test]
fn child_hits_an_internal_apply_error() {
    let Ok(dir) = std::env::var(INTERNAL_CHILD) else { return };
    let store = Store::open(dir.as_ref(), options(2)).unwrap();
    let mut out = std::io::stdout();
    let mut data = 0;
    for step in workload(80, 40) {
        let result = match step {
            Step::Tx(m) => {
                data += 1;
                if data == 31 {
                    iwdb_engine::failpoint::fail_next_apply(ironweaver_core::GraphError::Internal("injected".into()));
                }
                store.commit(&m)
            }
            Step::Catalog(c) => store.commit_catalog(c),
        };
        if let Ok(result) = result {
            writeln!(out, "ack {}", result.seq).unwrap();
            out.flush().unwrap();
        }
    }
    println!("survived");
}

/// An apply that fails with `GraphError::Internal` may leave part of the
/// transaction in the graph: like a panic, it aborts, and recovery restores
/// every logged commit, the one in flight included (its record was written
/// before the apply).
#[test]
fn an_internal_apply_error_aborts_and_recovery_restores_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_hits_an_internal_apply_error", "--nocapture", "--test-threads=1"])
        .env(INTERNAL_CHILD, dir.path())
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
        assert_ne!(line, "survived", "the child kept running after the internal error");
        if let Some(seq) = line.strip_prefix("ack ") {
            acked = seq.parse().unwrap();
        }
    }
    let status = child.wait().unwrap();
    let stderr = stderr.join().unwrap();
    assert!(!status.success());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "the child aborted (SIGABRT), {:?}", status);
    }
    assert!(stderr.contains("failed inside the core (injected)") && stderr.contains("aborting"), "{}", stderr);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert!(acked > 0);
    assert_eq!(store.seq(), acked + 1, "the commit in flight is in the log");
    assert_eq!(store_state(&store), state(&reference_at(acked + 1)));
    assert!(store.read_only().is_none());
}

/// An apply that fails otherwise was rolled back by the core: the
/// namespace becomes read-only, readers go on seeing the state before it,
/// and the process lives (ADR 0028).
#[test]
fn a_cleanly_rolled_back_apply_error_makes_the_namespace_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let mut reference = reference();
    support::run(&store, &mut reference, &workload(10, 41));
    let before = store_state(&store);
    iwdb_engine::failpoint::fail_next_apply(ironweaver_core::GraphError::NodeNotFound("x".into()));
    let node = iwdb::Mutation::UpsertNode {
        id: "late".into(),
        labels: vec![],
        attr: Default::default(),
        meta: Default::default(),
        expected_version: None,
    };
    let e = store.commit(std::slice::from_ref(&node)).unwrap_err();
    assert!(e.to_string().contains("applying commit"), "{}", e);
    assert!(store.read_only().is_some());
    assert_eq!(store_state(&store), before, "readers see the state before the failed commit");
    assert!(store.commit(&[node]).is_err());
}
