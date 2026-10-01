# Step 7: Backup, PITR, `verify` and Python embedded bindings

Status: done (the PyPI publish of 0.1.0 is prepared and left to the owner; see Tasks)
Milestone: M1 Embedded durable
Depends on: step 6

## Goal

Complete M1: online backup and point-in-time restore, integrity checks, a local admin CLI, and the embedded store usable from Python.

## Tasks

- [x] Online backup: checkpoint + WAL segments copied while the store runs (like `pg_basebackup`); optional continuous WAL archiving to a directory. `Store::backup`, `StoreOptions::archive` ([ADR 0009](../adr/0009-backup-archive-restore.md), [backup.md](../formats/backup.md), [archive.md](../formats/archive.md)).
- [x] Restore to a given `seq` or timestamp: `iwdb::restore`, to a seq, a time (WAL format 2 carries commit times, [ADR 0010](../adr/0010-commit-times.md)) or the latest, from a backup, an archive or both ([data-dir.md](../formats/data-dir.md), "Restore").
- [x] `verify` (like `PRAGMA integrity_check`): checksums of all files plus invariants (edge endpoints, versions, index contents vs. scan, catalog consistency: non-empty `IndexChanges` after load means the saved indexes and the catalog disagree). `iwdb::verify`, `iwdb_engine::invariants` ([ADR 0011](../adr/0011-verify.md)).
- [x] Create `crates/iwctl` with `status`, `checkpoint`, `backup`, `restore`, `verify` for local data directories ([iwctl.md](../iwctl.md), [ADR 0012](../adr/0012-iwctl.md)).
- [x] Create `crates/iwdb-python` (PyO3, maturin): `Store`, transactions as context managers, reads returning plain Python values. Keep the API shape identical to the future remote client (step 14): the contract is [python-api.md](../python-api.md) ([ADR 0013](../adr/0013-python-bindings.md)).
- [x] Wheels via maturin for Linux and macOS, x86_64 and arm64 (abi3, CPython 3.9 and later), built and tested in CI on every push. Windows wheels moved to step 16 (Changes to the plan).
- [ ] Publish a `0.1.0` pre-release on PyPI. Prepared: the version is 0.1.0, `.github/workflows/release.yml` publishes on a `v0.1.0` tag with trusted publishing, and [releasing.md](../releasing.md) has the steps. The owner publishes; this box (and M1's PyPI part) stays open until then.

## Notes from step 6

- **Every new write goes through `LogFs`**: backup, restore, WAL archiving and `iwctl`'s repairs, if any. Then `failpoint::FailFs` covers them without changes. Add their calls to the harness's plan list (`points()` in `tests/crash/src/harness.rs`) and a test per failpoint, as `crates/iwdb/tests/faults.rs` does for the existing ones. A restore writes a data directory, so it should follow initialization's order (subdirectories, directory sync, marker last with `write_atomic`, directory sync). Then an interrupted restore is a directory without a marker, which open finishes or refuses, and never a half-valid store.
- **Online backup races the checkpointer.** The checkpointer removes old checkpoints and WAL segments while the store runs. Step 6 found that race in `Wal::sync` under `off` (a listed segment vanished before it was opened). A backup must either hold the checkpointer's lock while it lists and copies what it needs, or tolerate a vanished file by retrying from a newer checkpoint. The WAL it copies must reach from its checkpoint to the seq it reports, which the bounded reader (`WalReader::open_until`) can check.
- **Archiving before removal.** Continuous WAL archiving must have a segment durable in the archive (file and directory fsynced) before the checkpointer removes it. The natural place is the checkpointer's segment removal. A crash between archiving and removal leaves the segment in both places, so archiving must be idempotent.
- **PITR acceptance**: the harness's reference model (`iwdb_crash::Model`) keeps every commit record and can go to any seq (`Model::at`). A PITR test can restore to a random mid-history seq and compare with `Model::at(seq)`, also after kills during the restore.
- **`verify` in the harness**: once `verify` exists, `check_recovery` in `tests/crash/src/harness.rs` should run it after every recovery. That checks the files and invariants thousands of times per run.
- **Python and ADR 0008**: a panic in the store's commit path aborts the process, and so the Python interpreter. It is a bug-only path (upstream #28), but the Python docs must say so. A later option can keep the process alive with the store closed for reads as well (ADR 0008, option 3).
- **`Store::synced_seq` under `off`** is 0 after open, and moves only on explicit syncs (step 6 fix). `iwctl status` should show it as "unknown/none" under `off` rather than as a durable seq.
- **Interrupted cleanup**: after a crash, a data directory can hold more checkpoints and WAL segments than `keep` allows, until the next checkpoint that writes a file ([data-dir.md](../formats/data-dir.md), "Interrupted cleanup"). `verify` must accept that, and `iwctl checkpoint` on an idle store removes nothing extra.

## Decisions

Each is recorded where the table says; the formats are versioned contracts with fixtures (design rule 4).

| Question | Decision | Recorded in |
|---|---|---|
| What a backup is on disk | A data directory in layout 2 (the source's marker and history id, `checkpoints/`, `wal/`) plus a `BACKUP` manifest (version 1: magic, version, length, JSON, CRC32C) listing every file with its length and CRC32C, the seq, the history, the commit time. Fixture `backup-v1`. A store refuses to open a backup; it is restored | ADR 0009, [backup.md](../formats/backup.md) |
| Consistency with the checkpointer | The backup holds the checkpointer's mutex while it lists and copies, so nothing it copies is removed; it takes the live namespace's lock only for the fsync. Commits wait for the fsync, checkpoints for the copy | ADR 0009, guarantees.md |
| Which seq a backup reaches | It fsyncs the WAL first and takes the synced seq (the last commit; the synced seq as it is if the store is read-only). The last segment is copied up to that record (`segment_prefix` checks every frame and never decodes past it); verify checks that the WAL ends at the manifest's seq | ADR 0009 |
| Writing a backup | Through `LogFs`, in initialization's order: an empty `BACKUP` first, the files (fsynced), the directory syncs, the manifest (`write_atomic`), the marker last. An interrupted backup is refused by a store, verify and restore | backup.md |
| WAL archiving | The checkpointer copies segments into the archive (temporary file, fsync, rename) and syncs the archive directory before removing them. Idempotent: an existing copy must have the same bytes and is rewritten. A failed copy removes nothing and is retried by the next checkpoint (the WAL grows, `checkpoint_failure` reports it); a failed directory sync disables checkpoints until reopening | ADR 0009, [archive.md](../formats/archive.md), guarantees.md |
| The end of a restore at `N` | A new directory with one checkpoint at `N` and an empty WAL (replayed in memory from the newest backup checkpoint at or below `N`), so the next commit is `N + 1`; no WAL is cut. A `RESTORING` file guards the write; an interrupted restore is refused, never finished by open | ADR 0009, data-dir.md "Restore" |
| Divergent histories | A random history id in the data directory's marker (layout 2, fixture `data-dir-v2`; layout 1 is upgraded on open) and in the archive's marker. Every restore starts a new history. A store archives only into an archive of its own history; a restore combines a backup and an archive only of the same history | ADR 0009, data-dir.md |
| Timestamps | Added now: WAL format 2 puts a commit time (the writer's clock, microseconds, made non-decreasing) in every frame (fixture `wal-v2`, format 1 still read). Restore to `T` = the last commit in seq order whose time is at or before `T` | ADR 0010, wal.md |
| What verify checks and tolerates; locking | Every checksum, every checkpoint (load, seq, namespace, index changes, invariants), the WAL from its first segment, coverage of every checkpoint, the WAL replayed onto the oldest checkpoint against each newer one, a backup's manifest, an archive's chain. A torn tail, temporary files and an interrupted cleanup are notes. It never writes, and takes a shared lock (`Locked` on an open store) | ADR 0011 |
| iwctl | Only parses, calls the library (`iwdb::status`, `Store::checkpoint`/`backup`, `iwdb::restore`/`verify`) and prints. A hand-written parser (no new dependency); text or `--json`; exit codes 0 ok, 1 damage, 2 usage, 3 locked, 4 other. `status` shows the synced seq as `none` under `off`. `checkpoint` needs `--archive` or `--no-archive` | ADR 0012, [iwctl.md](../iwctl.md) |
| Python API, values, errors, threads, ABI, panics | Written down first in python-api.md. Exact value conversion both ways (i64, float bits, bytes, lists, str-keyed dicts, dates, datetimes with offsets, the depth limit as the pipeline counts it; tuples refused). One exception per error kind under `iwdb.Error`. The GIL is released around every call that does I/O or may wait; a `Store` is shared between threads. abi3 for CPython 3.9+. A commit-path panic aborts the interpreter (documented); other panics raise `iwdb.InternalError` | ADR 0013, python-api.md |
| Design rule 1 in CI | `iwdb-python` joins the workspace; the "No Python in the dependency graph" check now lists the engine, storage, store, CLI and harness crates | ADR 0013, ci.yml |
| Platforms | Linux and macOS only; Windows wheels come later (step 16): no directory fsync there, and nothing is tested on Windows | ADR 0013, guarantees.md "Platforms" |

## Format changes

| Format | From | To | Fixtures |
|---|---|---|---|
| WAL segment ([wal.md](../formats/wal.md)) | 1 | 2: a commit time per frame (frame header 25 → 33 bytes) | `crates/iwdb-storage/tests/fixtures/wal-v1/` (still read), `wal-v2/` |
| Data directory ([data-dir.md](../formats/data-dir.md)) | 1 | 2: a history id in the marker (16 → 32 bytes); `BACKUP`, `RESTORING` | `crates/iwdb/tests/fixtures/data-dir-v1/` (still read, upgraded on open), `data-dir-v2/` |
| Backup manifest ([backup.md](../formats/backup.md)) | new | 1 | `crates/iwdb/tests/fixtures/backup-v1/` |
| WAL archive ([archive.md](../formats/archive.md)) | new | 1 | `crates/iwdb/tests/fixtures/archive-v1/` |

## Acceptance criteria

- [x] PITR to a mid-history `seq` equals the reference model at that `seq`: from a backup alone, an archive alone and both, to random seqs and to times (`crates/iwdb/tests/pitr.rs`), and in the crash harness, which restores to a random seq in a quarter of its cycles and compares with `Model::state_at` (Results).
- [x] The wheel installs and passes the Python tests on all target platforms in CI: the `python` job of `ci.yml` builds the wheel with maturin on Linux x86_64 and arm64 and macOS x86_64 and arm64 and runs the tests on Python 3.9 and 3.13. Locally (macOS arm64): the same 68 tests on 3.9, 3.12 and 3.13, also against a wheel built with Rust 1.85. CI hasn't run yet (nothing is pushed).
- [x] M1 is done, except the PyPI publish: [README.md](README.md) says so.

## Changes to the plan

- **Windows wheels come later (step 16)**, not in this step: on Windows the directory fsync is a no-op (ADR 0005), so the durability after an OS crash differs, and nothing (the workspace tests, the crash harness, the Python tests) runs on Windows yet. A wheel with different, untested guarantees would break design rule 3. Step 16 has the task (directory sync, CI, a harness that kills with `TerminateProcess`, then the wheels).
- **The PyPI publish is the owner's**: the release workflow and the steps are ready (releasing.md); the task stays open until the publish.
- **Timestamps are in**, not deferred: WAL format 2 (ADR 0010).
- **The data directory layout moved to 2** for the history id (ADR 0009); step 5's layout 1 is still read and is upgraded when a store opens it.
- **More than the task list**: `iwdb::status` and `iwdb_storage::inspect` (what `iwctl status` shows), `Store::status`, a `segment_prefix` reader and `WalReader::from_segments`, invariant checks and a state comparison in `iwdb_engine::invariants`, crash points for backups, archiving and restores.
- **`iwctl checkpoint` needs `--archive` or `--no-archive`**: whether a store archives is an option of its program, not of the directory, and a checkpoint without the archive would leave a gap in it (ADR 0012).

## Bugs found in steps 4 to 6

- **Step 5: a just-closed store's lock could look held** (`Locked`) while another thread spawned processes: a process being spawned holds a copy of every open file until its exec, and a `flock` belongs to the open file. 3.5% of 2000 reopens failed under heavy spawning; the crash points ran into it once `verify` (a shared lock) ran right before each open. Fixed in its own commit: taking any lock retries for about 80 ms (`layout::lock_file`); regression test `reopening_while_another_thread_spawns_processes` (60 to 73 failures in 800 attempts without the fix, none with it). data-dir.md also documents that a child forked without exec inherits the lock.
- Found in this step's own code by the long run: verify missed a WAL that ends before its base checkpoint (above, Results). Fixed with a test.
- Two test assumptions from step 5 that the larger WAL frames broke (no behaviour change): a fault test assumed that its failing write tore a frame, while with 8 more bytes per frame it now tore a new segment's header first (`crashed_with_torn_tail` finds a seed that tears a frame).

## Findings in ironweaver-core

- **New: the binary header's `flags` and `reserved` bytes are never checked** (and the CRC covers only the payload): damage there, or a flag a newer writer sets, goes unnoticed. Pinned in `binary_header_flags_and_reserved_bytes_are_not_checked` (`core_smoke.rs`), drafted as upstream-issues.md draft 15, filed as [#33](https://github.com/p-sodmann/Ironweaver/issues/33), worked around in `verify` (it reads the 16 header bytes itself), and listed in [upstream-check.md](upstream-check.md).
- Everything else verify needs is exposed: the index check enumerates an index completely through range lookups. #30 and #28 are unchanged by this step (core review, "Findings from step 7").

## Results

**Long run** (`iwdb-crash --policy all --seed 20261001 --seeds 2 --cycles 1000`, release build, Apple silicon laptop, macOS, 2026-10-01). Five of the six policy/seed runs passed: **5000 kill/recover cycles in 17.4 minutes, no lost acknowledged commit, no partial transaction**:

| Policy, seed | Time | Recoveries checked (+ by digest) | verify before recovery | Archives verified | Backups complete and restored / interrupted and refused | Restores in a child: complete / interrupted (killed at a failpoint) |
|---|---|---|---|---|---|---|
| `always` 20261001 | 230 s | 768 (+969) | 768 | 607 | 350 / 134 | 71 / 82 (66) |
| `group` 20261001 | 218 s | 776 (+967) | 776 | 356 | 474 / 170 | 37 / 52 (41) |
| `off` 20261001 | 160 s | 794 (+985) | 797 | 708 | 291 / 158 | 92 / 103 (83) |
| `always` 20261002 | 228 s | 750 (+964) | 750 | 619 | 368 / 120 | 72 / 72 (65) |
| `group` 20261002 | 208 s | 755 (+970) | 755 | 693 | 396 / 158 | 89 / 83 (74) |

In all: verify ran 3846 times before a recovery, archives were verified 2983 times (each starting at seq 1 and reaching the WAL: no removed segment lost), 1879 backups taken while the store ran were verified and restored to the model's state at their seq, 740 interrupted ones were refused, and 361 restores to random seqs matched the model while 392 killed ones were refused. The sixth run (`off` 20261002) **failed at cycle 269**: after a simulated OS crash the WAL ended before the only checkpoint; recovery refused (allowed under `off`) but verify reported no problem. Cause: verify didn't check that the WAL reaches its base checkpoint. Fixed (a step 7 bug; test `a_wal_that_ends_before_the_only_checkpoint_is_a_problem`), and that seed then passed 300 cycles. An earlier attempt also stopped at a harness bug (a panic chosen at a backup's fsync unwinds, by design), fixed in the harness. A full rerun of the long run is left to the nightly workflow.

**Short run** (as CI, `--policy all --cycles 150`): 450 cycles in 110 s (59 s in step 6; each cycle now also verifies, checks backups and archives, and sometimes restores).

**`cargo test --workspace`**: about 100 s of test time locally (73 s in step 6): `crash_points.rs` 24 s, `model.rs` 13 s, `faults.rs` 9 s, `backup.rs` 9 s, `pitr.rs` 7 s, `short_run.rs` 5 s.

**Python** (`pytest crates/iwdb-python/tests`, 68 tests, about 6 s): passed on macOS arm64 with Python 3.9, 3.12 and 3.13, also against the wheel built with Rust 1.85. Linux x86_64/arm64 and macOS x86_64 run in CI, which hasn't run yet.

## Notes for step 8

See [step_8.md](step_8.md#notes-from-step-7).

## Non-goals

- Concurrent readers, idempotency keys, `min_seq` (step 8); several namespaces (step 9); the `Database` trait, the query layer and `EXPLAIN` (step 10); servers and the remote client (steps 11 to 14); `iwctl shell` (step 14).
- Incremental backups (an archive does the same), restoring in place, pruning an archive (step 16), Windows (step 16).
