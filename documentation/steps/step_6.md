# Step 6: Crash and fault-injection suite

Status: todo
Milestone: M1 Embedded durable
Depends on: step 5

## Goal

Prove the durability guarantee: after any crash, recovery reaches exactly the last acknowledged commit (fsync `always`) and never exposes a partial transaction.

## Tasks

- [ ] Failpoints (`fail` crate) in WAL append, fsync, segment rotation, checkpoint write, rename and directory fsync.
- [ ] Kill -9 harness: a child process runs a random workload and reports acknowledged `seq`s; the parent kills it at random points, reopens and compares with the reference model (canonical state).
- [ ] Simulated torn writes, full disk (ENOSPC) and fsync errors, each with defined behaviour.
- [ ] A panic inside the commit path is treated as a crash (process exits, recovery restores state).
- [ ] Short run on every PR, long run nightly.

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

- Thousands of kill/recover cycles without a lost acknowledged commit or a partial transaction.
- Every failpoint has at least one test.
