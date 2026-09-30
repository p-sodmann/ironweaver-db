# Step 2: DB payload and catalog model

Status: done
Milestone: M0 Foundation
Depends on: step 1

## Goal

The data types the engine stores: a payload that carries versions for optimistic concurrency, and a catalog that describes namespaces, indexes and constraints. Both save and load through the core's format.

## Tasks

- [x] `DbRecord { attr: Attrs, meta: Attrs, version: u64 }` implementing `Attributes` (delegating to the same path rules as `Record`) and `AttrPatch`.
- [x] A `Codec` for `Graph<DbRecord, DbRecord>` and a loader via `LoadGraph::build` and `LoadGraph::build_from_reader`, storing `version` in a reserved meta key. Saves are deterministic. Round-trip proptest.
- [x] Reserved-name policy: meta keys starting with `iwdb.` are owned by the database and rejected in user input.
- [x] `Catalog` struct: namespaces, index definitions (attribute path), constraints (unique per label + path, required per label + path), with serde and validation. Stored in graph meta or a sidecar file (decide in an ADR): graph meta, [ADR 0003](../adr/0003-catalog-storage.md).
- [x] Extend the canonical-state helper to include versions.

## Acceptance criteria

- `Graph<DbRecord, DbRecord>` round-trips through the binary and JSON formats, including versions and catalog.
- Filters (`Expr`), indexes and `Projection` work on `DbRecord` graphs (tests).

## Outcome

- `crates/iwdb-engine`: `DbRecord` and `DbGraph`, `reserved` (the `iwdb.` policy and the keys `iwdb.version`, `iwdb.seq`, `iwdb.catalog`), `catalog` (validated `AttrPath`, `Label` and `NamespaceName`, plus `IndexDef`, `Constraint`, `NamespaceCatalog`, `Catalog`, `apply_indexes`), `codec` (`DbCodec`, `node_record` / `edge_record`, `GraphMeta`, `to_binary` / `to_json` / `write_binary`, `from_binary` / `from_binary_reader` / `from_json`), and a typed `Error`.
- Tests: unit tests per module; `tests/db_record.rs` (proptest: `DbRecord` answers every `Attributes` method like `Record`); `tests/db_graph.rs` (round-trip and determinism proptests, byte equality with the core's `RecordCodec`, load errors via both loaders, catalog vs. `metadata.indexes`, `Expr` / indexes / `Projection` + PageRank).

### Changes to this step, and why

- The loader task now covers `build_from_reader` too. The streaming loader landed upstream in `a14149e`, and checkpoints (step 5) will stream.
- Saves are deterministic (sorted maps, no timestamp). Upstream made this possible in `a14149e`, and step 7's backups benefit from byte-identical checkpoints.
- `DbRecord::at` is a marked copy of the core's private `Record::at`. The proptest keeps it equal to the original across core bumps.
- Versions are saved as `Int`, so only versions up to `i64::MAX` can be saved (saving a larger one is an error).
- The core's JSON loader reads `-0.0` back as `0.0` (binary is exact). The random round trip leaves `-0.0` out, and a separate test pins the deviation.

## Non-goals

- Mutations and `seq` (step 3), persistence of anything beyond a single file (steps 4–5).
