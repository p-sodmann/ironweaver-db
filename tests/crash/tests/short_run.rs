//! A short run of the kill -9 harness for each fsync policy, so that
//! `cargo test --workspace` exercises it. The runs CI relies on are longer
//! (`iwdb-crash --cycles N`, see .github/workflows).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use iwdb_crash::{Config, Policy, run};

fn short_run(policy: Policy, seed: u64) {
    let work = tempfile::tempdir().unwrap();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_iwdb-crash"));
    let mut config = Config::new(exe, policy, seed, 12, work.path().to_path_buf());
    config.acts = 60;
    match run(&config) {
        Ok(summary) => {
            println!("{}", summary);
            assert_eq!(summary.cycles, 12);
            assert!(summary.checked + summary.child_opens >= 12, "every cycle's recovery is checked");
        }
        Err(failure) => panic!("{}", failure),
    }
}

#[test]
fn always() {
    short_run(Policy::Always, 1);
}

#[test]
fn group() {
    short_run(Policy::Group, 2);
}

#[test]
fn off() {
    short_run(Policy::Off, 3);
}
