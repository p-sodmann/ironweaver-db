//! Crash points of namespace operations (ADR 0017), each hit
//! exactly by a pause: a namespace's directory created and its create
//! event written, a drop's archive copy, its event and its directory
//! removal, each write of the namespace log, and the upgrade of a layout 3
//! directory to layout 4. After each kill the store recovers to a state the
//! acknowledged acts (or the one in flight) produce, and every keyed act
//! retried applies once.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwdb::Store;
use iwdb_crash::catalog::{self, CatalogArgs};
use iwdb_crash::{ChildProcess, Plan, Policy};
use iwdb_storage::failpoint::{Action, Call, Rule, When};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_iwdb-crash"))
}

fn pause(call: Call, when: When, path: &str, skip: u64) -> Plan {
    Plan::At(Rule::new(call, when, Action::Pause).path(path).skip(skip))
}

/// Each plan, several times over, under each policy: the scenario checks
/// every cycle.
#[test]
fn creating_and_dropping_namespaces() {
    for policy in Policy::ALL {
        // `always` hits each point at a later skip too: the others are kept short
        let skips: &[u64] = if policy == Policy::Always { &[0, 2] } else { &[0] };
        creating_and_dropping_under(policy, skips);
    }
}

fn creating_and_dropping_under(policy: Policy, skips: &[u64]) {
    let mut plans = Vec::new();
    for &skip in skips {
        for when in [When::Before, When::After] {
            plans.push(pause(Call::CreateDir, when, "/ns/", skip));
            plans.push(pause(Call::RemoveDirAll, when, "/ns/", skip.min(2)));
            plans.push(pause(Call::SyncDir, when, "/ns", skip * 3));
            plans.push(pause(Call::Sync, when, "NAMESPACES", skip));
        }
        for when in [When::Before, When::Midway, When::After] {
            plans.push(pause(Call::Write, when, "NAMESPACES", skip));
        }
        plans.push(pause(Call::OpenAppend, When::Before, "NAMESPACES", skip.min(2)));
        plans.push(pause(Call::CreateDir, When::After, "/archive-", skip.min(2)));
    }
    {
        let work = tempfile::tempdir().unwrap();
        let cycles = plans.len() as u64;
        let summary =
            catalog::run_with(&exe(), policy, 41, cycles, 120, Duration::from_millis(30), work.path(), &plans)
                .unwrap_or_else(|e| panic!("{}: {}", policy, e));
        assert!(summary.reached.len() >= 10, "{}: only {:?}", policy, summary.reached);
        assert!(summary.creates > 0 && summary.drops > 0, "{}: {}", policy, summary);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// A layout 1, 2 or 3 directory opened by a child that is killed at each
/// file operation of the upgrade: whatever it left, the next open finishes
/// the upgrade and has the same state.
#[test]
fn upgrading_to_layout_4() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/iwdb/tests/fixtures");
    let work = tempfile::tempdir().unwrap();
    for version in ["data-dir-v1", "data-dir-v2", "data-dir-v3"] {
        let pristine = work.path().join(format!("{}-pristine", version));
        copy_dir(&fixtures.join(version).join("store"), &pristine);
        let expected = {
            let store = Store::open(&pristine, catalog_options()).unwrap();
            catalog::snapshot(&store).unwrap()
        };
        let mut killed = 0;
        for call in [Call::Rename, Call::CreateDir, Call::Create, Call::WriteAtomic, Call::SyncDir, Call::RemoveFile] {
            for when in [When::Before, When::After] {
                for skip in 0..30 {
                    let dir = work.path().join(format!("{}-{:?}-{:?}-{}", version, call, when, skip));
                    copy_dir(&fixtures.join(version).join("store"), &dir);
                    let args = CatalogArgs {
                        dir: dir.clone(),
                        seed: 1,
                        acts: 0,
                        policy: Policy::Always,
                        keep: 2,
                        archive: None,
                        retries: Vec::new(),
                        rules: vec![Rule::new(call, when, Action::Pause).skip(skip)],
                    };
                    let mut child =
                        ChildProcess::spawn_command(&exe(), "catalog", args.to_args(), &work.path().join("stderr"))
                            .unwrap();
                    let line = child.wait_for(&["paused", "done", "error"], Duration::from_secs(60));
                    let outcome = child.kill().unwrap();
                    assert!(!outcome.lines.iter().any(|l| l.starts_with("error")), "{:?}", outcome.lines);
                    if line.as_deref() == Some("done") {
                        break;
                    }
                    killed += 1;
                    let report = iwdb::verify(&dir).unwrap();
                    assert!(report.is_ok(), "{} {:?} {:?} {}: {:#?}", version, call, when, skip, report.problems);
                    let store = Store::open(&dir, catalog_options()).unwrap();
                    assert_eq!(
                        catalog::snapshot(&store).unwrap(),
                        expected,
                        "{} {:?} {:?} {}",
                        version,
                        call,
                        when,
                        skip
                    );
                    drop(store);
                    assert!(iwdb::verify(&dir).unwrap().is_ok());
                    fs::remove_dir_all(&dir).unwrap();
                }
            }
        }
        assert!(killed >= 10, "{}: only {} kills landed in the upgrade", version, killed);
    }
}

fn catalog_options() -> iwdb::StoreOptions {
    iwdb_crash::script::check_options(Policy::Always, 2, None)
}
