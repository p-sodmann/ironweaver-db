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
