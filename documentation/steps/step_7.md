# Step 7: Backup, PITR, `verify` and Python embedded bindings

Status: todo
Milestone: M1 Embedded durable
Depends on: step 6

## Goal

Complete M1: online backup and point-in-time restore, integrity checks, a local admin CLI, and the embedded store usable from Python.

## Tasks

- [ ] Online backup: checkpoint + WAL segments copied while the store runs (like `pg_basebackup`); optional continuous WAL archiving to a directory.
- [ ] Restore to a given `seq` or timestamp.
- [ ] `verify` (like `PRAGMA integrity_check`): checksums of all files plus invariants (edge endpoints, versions, index contents vs. scan, catalog consistency).
- [ ] Create `crates/iwctl` with `status`, `checkpoint`, `backup`, `restore`, `verify` for local data directories.
- [ ] Create `crates/iwdb-python` (PyO3, maturin): `Store`, transactions as context managers, reads returning plain Python values. Keep the API shape identical to the future remote client (step 14).
- [ ] Wheels via maturin for Linux/macOS/Windows, x86_64/arm64; publish a `0.1.0` pre-release.

## Acceptance criteria

- PITR to a mid-history `seq` equals the reference model at that `seq`.
- The wheel installs and passes the Python tests on all target platforms in CI.
- M1 is done: update [README.md](README.md).
