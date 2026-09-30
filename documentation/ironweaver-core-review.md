# Review: `ironweaver-core` 0.2 as a database foundation

Reviewed: Ironweaver `origin/main` at `a14149e` (2026-09, "0.2.0 — unreleased", merge of PR #25), crate `crates/ironweaver-core`. Previous review: `02cefab`.
Question: which changes does Ironweaver need so that a production-grade database can sit on top of it?
Verified: `crates/iwdb-engine/tests/core_smoke.rs` checks the claims below against the pinned revision. Corrections from step 1 (against `02cefab`) are marked *(step 1)*; findings from the `a14149e` bump are marked *(a14149e)*.

**Short answer: none are blocking.** The core provides the foundations a database layer needs. The seven upstream changes this review recommended all landed in PR #25 (`a14149e`). The bump review found one gap in the new visit budgets, which has a workaround, and a few smaller points. See [Findings from the `a14149e` bump](#findings-from-the-a14149e-bump).

## What the core already provides

| Need | Provided by | Where |
|---|---|---|
| No Python in the engine | Pure-Rust crate, PyO3 only in the bindings crate | `crates/ironweaver-core/Cargo.toml` |
| Custom payloads | `Graph<N, E>` is generic; algorithms read payloads through the `Attributes` trait, ops mutate them through `AttrPatch` | `graph.rs`, `record.rs`, `ops.rs` |
| Stable identity | String node ids; persistent `EdgeId` (per-graph counter, never reused, explicit ids allowed via `insert_edge`); generation-checked handles | `graph.rs` |
| Labels and types | Interned `Symbol`s, sorted label sets per node, label index (`nodes_with_label`) | `graph.rs` |
| Changes as data | `Op` (12 variants, serde, addresses nodes by id and edges by `EdgeId`); `Graph::apply` returns undo ops; `apply_all` is all-or-nothing, except that a rolled-back `AddEdge` leaves `next_edge_id()` raised *(step 1; documented upstream in `a14149e`)*. A failing undo returns `GraphError::Internal` instead of panicking *(a14149e)* | `ops.rs` |
| Typed values | `Value` (String, Int, Float, Half, Bool, None, List, Dict, Bytes, Date, DateTime); `Key`, a totally ordered hashable scalar for indexes | `value.rs`, `temporal.rs` |
| Property indexes | BTreeMap per attribute path; `find_nodes`, `find_nodes_in_range`, `index_candidates(&Expr)`; dirty tracking with `flush_indexes`; `index_paths`, `has_index`, `drop_index` | `index.rs` |
| Declarative filters | `Expr` (compare, in, exists, label, type, and/or/not), SQL-NULL semantics; serde, externally tagged, nesting limited to `MAX_EXPR_DEPTH` (100) *(a14149e)* | `expr.rs` |
| Query language | Cypher-like `Pattern::parse` + `find_matches(limit)`; variable-length paths with uniqueness modes; `Pattern` has serde and a `Display` that round-trips through `parse` *(a14149e)* | `query/` |
| Bounded traversals | `Budget { max_visited, max_results, on_limit }` for `traversal::{dfs,bfs,expand}_limited`, `query::expand_paths_limited`, `WalkPlan::run_limited`; fails with `GraphError::BudgetExceeded` or returns `Limited { value, truncated, visited }` *(a14149e; see the budget gap below)* | `budget.rs` |
| Analytics | `Projection` (CSR, `Send + Sync`) with PageRank, Leiden, betweenness, closeness, components, k-core, triangles, similarity, spanning forests, k shortest paths, FastRP, node2vec | `projection.rs`, `algo/` |
| Pathfinding | BFS, Dijkstra, A* with heuristics; batch shortest paths in parallel | `pathfinding/`, `batch.rs` |
| Timeouts | Cooperative cancellation: `cancel::run(&token, ..)`, checked in algorithms, matching, walks and traversals, also inside rayon workers | `cancel.rs` |
| File format | Format v2: JSON or postcard binary, 16-byte header, CRC32 trailer, depth limit (100), format-1 migration, compatibility promise | `format/`, `docs/format.md` |
| Deterministic saves | Attribute maps and dicts are written sorted by key (`value::sorted_entries`, `value::serialize_sorted`); `GraphWriter::with_timestamp(None)` leaves out the save time, so equal graphs with equal slot order save to equal bytes *(a14149e)* | `format/save.rs`, `value.rs` |
| Safe saves | `write_atomic`: temp file, fsync, rename, directory fsync | `format/mod.rs` |
| Custom payloads on disk | `Codec` trait + `GraphWriter` for saving, `LoadGraph::build(make_node, make_edge)` for loading from a slice, `LoadGraph::build_from_reader(reader, make_node, make_edge)` for streaming binary loads (checksum checked at the end, partial graph dropped on mismatch) *(a14149e)* | `format/save.rs`, `format/load.rs`, `format/stream.rs` |
| Index definitions on disk | Saved files list property index paths in `metadata.indexes`; loaders recreate the indexes empty and dirty, `flush_indexes` fills them *(a14149e)* | `format/save.rs`, `format/load.rs` |
| Graph-level meta | Not a field of `Graph`: it is passed to the writer (`Codec::graph_meta`, `RecordCodec { meta, .. }`, `to_binary(&g, &meta, ..)`) and returned by the loader (`from_binary` returns `(Graph, Attrs)`, `LoadGraph::meta`, `build_from_reader` returns `(Graph, LoadAttrs)`) *(step 1)* | `format/` |
| Limits | `GraphError::Capacity` instead of panics at 2^32 slots; `memory_usage()` in O(1) (O(number of indexes)), payload heap memory excluded *(a14149e)* | `graph.rs` |
| Error type | `GraphError` is `#[non_exhaustive]`; new variants `BudgetExceeded { visited, results }` and `Internal(String)` *(a14149e)*. Match with a wildcard arm | `error.rs` |
| Release hygiene | MSRV 1.85, crates.io metadata, CHANGELOG, LICENSE, CI | repo root |

## How the database covers the rest without core changes

| Database need | Built in the DB layer as |
|---|---|
| WAL record | A committed transaction's list of resolved `Op`s, plus `seq` and metadata |
| Per-entity versions (OCC) | Own payload type `DbRecord { attr, meta, version }` implementing `Attributes`, `AttrPatch` and a `Codec`. Resolved ops set versions with `SetNodeAttr` / `SetEdgeAttr` on the reserved key `iwdb.version`, which `DbRecord::set_attr` turns into an exact, undoable version change ([ADR 0004](adr/0004-version-ops.md)) |
| Upsert, merge, list append, `expected_version` | Resolved by the single writer into plain `Op`s (e.g. upsert → `AddNode` or `SetNode`, list append → `SetNodeAttr` with the new list) **before** logging. The log only has deterministic, already-validated ops. |
| Edge id assignment | Writer assigns explicit ids from `next_edge_id()` so replay produces identical ids. The writer must not assume the counter is unchanged after a failed transaction (see *Design consequences*) |
| Checkpoint position | `seq` stored in the graph-level `meta` of the saved file (the storage layer keeps it next to the in-memory graph and hands it to the codec) |
| Index persistence | Index definitions live in the catalog, which is the source of truth. Files also carry `metadata.indexes`; after loading, the database reconciles the loaded indexes with the catalog (`NamespaceCatalog::apply_indexes`, [ADR 0003](adr/0003-catalog-storage.md)) and flushes them |
| Unique / required constraints | Checked at commit using `find_nodes` on a (hash/BTree) index |
| Bounded reads | Core `Budget` for nodes and results, plus an edge-counting `edge_ok` closure for edges examined (the budget gap below), plus a cancel token for wall time |
| Memory limit per namespace | `memory_usage()` (O(1)) plus the payload sizes, which the database tracks itself on commit |
| Snapshot reads (v0) | `RwLock<Graph>` per namespace; heavy analytics build a `Projection` under a short read lock and run lock-free afterwards |
| Checkpoints without stalling writers | A background checkpointer with its own `Graph`: load last checkpoint (streaming), replay WAL segments, save. Works like a replica. |
| Timeouts | Every request runs under a `cancel::Token` cancelled by a timer. Must run on a blocking thread (`spawn_blocking`): the `Token` itself is `Send + Sync`, but `cancel::run` installs it as the *current* token in a thread-local for the duration of the call. |

## Recommended upstream changes

All seven are **done upstream**: implemented in PR #25, merged as `a14149e`, and reviewed in the bump to that revision. The issue drafts are kept in [upstream-issues.md](upstream-issues.md) for reference.

1. **Visit budgets for traversals.** ([draft](upstream-issues.md#1-visit-budgets-for-traversals-path-expansion-and-random-walks)) Done: `Budget`, `*_limited` variants, `GraphError::BudgetExceeded`, `Limited { truncated }`. Leaves a gap on edges examined; follow-up [#27](https://github.com/p-sodmann/Ironweaver/issues/27).
2. **`serde` for `Expr` and `CmpOp`, round-tripping `Display` for `Pattern`.** ([draft](upstream-issues.md#2-serde-for-expr--cmpop-and-a-round-tripping-display-for-pattern)) Done, with a depth limit (`MAX_EXPR_DEPTH`).
3. **Streaming binary loader.** ([draft](upstream-issues.md#3-streaming-binary-loader-loadgraph-from-impl-read)) Done: `LoadGraph::build_from_reader`, `format::from_binary_reader`.
4. **Deterministic attribute order when saving.** ([draft](upstream-issues.md#4-deterministic-attribute-order-when-saving)) Done: sorted maps, plus `GraphWriter::with_timestamp(None)`.
5. **Incremental memory accounting.** ([draft](upstream-issues.md#5-incremental-memory-accounting)) Done: `memory_usage()` is O(1); payloads excluded.
6. **No panic in `apply_all` rollback.** ([draft](upstream-issues.md#6-no-panic-in-apply_all-rollback)) Done: `GraphError::Internal`; both observable rollback behaviours documented.
7. *(Optional)* **Save index definitions in the file format.** ([draft](upstream-issues.md#7-optional-save-index-definitions-in-the-file-format)) Done: definitions only, in `metadata.indexes`; contents are rebuilt.

## Findings from the `a14149e` bump

- **Budget gap: `max_visited` counts nodes expanded, not edges examined.** In `traversal::bfs_limited` and `expand_limited`, a node counts once when it is entered, and then its whole edge list is scanned. The cancel token is also only polled per node. Measured: a hub with 1M parallel edges under `max_visited(1)` runs the `edge_ok` filter 1M times, even when the token is cancelled on the first edge. `dfs_limited` and `expand_paths_limited` also count nodes only, but they poll cancellation per edge, so a timeout does stop them. The smoke test `visit_budget_counts_nodes_not_edges` pins this (10k edges).
  - Workaround: count edges in the bfs `edge_ok` closure and return `GraphError::BudgetExceeded` from it once over the limit (also tested).
  - `expand_limited` takes no `edge_ok` closure, so the workaround doesn't apply. **Don't expose `expand` to remote callers until this is fixed upstream.**
  - Upstream issue: [#27](https://github.com/p-sodmann/Ironweaver/issues/27) ([draft 8](upstream-issues.md#8-edge-budget-and-per-edge-cancellation-in-bfs-and-expand)).
- **`memory_usage()` is O(1), but excludes payloads.** Slots count the payload's inline size, but not the heap memory it owns (attribute maps, strings, lists). The database must add payload sizes itself, keeping its own counter updated on commit.
- **Saved files carry index definitions** (`metadata.indexes`). `LoadGraph::build` and `build_from_reader` recreate them empty and dirty (lookups are exact but scan until `flush_indexes`). The DB catalog stays the source of truth: on load, the database drops indexes the catalog doesn't know, creates the ones missing, then flushes ([ADR 0003](adr/0003-catalog-storage.md)).
- **Deterministic saves.** Custom codecs must sort maps themselves; `value::sorted_entries` and `value::serialize_sorted` are public for that. `Value`'s own `Serialize` is the tagged encoding and sorts nested dicts, so a codec can write `serialize_sorted(&attrs, s)` for a map (no half-precision floats; `format::tagged` has the per-variant encoders if needed).
- **Nits.**
  - New `.expect("live nodes are indexed")` in `Graph::remove_node` / `rename_node`, reached from `apply`. It is believed unreachable, but it is on the apply path that PR #25 otherwise made panic-free. Upstream issue: [#28](https://github.com/p-sodmann/Ironweaver/issues/28).
  - `Expr` serde errors (depth limit) are raised with `serde::ser::Error::custom`, not `format::ser_error`, so under postcard the message is lost (postcard drops custom messages). JSON keeps it. Pinned in `expr_and_pattern_round_trip`. Upstream issue: [#29](https://github.com/p-sodmann/Ironweaver/issues/29).
  - The JSON loader reads `Float(-0.0)` back as `Float(0.0)`: the saver writes `-0.0`, the sign is lost on parsing. The binary format keeps it. Harmless for checkpoints (binary), a small inexactness in JSON export. Found in step 2 and pinned in `tests/db_graph.rs` (`json_loses_the_sign_of_negative_zero`). Cause: sonic-rs 0.5.10 parses `-0.0` as `+0.0`. Upstream issue: [#26](https://github.com/p-sodmann/Ironweaver/issues/26) ([draft 9](upstream-issues.md#9-json-loader-reads--00-back-as-00)).
  - `Value`'s serde and the file format count nesting depth differently for empty containers: 100 nested lists with an empty innermost one (depth 100) save and load in the file format, but `Value`'s serde (so postcard, JSON, `Op` and our codec's attribute encoding) rejects them. The commit pipeline counts an empty container as holding a scalar (`mutation::MAX_VALUE_DEPTH`), so it never logs such a value; a foreign file with one loads but can't be checkpointed again. Found in step 3 and pinned in `value_serde_rejects_empty_containers_at_the_depth_limit` (`core_smoke.rs`). Upstream issue: [#31](https://github.com/p-sodmann/Ironweaver/issues/31) ([draft 13](upstream-issues.md#13-value-serde-rejects-empty-containers-at-the-depth-limit-that-the-file-format-accepts)).
  - `Record::at` (the attribute-path lookup behind `Attributes`) is still private, so payloads that want the same path rules copy it. `iwdb-engine` does so for `DbRecord` (step 2), with a proptest in `tests/db_record.rs` that checks every `Attributes` method against `Record`. Upstream issue: [#30](https://github.com/p-sodmann/Ironweaver/issues/30) (a public `record::lookup(&Attrs, path)`).

## Design consequences for the database

- **A failed `apply_all` can advance the edge id counter.** Rollback restores ids, labels, types and payloads, but `next_edge_id()` stays above any explicit id the failed batch used *(step 1; now documented upstream)*. Ids are still never reused, and the WAL carries explicit ids, so replay is exact; but the counter is not comparable between a primary and a replayed graph, and the canonical-state helper leaves it out.
- **`GraphError::Internal` from `apply_all` means the graph may be inconsistent.** The database fails the transaction and reloads the namespace from checkpoint + WAL instead of continuing on it *(a14149e)*.
- **Iteration order is not part of the contract.** Slots are reused and save/load compacts them, so order after recovery differs from order before. Database results that need an order sort by id; recovery tests compare a canonical form of the state.
- **The write path owns the graph.** Users of the database never get `node_mut` / `edge_mut`; all changes go through ops, so the WAL, indexes and versions stay consistent.
- **Panics in the core are fatal for the process**, not for the data: the database treats a panic during commit as a crash and relies on recovery from checkpoint + WAL.
- **Pin a git revision of `ironweaver-core`** until 0.2.0 is on crates.io; bump deliberately and run the full crash and compatibility suites on every bump.

## Notes (not upstream requests yet)

- `format::to_binary` / `from_binary` doc comments say "bincode"; format 2 is postcard (bincode is only used to read format-1 files). Documentation nit, still present at `a14149e`. Upstream issue: [#30](https://github.com/p-sodmann/Ironweaver/issues/30).
- `bincode` 1.x is flagged unmaintained (RUSTSEC-2025-0141) and reaches us through the core's format-1 reader. It is still an unconditional dependency at `a14149e`, so `deny.toml` keeps ignoring it, with that reason; revisit on each core bump. Upstream issue: [#30](https://github.com/p-sodmann/Ironweaver/issues/30) (bincode behind a `format-v1` feature).
