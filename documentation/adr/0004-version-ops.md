# ADR 0004: Set entity versions with an attribute op on a reserved key

Status: accepted
Date: 2026-09-30

## Context

Every node and edge has a version (`DbRecord::version`, step 2) for optimistic concurrency. The commit pipeline (step 3) resolves each transaction into core `Op`s, and those ops are the WAL record (step 4). Replaying the records must reproduce everything, versions included.

The core's attribute ops (`SetNodeAttr` / `SetEdgeAttr`) go through `AttrPatch::set_attr(key, value)`, and in step 2 `DbRecord`'s implementation changed only `attr`. So a transaction that sets one attribute had no op that also bumps the version. We can't add variants to the core's `Op`.

Options:

- **(a) Whole records.** Resolve every write to `SetNode` / `SetEdge` with the complete new record, version included.
- **(b) Attribute ops plus a version op.** Keep `SetNodeAttr` / `SetEdgeAttr` (and label and type ops), and set the version with an op that applies and undoes exactly.
  - (b1) Our own op type wrapping the core's `Op`, with an extra `SetVersion` variant. Replay and rollback would then need our own apply loop next to `apply_all`, and a failure halfway could no longer be rolled back by the core.
  - (b2) An attribute op on a reserved key: `SetNodeAttr { key: "iwdb.version", value: Some(Int(v)) }`, which `DbRecord::set_attr` turns into a version change.

What matters:

- **WAL size.** With (a), setting a flag on a node with a large payload (long lists, big strings) logs the whole payload. With (b), the record grows by one small op per written entity. Write amplification also hits replication and change streams (steps 13, 18), which ship the same ops.
- **Exact replay.** Both are exact, as long as the ops carry the final version.
- **Undo through `apply_all`.** (a) and (b2) are plain core ops, so the core rolls a failed batch back, and `apply` returns the inverse ops. (b1) is not.
- **Meaning of the log.** With (b), the log keeps the user's intent (this attribute changed, this label was added), which change streams (step 13) can pass on as is. Step 3's own task list already expects list appends to become `SetNodeAttr` with the new list.

## Decision

We use **(b2)**.

1. `DbRecord`'s `AttrPatch::set_attr` treats `reserved::VERSION_KEY` (`iwdb.version`) specially:
   - `Some(Value::Int(v))` sets `version` to `v as u64` and returns the old version as `Some(Int(old as i64))`. The bit cast makes the round trip exact for every `u64`, so the undo op restores the old version exactly.
   - Any other value (`None`, another type) changes nothing and returns the value it was given, so the undo op is the same no-op.
   - Every other key sets or removes an attribute as before.
2. Top-level attribute keys starting with `iwdb.` are reserved, like meta keys. The commit pipeline rejects them in every mutation (`Error::ReservedName`), `DbRecord`'s serde and the codec reject them on load, and the codec refuses to save them. So a user attribute can never shadow the version op. Keys inside nested dicts stay unrestricted.
3. The resolver emits, per transaction:
   - the ops of each mutation, in order: `AddNode` / `AddEdge` / `SetNode` / `SetEdge` carry the entity's final version in their record; attribute, label and type ops don't touch it;
   - then one version op for each entity the transaction wrote that still exists and whose record doesn't carry its final version yet. Nodes come first, sorted by id, then edges, sorted by id.
4. Versions in ops are always at most `i64::MAX`: the resolver fails with `Error::VersionOverflow` rather than bump past it (versions above `i64::MAX` can't be saved, see `codec.rs`).

Example: `SetAttr(a, "x", 2)` on node `a` at version 1 gives

```
SetNodeAttr { id: "a", key: "x",            value: Some(Int(2)) }
SetNodeAttr { id: "a", key: "iwdb.version", value: Some(Int(2)) }
```

## Consequences

- WAL records stay proportional to what changed, plus one small op per written entity (about 20 bytes and its id, in postcard).
- Replay is plain `Graph::apply_all` on the logged ops: no second apply path, and the core's all-or-nothing rollback covers version ops too.
- `SetNodeAttr` on `iwdb.version` means something different for `DbRecord` than for the core's `Record`. The meaning is local to our payload type and documented on its `AttrPatch` impl. Tools that read our WAL with the core's `Record` type would see an attribute named `iwdb.version`. That's acceptable: the WAL is our format (step 4), and saved files keep the version in entity meta (ADR 0003's layout), not in `attr`.
- Users can't name a top-level attribute `iwdb.*`. This matches the policy for meta keys and leaves room for future database attributes.
- The version op for a written entity is emitted even when the transaction's other ops were no-ops (a write that changes nothing still bumps the version), so every successful data commit logs at least one op.
