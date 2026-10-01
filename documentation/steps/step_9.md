# Step 9: Catalog operations and namespaces

Status: done
Milestone: M2 Service
Depends on: step 8

## Goal

Multiple named graphs per store, and index and constraint definitions managed like a database catalog.

## Tasks

- [x] Namespaces: create, list, drop, each with its own graph, seq space, WAL and checkpoints ([ADR 0017](../adr/0017-namespaces.md): one WAL per namespace plus a namespace log; names, ids, layout 4, the upgrade from layouts 1-3, crash-safe create and drop, what happens to readers and waiters of a dropped namespace; keys: [ADR 0018](../adr/0018-namespace-keys-and-restore.md)).
- [x] Index management: create (online build, [ADR 0019](../adr/0019-online-index-build.md): the scan holds neither the writer mutex nor the namespace lock for long; the insertion still holds the write lock, measured), drop, list with state; definitions persisted in the catalog, indexes rebuilt on recovery.
- [x] Constraints: unique (label + path), required (label + path); adding one validates existing data first (a unique constraint's validation holds the writer mutex, ADR 0019); scoped to one namespace.
- [x] Catalog views for `status`: counts, index state and entries, memory per namespace, in `Store::status`, `iwctl status` / `namespaces` / `indexes` and Python.
- [x] Formats: layout 4, backup manifest 2, archive format 2 (the WAL format stays 3), readers for the older versions, fixtures `data-dir-v4`, `backup-v2`, `archive-v2`; `data-dir.md`, `wal.md`, `backup.md`, `archive.md` updated.
- [x] Python and `iwctl`: the API is in `documentation/python-api.md` (written first); namespace and catalog commands in `documentation/iwctl.md`.

## Notes from step 8

- **Locks per namespace.** Each `LoggedNamespace` has a writer mutex (its WAL) and an `RwLock<Namespace>` (ADR 0014). Several namespaces need one pair each, or (with a shared WAL) one writer mutex and a lock per namespace. Keep the order checkpointer → writer → namespace, and nothing that takes a writer lock while holding a namespace lock. If commits ever span namespaces, take namespace locks in a fixed order (by name).
- **Key tables per namespace.** The idempotency key table lives in the namespace and its checkpoint (`iwdb.keys`, layout 3, ADR 0015). With several namespaces a key is per namespace; say so, or move to a store-wide table (then it needs its own checkpointed place). Catalog changes already take keys: index creation and constraint changes in this step get them for free if they stay `CatalogChange`s. Creating and dropping a namespace should take a key too.
- **The table's size** is a format constant (10 000). If it becomes configurable, make it a logged catalog setting, so that replay stays independent of options.
- **Layout 4.** Namespaces change the directory layout (a WAL and checkpoints per namespace, or a shared WAL with a namespace per record): the next layout version, with a reader for layout 3 and a `data-dir-v4` fixture. A shared WAL also changes the WAL format (a namespace field: format 4).
- **`ReadOptions` and `min_seq`** are per namespace: a seq belongs to one namespace's history (with a shared WAL, one seq space for all).
- **Unique constraints under concurrent writers** (this step's acceptance criterion) hold by construction while every commit runs under the writer mutex; with one writer per namespace they hold per namespace, which is what a unique constraint scopes.

## Acceptance criteria

- [x] Catalog changes survive crash and recovery (the harness's catalog scenario and `namespace_points.rs`).
- [x] Unique constraints hold under concurrent writers (`namespaces.rs`, per namespace).

## Corrections to this step

- The task said "WAL stream position and checkpoint" per namespace; with one WAL per namespace (ADR 0017) each namespace has its own seq space, WAL and checkpoints, so there is no stream position. The ADR also settles the note "with a shared WAL, the WAL format changes (format 4)": it doesn't, because the WAL isn't shared.
- "Unique constraints hold under concurrent writers while every commit runs under the writer mutex" (step 8 note) holds per namespace, which is the scope of a unique constraint.
- The online build is only partly online (ADR 0019): the scan is, the insertion into the index isn't, because the core can only build an index inside `&mut Graph` (upstream #34). It was measured instead of promised.

## Bugs found in earlier steps

- `verify` reported a layout 1-3 directory whose upgrade to layout 4 had been interrupted (files already moved under `ns/`) as damaged, though a store opens it fine. Found by killing the upgrade at each file operation (`namespace_points.rs`); fixed (`layout::legacy_paths_at`). The same state can't arise from step 8 code, so no earlier release is affected.
- No bug in steps 1-8 needed a fix. One design flaw was caught in this step's own first draft: damage to the last event of the namespace log looks like a torn tail, and recovery would then have removed that namespace's directory as an orphan. Open now refuses (`NamespaceDamaged`) and `verify` reports it.

## Findings in ironweaver-core

Two, both filed: [#34](https://github.com/p-sodmann/Ironweaver/issues/34) (no off-graph index build and O(1) install) and [#35](https://github.com/p-sodmann/Ironweaver/issues/35) (no per-index entry count or memory accessor), pinned in `core_smoke.rs`, in the core review and in [upstream-check.md](upstream-check.md). Open issues: #26-#35.

## Results

**Long run** (`iwdb-crash --policy all --seeds 2 --cycles 1000`, seeds 930001 and 930002, release build, Apple silicon laptop, macOS, 2026-10-01). All six policy/seed runs passed: **6000 kill/recover cycles, no lost acknowledged commit, no partial transaction, no key applied twice**, plus **1998 cycles of the new catalog scenario** (a third of the cycles, run after each policy's). Cycle time 1941 s (32 min: `always` 350 s and 358 s, `group` 336 s and 330 s, `off` 283 s and 284 s, each including its catalog cycles); wall time 2932 s (48.9 min).

Catalog scenario in all (six runs): 1998 cycles, 644 recoveries by the parent checked; 114 456 acts acknowledged: 2978 namespaces created, 2854 dropped, 34 175 commits (data, indexes and constraints) into up to 5 namespaces at once; 462 keyed acts answered from their key after a kill; 587 acts in flight at a kill were found complete (the rest not applied; each was retried by key and applied once). Kills landed at the namespace log (`write`, `sync`, `open_append`), namespace directories (`create_dir`, `remove_dir_all`, `sync_dir` on `ns/`), WAL, checkpoint and archive files.

**Short run** (as CI, `--policy all --cycles 150`): 450 cycles plus 150 catalog cycles; `always` 58 s, `group` 55 s, `off` 41 s. (One run showed 735 s of wall time with the same cycle times; the machine was busy with other jobs and a stuck background process, not reproduced.)

**Online index build** (ADR 0019, `latency.rs`, 500 000 nodes, release): plain build in one lock hold 183 ms; online: build 156 ms, longest commit stall 105 ms (about 40 % shorter); unique constraint: 406 ms, stall 345 ms.

**`cargo test --workspace`**: **151 s of test time** (108 s in step 8; the target was about 110 s and this misses it), 245 s of wall time with the build. The growth is new coverage: `namespaces.rs` 13.6 s (every file operation of create, drop and the upgrade failing, in parallel threads, 28 s of CPU), `namespace_points.rs` 19 s (kills at namespace operations and in the upgrade), `crash_points.rs` 28 s, `model.rs` 11 s, `backup.rs` 10 s, `faults.rs` 10 s. Also clean: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo deny check`, the build with Rust 1.85 (`--locked`).

**Python** (`pytest crates/iwdb-python/tests`, 87 tests, 9 of them new in `test_namespaces.py`): passed on macOS arm64.

## Notes for step 10

See [step_10.md](step_10.md#notes-from-step-9).
