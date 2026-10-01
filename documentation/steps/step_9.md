# Step 9: Catalog operations and namespaces

Status: todo
Milestone: M2 Service
Depends on: step 8

## Goal

Multiple named graphs per store, and index and constraint definitions managed like a database catalog.

## Tasks

- [ ] Namespaces: create, list, drop, each with its own graph, WAL stream position and checkpoint (ADR: one WAL per namespace vs. a shared WAL).
- [ ] Index management: create (online build), drop, list; definitions persisted in the catalog, indexes rebuilt on recovery.
- [ ] Constraints: unique (label + path), required (label + path); adding one validates existing data first.
- [ ] Catalog views for `status`: counts, index sizes, memory per namespace.

## Notes from step 8

- **Locks per namespace.** Each `LoggedNamespace` has a writer mutex (its WAL) and an `RwLock<Namespace>` (ADR 0014). Several namespaces need one pair each, or (with a shared WAL) one writer mutex and a lock per namespace. Keep the order checkpointer → writer → namespace, and nothing that takes a writer lock while holding a namespace lock. If commits ever span namespaces, take namespace locks in a fixed order (by name).
- **Key tables per namespace.** The idempotency key table lives in the namespace and its checkpoint (`iwdb.keys`, layout 3, ADR 0015). With several namespaces a key is per namespace; say so, or move to a store-wide table (then it needs its own checkpointed place). Catalog changes already take keys: index creation and constraint changes in this step get them for free if they stay `CatalogChange`s. Creating and dropping a namespace should take a key too.
- **The table's size** is a format constant (10 000). If it becomes configurable, make it a logged catalog setting, so that replay stays independent of options.
- **Layout 4.** Namespaces change the directory layout (a WAL and checkpoints per namespace, or a shared WAL with a namespace per record): the next layout version, with a reader for layout 3 and a `data-dir-v4` fixture. A shared WAL also changes the WAL format (a namespace field: format 4).
- **`ReadOptions` and `min_seq`** are per namespace: a seq belongs to one namespace's history (with a shared WAL, one seq space for all).
- **Unique constraints under concurrent writers** (this step's acceptance criterion) hold by construction while every commit runs under the writer mutex; with one writer per namespace they hold per namespace, which is what a unique constraint scopes.

## Acceptance criteria

- Catalog changes survive crash and recovery (added to the crash suite).
- Unique constraints hold under concurrent writers.
