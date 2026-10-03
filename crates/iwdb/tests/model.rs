//! A random workload with random checkpoints, clean reopens and crashes in
//! between (a drop without close, sometimes with a torn frame of an
//! unacknowledged commit). After every reopen, the store's canonical
//! state, catalog and seq equal those of a reference namespace that ran
//! the same commits in memory. Small segments and `keep` 1 to 3 make
//! checkpoints remove segments and older checkpoints along the way.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs::OpenOptions;
use std::io::Write;

use iwdb::Store;
use iwdb_engine::testutil::workload::{Step, seed, step};
use proptest::collection::vec;
use proptest::prelude::*;
use support::{checkpoints, frame, last_segment, options, pad, reference, run, segment_seqs, state, store_state};

#[derive(Clone, Debug)]
enum Action {
    Commit(Step),
    Pad,
    Checkpoint,
    /// Close and reopen.
    Reopen,
    /// Drop without close and reopen.
    Crash,
    /// Drop, append part of a frame (a commit in progress), reopen.
    TornCrash(usize),
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        10 => step().prop_map(Action::Commit),
        6 => Just(Action::Pad),
        3 => Just(Action::Checkpoint),
        1 => Just(Action::Reopen),
        1 => Just(Action::Crash),
        1 => (1usize..60).prop_map(Action::TornCrash),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn reopening_always_recovers_the_reference_state(
        seed in seed(),
        actions in vec(action(), 1..120),
        keep in 1usize..=3,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(keep);
        let mut reference = reference();
        let mut store = Store::open(dir.path(), opts.clone()).unwrap();
        run(&store, &mut reference, &[seed]);
        let mut cut = false;
        for (i, action) in actions.into_iter().enumerate() {
            match action {
                Action::Commit(step) => run(&store, &mut reference, &[step]),
                Action::Pad => run(&store, &mut reference, &[pad(i)]),
                Action::Checkpoint => {
                    let outcome = store.checkpoint().unwrap();
                    prop_assert_eq!(outcome.seq, reference.seq());
                    prop_assert!(checkpoints(dir.path()).len() <= keep);
                    cut |= !outcome.removed_segments.is_empty();
                }
                Action::Reopen | Action::Crash | Action::TornCrash(_) => {
                    match action {
                        Action::Reopen => store.close().unwrap(),
                        _ => drop(store),
                    }
                    if let Action::TornCrash(len) = action {
                        let next = reference.seq() + 1;
                        let bytes = frame(next, next - 1, &[7; 40]);
                        let mut file = OpenOptions::new().append(true).open(last_segment(dir.path())).unwrap();
                        file.write_all(&bytes[..len.min(bytes.len() - 1)]).unwrap();
                    }
                    store = Store::open(dir.path(), opts.clone()).unwrap();
                    prop_assert_eq!(store_state(&store), state(&reference));
                    prop_assert_eq!(store.recovery().torn_tail.is_some(), matches!(action, Action::TornCrash(_)));
                    prop_assert!(store.recovery().skipped_checkpoints.is_empty());
                }
            }
        }
        drop(store);
        let store = Store::open(dir.path(), opts).unwrap();
        prop_assert_eq!(store_state(&store), state(&reference));
        if cut {
            prop_assert!(segment_seqs(dir.path())[0] > 1);
        }
    }
}
