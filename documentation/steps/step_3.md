# Step 3: Commit pipeline (in memory)

Status: todo
Milestone: M0 Foundation
Depends on: step 2

## Goal

All changes go through one commit function that turns high-level mutations into deterministic, validated core `Op`s. The resolved ops are exactly what the WAL will store in step 4.

## Tasks

- [ ] `Mutation` enum (the public write vocabulary): upsert/delete node, add/upsert/delete edge (by `EdgeId`, or by endpoints + type for upsert), set/remove/append attribute, add/remove label, set edge type, each with an optional `expected_version`.
- [ ] Resolver: reads the current graph and produces `Vec<Op<DbRecord, DbRecord>>` with explicit edge ids (from `next_edge_id`) and bumped versions. Upsert becomes `AddNode` or `SetNode`, append becomes `SetNodeAttr` with the full new list.
- [ ] Checks before applying: `expected_version` mismatch → conflict error; constraint checks via index lookups (unique, required); reserved-name policy.
- [ ] `apply_all`, then `flush_indexes`; assign a global `seq`; return `CommitResult { seq, edge_ids, versions }`.
- [ ] Catalog changes (create/drop index, add/drop constraint) as their own commit record type, validated against existing data.
- [ ] Model-based proptest: random mutation sequences vs. a simple reference model; failed commits leave state unchanged (canonical comparison).

## Acceptance criteria

- No code path mutates the graph outside the commit function.
- Replaying the resolved ops of a commit history onto an empty graph reproduces the same canonical state (the property the WAL relies on).
- M0 is done: update [README.md](README.md).

## Non-goals

- Durability, concurrency, idempotency keys (step 8).
