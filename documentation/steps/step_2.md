# Step 2: DB payload and catalog model

Status: todo
Milestone: M0 Foundation
Depends on: step 1

## Goal

The data types the engine stores: a payload that carries versions for optimistic concurrency, and a catalog that describes namespaces, indexes and constraints. Both save and load through the core's format.

## Tasks

- [ ] `DbRecord { attr: Attrs, meta: Attrs, version: u64 }` implementing `Attributes` (delegating to the same path rules as `Record`) and `AttrPatch`.
- [ ] A `Codec` for `Graph<DbRecord, DbRecord>` and a loader via `LoadGraph::build`, storing `version` in a reserved meta key. Round-trip proptest.
- [ ] Reserved-name policy: meta keys starting with `iwdb.` are owned by the database and rejected in user input.
- [ ] `Catalog` struct: namespaces, index definitions (attribute path), constraints (unique per label + path, required per label + path), with serde. Stored in graph meta or a sidecar file (decide in an ADR).
- [ ] Extend the canonical-state helper to include versions.

## Acceptance criteria

- `Graph<DbRecord, DbRecord>` round-trips through the binary and JSON formats, including versions and catalog.
- Filters (`Expr`), indexes and `Projection` work on `DbRecord` graphs (tests).

## Non-goals

- Mutations and `seq` (step 3), persistence of anything beyond a single file (steps 4–5).
