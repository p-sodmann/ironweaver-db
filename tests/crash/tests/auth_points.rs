//! Crash points of user, grant and token changes (step 15a, ADR 0043):
//! a child is killed at each write and fsync of the system namespace's WAL
//! (before, halfway through and after), at each write of the namespace log
//! (the system namespace's creation) and at its directory's creation.
//! After each kill the store recovers the users of the acknowledged
//! changes or of one more, never part of one (design rule 3).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use iwdb_crash::{Plan, Policy, auth};
use iwdb_storage::failpoint::{Action, Call, Rule, When};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_iwdb-crash"))
}

fn pause(call: Call, when: When, path: &str, skip: u64) -> Plan {
    Plan::At(Rule::new(call, when, Action::Pause).path(path).skip(skip))
}

#[test]
fn user_and_grant_changes_are_all_or_nothing() {
    let changes = auth::script().len() as u64;
    let mut plans = Vec::new();
    for skip in 0..changes {
        for when in [When::Before, When::Midway, When::After] {
            plans.push(pause(Call::Write, when, "/wal/", skip));
        }
        for when in [When::Before, When::After] {
            plans.push(pause(Call::Sync, when, "/wal/", skip));
        }
    }
    for skip in 0..3 {
        for when in [When::Before, When::Midway, When::After] {
            plans.push(pause(Call::Write, when, "NAMESPACES", skip));
        }
        plans.push(pause(Call::CreateDir, When::After, "/ns/", skip));
    }
    plans.push(Plan::Finish);
    for policy in [Policy::Always, Policy::Group] {
        let work = tempfile::tempdir().unwrap();
        let summary = auth::run(&exe(), policy, work.path(), &plans).unwrap_or_else(|e| panic!("{}: {}", policy, e));
        assert!(summary.paused as u64 >= changes * 3, "{}: only {} kills at a failpoint", policy, summary.paused);
        assert!(summary.in_flight_kept > 0, "{}: no kill after a commit point", policy);
    }
}
