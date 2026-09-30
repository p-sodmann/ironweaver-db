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

## Acceptance criteria

- Thousands of kill/recover cycles without a lost acknowledged commit or a partial transaction.
- Every failpoint has at least one test.
