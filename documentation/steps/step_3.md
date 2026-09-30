# Step 3: Commit pipeline (in memory)

Status: done
Milestone: M0 Foundation
Depends on: step 2

## Goal

All changes go through one commit function that turns high-level mutations into deterministic, validated core `Op`s. The resolved ops are exactly what the WAL will store in step 4.

## Tasks

- [x] `Mutation` enum (the public write vocabulary): upsert/delete node, add/upsert/delete edge (by `EdgeId`, or by endpoints + type for upsert), set/remove/append attribute, add/remove label, set edge type, each with an optional `expected_version`.
- [x] Resolver: reads the current graph and produces `Vec<Op<DbRecord, DbRecord>>` with explicit edge ids (from `next_edge_id`) and bumped versions. Upsert becomes `AddNode` or `SetNode`, append becomes `SetNodeAttr` with the full new list.
- [x] Checks before applying: `expected_version` mismatch → conflict error; constraint checks via index lookups (unique, required); reserved-name policy.
- [x] Note from step 2: `AttrPatch` (used by `SetNodeAttr` / `SetEdgeAttr`) changes only `DbRecord::attr`, never `version`. So a version bump needs `SetNode` / `SetEdge` with the whole record, or a cheaper mechanism decided here (with an ADR). The resolved ops must carry the new version either way, so that replay reproduces it. Decided in [ADR 0004](../adr/0004-version-ops.md): a `SetNodeAttr` / `SetEdgeAttr` on the reserved key `iwdb.version`.
- [x] `apply_all`, then `flush_indexes`; assign a global `seq`; return `CommitResult { seq, edge_ids, versions }`.
- [x] Catalog changes (create/drop index, add/drop constraint) as their own commit record type, validated against existing data.
- [x] Model-based proptest: random mutation sequences vs. a simple reference model; failed commits leave state unchanged (canonical comparison).

## Acceptance criteria

- No code path mutates the graph outside the commit function.
- Replaying the resolved ops of a commit history onto an empty graph reproduces the same canonical state (the property the WAL relies on).
- M0 is done: update [README.md](README.md).

## Outcome

- `crates/iwdb-engine`:
  - `mutation`: `Mutation` (upsert/delete node, add/upsert/delete edge, set/remove/append attribute, add/remove label, set edge type), `Target`, `EdgeKey`, `CommitRecord { seq, change: Change::Data(ops) | Change::Catalog(CatalogChange) }` with serde, `CommitResult { seq, edge_ids, versions }`, `MAX_VALUE_DEPTH`. The module docs define the version semantics.
  - `resolve` (private): resolves a transaction against a transaction-local overlay of the graph and checks it (versions, existence, reserved keys, value depth, unique and required constraints on the state after the whole transaction) without touching the graph.
  - `Namespace`: owns the graph (only `&DbGraph` is lent out; a `compile_fail` doctest checks it), the catalog and `seq`. `prepare` / `prepare_catalog` → `apply`, `commit` / `commit_catalog`, `replay` for log records, and poisoning.
  - `DbRecord::set_attr` sets the version on `iwdb.version` (ADR 0004). Top-level attribute keys starting with `iwdb.` are reserved: the pipeline, `DbRecord`'s serde and the codec reject them.
  - New `Error` variants: `Conflict`, `NotFound`, `AmbiguousEdge`, `NoMatchingEdge`, `NotAList`, `ValueTooDeep`, `VersionOverflow`, `ConstraintViolation`, `EmptyTransaction`, `IndexExists`, `NoSuchIndex`, `UnindexablePath`, `ConstraintExists`, `NoSuchConstraint`, `EdgeIdsExhausted`, `SeqExhausted`, `OutOfOrder`, `ApplyFailed`, `Poisoned`.
- Version semantics: a commit that writes an entity sets its version to its version before the commit plus 1, once per commit. An entity that didn't exist counts as version 0, so new entities start at 1. Every successful mutation that addresses an entity writes it, even if it changes nothing. `expected_version` is compared with the version before the commit, and `Some(0)` means "must not exist". An entity deleted and re-created in a later commit starts again at 1 (no tombstones; documented ABA). Deleted and re-created within one commit, it gets the old version plus 1. Versions above `i64::MAX` are `VersionOverflow`, never a wrap.
- `seq` starts at 1 and grows by one per successful commit (data or catalog). Failed commits use none.
- Tests: `tests/commit.rs` (version semantics, edges and edge ids, failing transactions, unique and required constraints, catalog changes, reserved keys in every mutation, version overflow, value depth, record shape, codec round trip); `tests/commit_model.rs` (model-based proptest over random histories with failing commits, plus the replay property through postcard and through plain `apply_all`); unit tests for the version op, poisoning and prepared-commit ordering. The model test was checked against five injected resolver bugs, all of which it caught.
- Core finding: `Value`'s serde rejects empty containers at depth 100 that the file format accepts. Pinned in `core_smoke.rs`, filed as [#31](https://github.com/p-sodmann/Ironweaver/issues/31), and added to the [upstream check](upstream-check.md).

### Changes to this step, and why

- **Check before applying, not apply and roll back.** Step 4 appends to the WAL between validation and apply, so validation can't depend on applying. The resolver therefore checks the state after the whole transaction on an overlay. This also avoids the rollback caveats (adjacency order, a raised edge id counter).
- **`prepare` / `apply` split and `replay`.** Step 4 needs a validated record before applying it, and the replay acceptance criterion needs a way to apply records. `replay` applies a record without validating it again. Step 5's recovery will use it.
- **Any failure to apply a validated record poisons the namespace**, not only `GraphError::Internal`. A validated record that doesn't apply means that memory and log disagree, so the namespace is reloaded either way.
- **Reserved attribute keys and the version op** (ADR 0004): the cheaper mechanism the step 2 note asked for.
- **Value depth limit.** Values nested too deep for the log's encoding would make a logged record unreadable, so mutations are checked against `MAX_VALUE_DEPTH`, one level stricter for empty containers (upstream #31).
- **Mutation details:** `AddEdge` has no `expected_version` (it always adds). An upsert by `EdgeId` never creates an edge (ids are assigned by the database). An upsert by endpoints matches the type exactly and fails on more than one match. Empty transactions are rejected (`EmptyTransaction`) and use no `seq`.
- **One namespace, one `seq` counter.** `seq` lives in the namespace for now. Whether it stays per namespace or becomes store-wide is decided with the WAL layout in step 9.

## Non-goals

- Durability, concurrency, idempotency keys (step 8).
