# Review: `ironweaver-core` 0.2 as a database foundation

Reviewed: Ironweaver `origin/main` at `02cefab` (2026-09, "0.2.0 — unreleased"), crate `crates/ironweaver-core`.
Question: which changes does Ironweaver need so that a production-grade database can sit on top of it?
Verified: step 1 checks the claims below against `02cefab` with `crates/iwdb-engine/tests/core_smoke.rs`; corrections from that check are marked *(step 1)*.

**Short answer: none are blocking.** The rewrite already provides the foundations a database layer needs. Seven small upstream changes would make the database layer simpler or cheaper. They are listed below and none of them has to land before we start.

## What the core already provides

| Need | Provided by | Where |
|---|---|---|
| No Python in the engine | Pure-Rust crate, PyO3 only in the bindings crate | `crates/ironweaver-core/Cargo.toml` |
| Custom payloads | `Graph<N, E>` is generic; algorithms read payloads through the `Attributes` trait, ops mutate them through `AttrPatch` | `graph.rs`, `record.rs`, `ops.rs` |
| Stable identity | String node ids; persistent `EdgeId` (per-graph counter, never reused, explicit ids allowed via `insert_edge`); generation-checked handles | `graph.rs` |
| Labels and types | Interned `Symbol`s, sorted label sets per node, label index (`nodes_with_label`) | `graph.rs` |
| Changes as data | `Op` (12 variants, serde, addresses nodes by id and edges by `EdgeId`); `Graph::apply` returns undo ops; `apply_all` is all-or-nothing, except that a rolled-back `AddEdge` leaves `next_edge_id()` raised *(step 1)* | `ops.rs` |
| Typed values | `Value` (String, Int, Float, Half, Bool, None, List, Dict, Bytes, Date, DateTime); `Key`, a totally ordered hashable scalar for indexes | `value.rs`, `temporal.rs` |
| Property indexes | BTreeMap per attribute path; `find_nodes`, `find_nodes_in_range`, `index_candidates(&Expr)`; dirty tracking with `flush_indexes` | `index.rs` |
| Declarative filters | `Expr` (compare, in, exists, label, type, and/or/not), SQL-NULL semantics | `expr.rs` |
| Query language | Cypher-like `Pattern::parse` + `find_matches(limit)`; variable-length paths with uniqueness modes | `query/` |
| Analytics | `Projection` (CSR, `Send + Sync`) with PageRank, Leiden, betweenness, closeness, components, k-core, triangles, similarity, spanning forests, k shortest paths, FastRP, node2vec | `projection.rs`, `algo/` |
| Pathfinding | BFS, Dijkstra, A* with heuristics; batch shortest paths in parallel | `pathfinding/`, `batch.rs` |
| Timeouts | Cooperative cancellation: `cancel::run(&token, ..)`, checked in algorithms, matching, walks and traversals, also inside rayon workers | `cancel.rs` |
| File format | Format v2: JSON or postcard binary, 16-byte header, CRC32 trailer, depth limit (100), format-1 migration, compatibility promise | `format/`, `docs/format.md` |
| Safe saves | `write_atomic`: temp file, fsync, rename, directory fsync | `format/mod.rs` |
| Custom payloads on disk | `Codec` trait + `GraphWriter` for saving, `LoadGraph::build(make_node, make_edge)` for loading | `format/save.rs`, `format/load.rs` |
| Graph-level meta | Not a field of `Graph`: it is passed to the writer (`Codec::graph_meta`, `RecordCodec { meta, .. }`, `to_binary(&g, &meta, ..)`) and returned by the loader (`from_binary` returns `(Graph, Attrs)`, `LoadGraph::meta`) *(step 1)* | `format/` |
| Limits | `GraphError::Capacity` instead of panics at 2^32 slots; `memory_usage()` | `graph.rs` |
| Release hygiene | MSRV 1.85, crates.io metadata, CHANGELOG, LICENSE, CI | repo root |

## How the database covers the rest without core changes

| Database need | Built in the DB layer as |
|---|---|
| WAL record | A committed transaction's list of resolved `Op`s, plus `seq` and metadata |
| Per-entity versions (OCC) | Own payload type `DbRecord { attr, meta, version }` implementing `Attributes`, `AttrPatch` and a `Codec` |
| Upsert, merge, list append, `expected_version` | Resolved by the single writer into plain `Op`s (e.g. upsert → `AddNode` or `SetNode`, list append → `SetNodeAttr` with the new list) **before** logging. The log only has deterministic, already-validated ops. |
| Edge id assignment | Writer assigns explicit ids from `next_edge_id()` so replay produces identical ids. The writer must not assume the counter is unchanged after a failed transaction (see *Design consequences*) |
| Checkpoint position | `seq` stored in the graph-level `meta` of the saved file (the storage layer keeps it next to the in-memory graph and hands it to the codec) |
| Index persistence | Index definitions live in the catalog; indexes are rebuilt with `create_index` on load |
| Unique / required constraints | Checked at commit using `find_nodes` on a (hash/BTree) index |
| Snapshot reads (v0) | `RwLock<Graph>` per namespace; heavy analytics build a `Projection` under a short read lock and run lock-free afterwards |
| Checkpoints without stalling writers | A background checkpointer with its own `Graph`: load last checkpoint, replay WAL segments, save. Works like a replica. |
| Timeouts | Every request runs under a `cancel::Token` cancelled by a timer. Must run on a blocking thread (`spawn_blocking`): the `Token` itself is `Send + Sync`, but `cancel::run` installs it as the *current* token in a thread-local for the duration of the call. |

## Recommended upstream changes

Ordered by value to the database. Each is a small, independent PR to Ironweaver. Issue drafts, one per item, are in [upstream-issues.md](upstream-issues.md) (drafted, pending review; not filed yet).

1. **Visit budgets for traversals.** ([draft](upstream-issues.md#1-visit-budgets-for-traversals-path-expansion-and-random-walks)) `traversal::{bfs, dfs, expand}`, `query::expand_paths` and random walks have depth limits but no `max_visited` / `max_results`. The workaround is counting inside the `edge_ok` closure and returning an error, plus a cancel token for wall time. A first-class budget argument (returning a typed "budget exceeded" error, or partial results with a flag) makes "every read is bounded" enforceable and cheap.
2. **`serde` for `Expr` and `CmpOp`** ([draft](upstream-issues.md#2-serde-for-expr--cmpop-and-a-round-tripping-display-for-pattern)), and a `Display` impl for `Pattern` that round-trips through `Pattern::parse`. Filters can then travel over REST/JSON and be stored in the catalog without the database keeping a parallel AST.
3. **Streaming binary loader** ([draft](upstream-issues.md#3-streaming-binary-loader-loadgraph-from-impl-read)) (`LoadGraph` from `impl Read`). `from_binary_slice` needs the whole file in memory, so recovering a large checkpoint briefly needs about twice the graph's memory.
4. **Deterministic attribute order when saving.** ([draft](upstream-issues.md#4-deterministic-attribute-order-when-saving)) `Record.attr` and `Value::Dict` are `HashMap`s, so equal graphs can produce different bytes. Sorting keys in `GraphWriter` gives byte-identical checkpoints, which helps backup deduplication and compatibility fixtures. Not needed for correctness.
5. **Incremental memory accounting.** ([draft](upstream-issues.md#5-incremental-memory-accounting)) `memory_usage()` walks every node (O(n)). Enforcing a memory limit on every commit needs a counter maintained on insert/remove.
6. **No panic in `apply_all` rollback.** ([draft](upstream-issues.md#6-no-panic-in-apply_all-rollback)) Rollback uses `expect("undoing an applied op succeeds")`. A bug there would panic while the database holds its write lock. Returning an internal error lets the database fail the transaction and restart cleanly. Also document that rollback can change adjacency order (re-added edges go last) and does not lower the edge id counter *(step 1)*.
7. *(Optional)* **Save index definitions in the file format** ([draft](upstream-issues.md#7-optional-save-index-definitions-in-the-file-format)), so reopening doesn't need a full rebuild. Only if benchmarks show rebuild time matters.

## Design consequences for the database

- **A failed `apply_all` can advance the edge id counter.** Rollback restores ids, labels, types and payloads, but `next_edge_id()` stays above any explicit id the failed batch used *(step 1)*. Ids are still never reused, and the WAL carries explicit ids, so replay is exact; but the counter is not comparable between a primary and a replayed graph, and the canonical-state helper leaves it out.
- **Iteration order is not part of the contract.** Slots are reused and save/load compacts them, so order after recovery differs from order before. Database results that need an order sort by id; recovery tests compare a canonical form of the state.
- **The write path owns the graph.** Users of the database never get `node_mut` / `edge_mut`; all changes go through ops, so the WAL, indexes and versions stay consistent.
- **Panics in the core are fatal for the process**, not for the data: the database treats a panic during commit as a crash and relies on recovery from checkpoint + WAL.
- **Pin a git revision of `ironweaver-core`** until 0.2.0 is on crates.io; bump deliberately and run the full crash and compatibility suites on every bump.

## Notes from step 1 (not upstream requests yet)

- `format::to_binary` / `from_binary` doc comments say "bincode"; format 2 is postcard (bincode is only used to read format-1 files). Documentation nit.
- `bincode` 1.x is flagged unmaintained (RUSTSEC-2025-0141) and reaches us through the core's format-1 reader. `deny.toml` ignores it with that reason; revisit on each core bump. Worth raising upstream (e.g. behind a `format-v1` feature) if it stays.
