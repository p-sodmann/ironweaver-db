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

## Acceptance criteria

- Catalog changes survive crash and recovery (added to the crash suite).
- Unique constraints hold under concurrent writers.
