# ADR 0019: Online index builds

Status: accepted
Date: 2026-10-01

## Context

Step 8 left `CreateIndex` and `AddConstraint(unique)` as ordinary catalog commits: the graph's index was built while the record was applied, under the namespace's **write** lock, which blocks every reader and the next commit's apply for the whole build (O(n log n)). Step 9 asks that an index build not hold the namespace write lock or the writer mutex for the whole build.

## What the core allows

The core builds an index only through `&mut Graph` (`create_index`: reads every payload, inserts every key), or from keys the caller read (`create_index_with_keys`: the keys come from outside, the insertion into the index still happens inside the `&mut` call). It has no way to build an index off the graph and install it in O(1), and a node whose index entry is stale (`dirty`) makes *every* index lookup scan all dirty nodes, so building incrementally through the dirty set is not an option (a half-built index would make every lookup, including the unique constraints' checks, O(backlog)). The finding is filed upstream (`documentation/upstream-issues.md`).

## Decision

An index the graph lacks is built in two phases:

1. **Scan, without the writer mutex and without holding the namespace lock for long**: the handles of all nodes are listed (one short read lock, O(n) handles), then the keys and versions are read `BUILD_CHUNK` (8192) nodes per read-lock hold. Commits and reads go on between chunks (a commit waits for its apply only for the length of one chunk).
2. **Install, under the writer mutex**: the commit is prepared (validated: for a constraint, the existing data is checked, a scan under the read lock), appended to the WAL, and applied with the pre-read keys: every node whose handle is still live and whose *version* is unchanged keeps its key; the others (changed or added since the scan) are not given a key, which marks them dirty, and the flush that follows the apply re-reads exactly those. The insertion of the keys is what the write lock is held for.

The catalog change is logged only after the scan, so the log never contains a build in progress; a crash during the scan loses nothing (no record). Indexes are rebuilt on recovery from the catalog, as before. `status` lists indexes with state `ready` and, while a scan runs, `building (scanned/total)`; the index isn't in the catalog until it is installed.

Adding a **constraint** validates the existing data first, under the writer mutex and the read lock (readers go on, commits wait): the scan is over the nodes with the constraint's label. Only the index part of a unique constraint is online.

## Consequences

The write-lock hold of an index build shrinks from "read every payload and insert every key" to "insert every key", measured in `crates/iwdb/tests/latency.rs` (see the Results of step 9). It is still O(n log n) until the core can build an index off the graph; the upstream issue asks for that. A build that races with heavy writes to the same nodes re-reads those nodes in the flush (correct, slower).

## Measurement

`cargo test --release -p iwdb --test latency latency_during_an_online_index_build -- --ignored --nocapture`, 500 000 nodes with three attributes each, fsync off, one writer committing one-node transactions in a loop while the build runs (Apple silicon laptop; the numbers are for comparison, not a promise):

| | whole build | longest commit stall | commits outside a build |
|---|---|---|---|
| index, in one lock hold (engine alone) | 183 ms | 183 ms (the hold) | p99 5 µs, max 221 µs |
| index, online (this ADR) | 156 ms | 105 ms | |
| unique constraint (validation holds the writer mutex) | 406 ms | 345 ms | |

The scan phase is about 50 ms of the 156 and no longer blocks anyone; the install (inserting 500 000 keys under the write lock) is about 100 ms, so the **hold shrinks by roughly 40 %, not to near zero**. That is the honest limit of what the core allows today: only an off-graph build with an O(1) install (the upstream issue) removes the rest. A unique constraint still stalls writers for its whole validation, as the Decision says. Both stalls grow linearly with the number of nodes carrying the label or attribute.
