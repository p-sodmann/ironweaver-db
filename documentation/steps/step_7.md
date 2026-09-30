# Step 7: Backup, PITR, `verify` and Python embedded bindings

Status: todo
Milestone: M1 Embedded durable
Depends on: step 6

## Goal

Complete M1: online backup and point-in-time restore, integrity checks, a local admin CLI, and the embedded store usable from Python.

## Tasks

- [ ] Online backup: checkpoint + WAL segments copied while the store runs (like `pg_basebackup`); optional continuous WAL archiving to a directory.
- [ ] Restore to a given `seq` or timestamp.
- [ ] `verify` (like `PRAGMA integrity_check`): checksums of all files plus invariants (edge endpoints, versions, index contents vs. scan, catalog consistency: non-empty `IndexChanges` after load means the saved indexes and the catalog disagree).
- [ ] Create `crates/iwctl` with `status`, `checkpoint`, `backup`, `restore`, `verify` for local data directories.
- [ ] Create `crates/iwdb-python` (PyO3, maturin): `Store`, transactions as context managers, reads returning plain Python values. Keep the API shape identical to the future remote client (step 14).
- [ ] Wheels via maturin for Linux/macOS/Windows, x86_64/arm64; publish a `0.1.0` pre-release.

## Notes from step 6

- **Every new write goes through `LogFs`**: backup, restore, WAL archiving and `iwctl`'s repairs, if any. Then `failpoint::FailFs` covers them without changes. Add their calls to the harness's plan list (`points()` in `tests/crash/src/harness.rs`) and a test per failpoint, as `crates/iwdb/tests/faults.rs` does for the existing ones. A restore writes a data directory, so it should follow initialization's order (subdirectories, directory sync, marker last with `write_atomic`, directory sync). Then an interrupted restore is a directory without a marker, which open finishes or refuses, and never a half-valid store.
- **Online backup races the checkpointer.** The checkpointer removes old checkpoints and WAL segments while the store runs. Step 6 found that race in `Wal::sync` under `off` (a listed segment vanished before it was opened). A backup must either hold the checkpointer's lock while it lists and copies what it needs, or tolerate a vanished file by retrying from a newer checkpoint. The WAL it copies must reach from its checkpoint to the seq it reports, which the bounded reader (`WalReader::open_until`) can check.
- **Archiving before removal.** Continuous WAL archiving must have a segment durable in the archive (file and directory fsynced) before the checkpointer removes it. The natural place is the checkpointer's segment removal. A crash between archiving and removal leaves the segment in both places, so archiving must be idempotent.
- **PITR acceptance**: the harness's reference model (`iwdb_crash::Model`) keeps every commit record and can go to any seq (`Model::at`). A PITR test can restore to a random mid-history seq and compare with `Model::at(seq)`, also after kills during the restore.
- **`verify` in the harness**: once `verify` exists, `check_recovery` in `tests/crash/src/harness.rs` should run it after every recovery. That checks the files and invariants thousands of times per run.
- **Python and ADR 0008**: a panic in the store's commit path aborts the process, and so the Python interpreter. It is a bug-only path (upstream #28), but the Python docs must say so. A later option can keep the process alive with the store closed for reads as well (ADR 0008, option 3).
- **`Store::synced_seq` under `off`** is 0 after open, and moves only on explicit syncs (step 6 fix). `iwctl status` should show it as "unknown/none" under `off` rather than as a durable seq.
- **Interrupted cleanup**: after a crash, a data directory can hold more checkpoints and WAL segments than `keep` allows, until the next checkpoint that writes a file ([data-dir.md](../formats/data-dir.md), "Interrupted cleanup"). `verify` must accept that, and `iwctl checkpoint` on an idle store removes nothing extra.

## Acceptance criteria

- PITR to a mid-history `seq` equals the reference model at that `seq`.
- The wheel installs and passes the Python tests on all target platforms in CI.
- M1 is done: update [README.md](README.md).
