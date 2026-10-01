# ADR 0015: Idempotency keys: a key table in the namespace, WAL format 3 and data-dir layout 3

Status: accepted
Date: 2026-10-01

## Context

A commit that fails with `Io`, times out, or whose process dies before it answers has an unknown outcome (ADR 0005): its record may be in the log and recovered. A client that retries blindly may apply it twice; one that doesn't may lose it. Step 8 asks for idempotency keys: a retried commit with the same key returns the original result and applies once, also across restarts, checkpoints, backups and restores (and kill -9).

Questions:

- Where the keys live so that every path that rebuilds a namespace (recovery, the checkpointer, restore, verify, a future replica) rebuilds them too.
- How many to keep, and what is evicted.
- What a retry with the same key but another request returns.
- What happens to keys across a restore, which starts a new history (ADR 0009).
- Whether to return the commit time (ADR 0010 left it for this step).

## Decision

### The key table is namespace state

`Namespace` holds a `KeyTable` next to its catalog: the last `KEY_TABLE_CAPACITY` (10 000) keyed commits, key → (fingerprint, original `CommitResult`). It changes only when a record is applied, so it is a pure function of the records, like the graph: every path that replays the log gets the same table, and comparing namespaces (`invariants::compare`, used by verify and the tests) compares tables too.

- **The record carries the key and the result** (`CommitRecord::keyed`: key, fingerprint, edge ids, versions). Replay rebuilds the entry from the record and the frame's commit time without resolving anything. The edge ids, in mutation order, can't be derived from the resolved ops, so they are stored; storing the versions as well keeps replay exact without a second derivation. Only keyed records pay for it.
- **Checkpoints save the table** in graph meta as `iwdb.keys` (a JSON string, entries by seq). A checkpoint holds the state at its seq, keys included; the WAL after it holds the rest. That is what makes keys survive WAL cuts, backups (which copy checkpoints and WAL) and restores (which write one checkpoint and no WAL).
- **Lookup first.** Preparing a keyed commit looks the key up before validating. A duplicate is found even if the request would fail now (an insert with `expected_version: Some(0)` that succeeded) and even if the store is read-only: that commit was applied, so its result stands.

### Formats

- **WAL format 3**: the payload of every record is `postcard((Option<Keyed>, body))`, so a record without a key costs one byte. The frame (33-byte header, CRC) is unchanged. The reader reads formats 1, 2 and 3 per segment; a writer starts a new segment in format 3, so an older log continues without rewriting. A new record kind would have worked too, but would double the kinds (data and catalog, each with and without a key) for the same information. Fixture `wal-v3`.
- **Data-dir layout 3**: checkpoints have the `iwdb.keys` graph meta key, and the marker says version 3 (otherwise unchanged). A step 7 store would treat `iwdb.keys` as an unknown reserved key and skip every checkpoint as damaged, so the layout version must change (data-dir.md, "Versioning"). Opening a layout 2 directory rewrites its marker as layout 3 with the same history id, as layout 1 was upgraded in step 7; its checkpoints have no `iwdb.keys` and load with an empty table. Fixture `data-dir-v3`, with keyed commits before and after its checkpoint.

### Semantics

- **Same key, same request** (equal fingerprints): the original result with `deduplicated: true`; nothing is logged or applied. The fingerprint is the CRC32C of a kind byte (1 data, 2 catalog) and the postcard encoding of the request, with attribute and meta maps sorted (equal mutations encode equally whatever the maps' insertion order). It is a check against client bugs, not a security mechanism: two different requests collide with probability about 2⁻³².
- **Same key, another request**: `IdempotencyKeyReused { key, seq }`, nothing changes. A key names one request (like Stripe's idempotency keys); silently returning another request's result would hide a client bug.
- **Size and eviction**: at most 10 000 keyed commits; a new one evicts the entry with the lowest seq. Deterministic, so replay gives the same table. A retry finds its key as long as fewer than 10 000 keyed commits came after the original. The size is a constant of the format: a configurable size would make replay depend on configuration. Step 9 may make it a logged catalog setting. Memory: the key (at most 255 bytes) and the result (proportional to the transaction) per entry; a huge keyed transaction keeps a large result.
- **Keys**: 1 to 255 bytes of UTF-8, chosen by the client (a UUID).
- **Catalog changes** take keys like data commits.
- **Across a restore**: a restore to seq `N` keeps the entries of the commits up to `N` (they are in its checkpoint) and none after. A retry of a commit at or below `N` returns its result; a retry of a commit after `N`, which the restored history doesn't contain, applies it. So the table always describes exactly the commits in the store's history. A client that wants to know whether it talks to the same history compares history ids (ADR 0016).
- **Durability**: an entry is as durable as its commit. Under `group`, an OS crash can lose an acknowledged commit and its key together; a retry then applies it again, which is correct.

### Commit times in results

`Wal::append` returns the record's commit time, and `CommitResult::time` carries it (`None` without a log, and for format 1 records). The key table keeps it, so a retried request returns the original commit's time. `CommitTime` moved from iwdb-storage to iwdb-engine for this (iwdb-storage re-exports it).

## Consequences

- A client that retries with the same key after any unknown outcome applies its commit exactly once, as long as the retry comes within 10 000 keyed commits. Tested in the engine (dedup, reuse, eviction, replay and checkpoint round trip), in the store (restart, checkpoint, backup + restore, failpoints) and by the kill -9 harness, whose children retry keyed commits of the killed child.
- `verify` compares the key table of every checkpoint with the WAL replayed to its seq, and checks each table's bounds; a damaged table fails to load.
- Step 7 versions can't open a layout 3 directory (`UnsupportedLayout`) or read format 3 segments; opening a step 7 directory with this version upgrades it.
