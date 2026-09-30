# Step 6: Crash and fault-injection suite

Status: done
Milestone: M1 Embedded durable
Depends on: step 5

## Goal

Prove the durability guarantee: after any crash, recovery reaches exactly the last acknowledged commit (fsync `always`) and never exposes a partial transaction.

## Tasks

- [x] Failpoints in WAL append, fsync, segment creation and rotation, checkpoint write, rename, directory fsync, file removal and truncation: `iwdb_storage::failpoint::FailFs`, a `LogFs` wrapper behind the `failpoints` feature, instead of the `fail` crate (see "Changes to the plan"; [ADR 0007](../adr/0007-failpoints-and-crash-harness.md)).
- [x] Kill -9 harness (`tests/crash`, binary `iwdb-crash`): a child process runs a random workload and reports acknowledged `seq`s; the parent kills it at random points and at failpoints, reopens and compares with the reference model (canonical state, catalog, seq).
- [x] Simulated torn writes, full disk (ENOSPC) and fsync errors, each with defined behaviour ([guarantees.md](../guarantees.md), "Crashes and simulated failures"; tests in `crates/iwdb/tests/faults.rs`). OS crashes are simulated on top of kills by cutting or zeroing the unsynced end of the WAL.
- [x] A panic inside the commit path is treated as a crash: the store aborts the process, and recovery restores the state ([ADR 0008](../adr/0008-panics-in-the-commit-path-abort.md); `crates/iwdb/tests/panics.rs` and the harness).
- [x] Short run on every PR (`ci.yml`, job `crash`), long run nightly (`nightly.yml`).

## Notes from step 4

- The fault-injection seam is `iwdb_storage::io::LogFs` / `LogFile`: `create`, `open_append`, `rename`, `sync_dir`, `write_all` and `sync`. Failpoints go on these calls (in `StdFs`, or in a wrapper). `crates/iwdb-storage/tests/common/mod.rs` (`TestFs`) already injects whole or partial write failures and failures of every other call, and counts calls. `tests/wal_faults.rs` shows the expected behaviour for each.
- ENOSPC is a failed write, which puts the log into the failed, read-only state (ADR 0005). A partial write leaves a torn tail that the reader reports.
- The random workload strategies are in `crates/iwdb-engine/tests/workload/mod.rs`, which test crates include with `#[path]`. A kill -9 child *binary* can't include test code that way. Move the strategies into a feature-gated `iwdb_engine::testutil` module when the harness needs them.
- `FsyncPolicy::Group` and `Off` lose acknowledged commits only in the windows described in [guarantees.md](../guarantees.md). The harness compares against the last synced `seq` for them, and against the last acknowledged `seq` for `Always`.

## Notes from step 5

- **Every write goes through `iwdb_storage::io::LogFs`** now: the WAL's `create`, `open_append`, `rename`, `sync_dir`, `write_all` and `sync`, plus `write_atomic` (checkpoints and the marker, through the core's), `remove_file` (old checkpoints, WAL segments, temp files, a headerless segment) and `truncate` (a torn tail). Put the failpoints there. `crates/iwdb-storage/tests/common/mod.rs` (`TestFs`) injects all of them, and `Store::open_with(fs, ..)` takes one. The `TestFs` hook (`set_hook`) pauses a thread at a call, for example inside a checkpoint's write. The lock (`fs4`) and the reads (`WalReader`, checkpoint loading) don't go through the seam.
- The core's `write_atomic` is one call: a failpoint can fail it before anything is written, or fail the writer halfway (`Fault::Partial`), but it can't stop between its fsync and its rename. A kill -9 can, which is what the harness should cover. The temp file it leaves is `checkpoints/.<name>.<pid>.<n>.tmp`, which open removes.
- **Crash points to cover in the harness**: during an append (a torn tail), between a checkpoint's rename and its directory sync, between removing old checkpoints and removing WAL segments, in the middle of removing segments, during recovery's truncation (the next open redoes it), and during initialization (before the marker).
- **Expected state per policy**: the child reports acknowledged seqs, and `Store::synced_seq()` gives the durable one under `group` and `off`. With `always`, recovery must reach the last acknowledged seq (or one more: a commit whose fsync failed or was cut off may still be in the log, ADR 0005). A process kill loses nothing under any policy, because the page cache survives; only an OS crash or power loss loses unsynced records, and a kill -9 harness can't produce one. Simulate it by truncating or zeroing the unsynced suffix of the last segment (after `synced_seq`) before reopening.
- **What must fail cleanly**: `a failed directory sync` disables checkpoints until reopening (`Error::CheckpointsDisabled`). A failed WAL write or fsync makes the store read-only (`Store::read_only`). In both cases reopening must recover exactly.
- **Panics**: a panic inside `Store::commit` poisons the store's mutex, which the store reports as read-only. Step 6 makes the process abort on a panic in the commit path (`panic = "abort"` for the child binary, or a panic hook) and checks that recovery restores the state.
- The random workload strategies are still test code (`crates/iwdb-engine/tests/workload/mod.rs`); the kill -9 child needs them in a feature-gated `iwdb_engine::testutil` module (see the notes from step 4). `crates/iwdb/tests/support/mod.rs` has a deterministic `workload(n, seed)` that pads each step with a 200-byte commit so that segments rotate.
- `crates/iwdb/tests/data_dir.rs` (`the_lock_is_released_when_the_process_dies`) already runs the test binary as a child and kills it; the harness can use the same pattern.

## Acceptance criteria

- [x] Thousands of kill/recover cycles without a lost acknowledged commit or a partial transaction (Results, below).
- [x] Every failpoint has at least one test (`faults.rs`, `panics.rs`, `wal_faults.rs`, `crash_points.rs`, and the `failpoint.rs` unit tests).

## Changes to the plan

- **No `fail` crate.** Its registry is global: tests that configure it must run one at a time (`FailScenario` takes a process-wide lock), and it brings `rand` 0.8 and `once_cell`. Instead, `FailFs` wraps any `LogFs` with rules of its own, behind a `failpoints` feature, so `StdFs` has no failpoints. It adds no dependency, and `TestFs` in the step 4 and 5 tests is now `FailFs`.
- **Failpoint coverage beyond the task list**: every `LogFs` call (`create`, `open_append`, `rename`, `sync_dir`, `write`, `sync`, `write_atomic`, `remove_file`, `truncate`), at four points in a call (before, halfway, when `write_atomic`'s writer is done, after), with five actions (I/O error, `ENOSPC`, pause, panic, abort).
- **OS-crash simulation.** A kill can't lose unsynced data, so the harness simulates an OS crash after some kills. It cuts or zeroes the last segment after the length its fsyncs made durable (a sync log the child writes before each fsync), which checks the `group` and `off` rows of the guarantees.
- **A panic in the commit path aborts the process** (ADR 0008). That is a library behaviour change: step 5 made the store read-only, but reads ignored the poisoned lock, so a panic halfway through apply could have shown a partial transaction.
- **Workload strategies** moved to `iwdb_engine::testutil::workload` (feature `testutil`), as the notes from step 4 planned, with `Stream`, an endless workload from a seed.
- **Bugs from earlier steps**, each fixed in its own commit with a regression test:
  - step 4: under `off`, `Wal::sync` fsynced only the current segment and no directory, and a new writer claimed the whole log synced. So `synced_seq` (also in frames) overstated durability, which could turn an OS crash's torn tail into reported corruption. Now a writer under `off` starts at `synced_seq` 0, and a sync covers every segment with unsynced records, then the directory (`off_syncs_every_unsynced_segment_on_an_explicit_sync`). The harness then found a race in that fix, with the checkpointer removing a segment during the sync (`off_sync_skips_a_segment_removed_meanwhile`);
  - step 5: a panic inside a `Store::read` closure made the store read-only (`a_panic_in_a_read_leaves_the_store_writable`).
- **Documented, not changed**: a checkpoint run with nothing new to write doesn't redo the removals that a crash interrupted. The next checkpoint that writes a file does them ([data-dir.md](../formats/data-dir.md), "Interrupted cleanup"). Redoing them would rely on a directory entry that may never have been synced.

## Results

**Long run** (`iwdb-crash --policy all --seed 20260930 --seeds 2 --cycles 1500`, release build; Apple silicon laptop, macOS, internal SSD, 2026-09-30): **9000 kill/recover cycles, no lost acknowledged commit, no partial transaction**, in 21 minutes (0.10 to 0.17 s per cycle).

| Policy, seed | Time | Recoveries checked (+ by the next child's digest) | Acknowledged commits | OS crashes simulated (unsynced acknowledged commits they lost) | In-flight commits recovered | Torn tails cut (frames discarded) | Refused (`off`) |
|---|---|---|---|---|---|---|---|
| `always` 20260930 | 250 s | 1094 (+1431) | 18,448 | 29 (0) | 366 | 39 (0) | 0 |
| `group` 20260930 | 195 s | 1155 (+1443) | 22,241 | 130 (157) | 22 | 117 (24) | 0 |
| `off` 20260930 | 160 s | 1212 (+1449) | 22,915 | 358 (445) | 19 | 292 (69) | 3 |
| `always` 20260931 | 235 s | 1128 (+1442) | 18,358 | 31 (0) | 354 | 52 (0) | 0 |
| `group` 20260931 | 197 s | 1156 (+1438) | 22,414 | 132 (164) | 30 | 115 (13) | 0 |
| `off` 20260931 | 153 s | 1220 (+1455) | 22,948 | 334 (426) | 16 | 263 (49) | 4 |

About 55% of the kills came at a failpoint and 40% after a random delay; the rest were plans whose point the child didn't reach. 25 of the 26 failpoint kinds in the plans were reached. The exception is recovery's truncation: `truncate:after` was reached once and `truncate:before` never, because a torn tail left for the next child's recovery is rare in random runs. Both are hit every time by `crash_points.rs` (`during_recoverys_truncation`). About 10% of the failpoint plans aborted or panicked instead of pausing. Every recovery from a checkpoint loaded it without a skip or index changes, 1800 temporary files were removed (kills inside checkpoints and rotations), and 124 new data directories covered initialization. The lost commits under `group` and `off` were all after the child's last reported synced seq, which those policies allow. The 7 refusals under `off` were `LogEndsBefore` with a checkpoint past the surviving log, as the guarantees allow.

**Short run** (as CI's PR job, `--policy all --cycles 150`): 450 cycles in 59 s locally.

**`cargo test --workspace`**: 73 s locally (41 s before step 6): `faults.rs` 9 s, `panics.rs` 1 s, `crash_points.rs` 16 s (every crash point under every policy), `short_run.rs` 3 s (12 cycles per policy).

**Checking the checker.** Two deliberate bugs made the harness fail within the first cycles, with the seed and a rerun command: an OS-crash simulation that also cut synced data (reported as `LogEndsBefore` where `always` must recover everything), and a model that skipped catalog changes (reported as a catalog difference).

## Notes for step 7

See [step_7.md](step_7.md#notes-from-step-6).

## Non-goals

- Backup, PITR, `verify`, `iwctl`, Python (step 7); concurrent readers and idempotency keys (step 8); several namespaces (step 9).
- Simulating an OS crash's loss in files other than the last WAL segment, or of directory entries (ADR 0007, "Consequences").
