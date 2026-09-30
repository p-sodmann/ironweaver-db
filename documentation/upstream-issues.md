# Upstream issue drafts for Ironweaver

Status: **drafted, pending review.** None of these are filed yet. After review, file each one in [p-sodmann/Ironweaver](https://github.com/p-sodmann/Ironweaver/issues) and replace "not filed" with the issue link, here and in the [core review](ironweaver-core-review.md#recommended-upstream-changes).

One draft per item in "Recommended upstream changes" of the core review, in the same order. All were checked against `ironweaver-core` at `02cefab`. Titles are ready to paste; the text below each title is the issue body.

| # | Title | Issue |
|---|---|---|
| 1 | [Visit budgets for traversals, path expansion and random walks](#1-visit-budgets-for-traversals-path-expansion-and-random-walks) | not filed |
| 2 | [`serde` for `Expr` / `CmpOp`, and a round-tripping `Display` for `Pattern`](#2-serde-for-expr--cmpop-and-a-round-tripping-display-for-pattern) | not filed |
| 3 | [Streaming binary loader (`LoadGraph` from `impl Read`)](#3-streaming-binary-loader-loadgraph-from-impl-read) | not filed |
| 4 | [Deterministic attribute order when saving](#4-deterministic-attribute-order-when-saving) | not filed |
| 5 | [Incremental memory accounting](#5-incremental-memory-accounting) | not filed |
| 6 | [No panic in `apply_all` rollback](#6-no-panic-in-apply_all-rollback) | not filed |
| 7 | [(Optional) Save index definitions in the file format](#7-optional-save-index-definitions-in-the-file-format) | not filed |

---

## 1. Visit budgets for traversals, path expansion and random walks

**Problem**

`traversal::{dfs, bfs, expand}`, `query::expand_paths` and the random walks limit depth but not work. On a graph with high-degree nodes, a depth-2 or depth-3 traversal can still visit millions of nodes and return a result of the same size. The only way to bound it today is to count inside the `edge_ok` closure and return an error from it, plus a `cancel::Token` for wall time. That works, but every caller has to reimplement it, the count is of edges examined rather than nodes visited or results produced, and there is no way to get "the first N results, and a flag that there were more".

**Proposal**

Add an optional budget to these functions (or a variant that takes one), for example:

```rust
pub struct Budget {
    pub max_visited: Option<usize>, // nodes entered
    pub max_results: Option<usize>, // nodes / paths returned
}
```

When a limit is hit, either return a typed error (`GraphError::BudgetExceeded { visited, results }`) or return the partial result with a `truncated: bool`. The caller picks which through the API (two entry points, or a field on `Budget`). Checks are a counter compare per visit, so the cost is negligible.

**Why the database needs it**

Ironweaver DB promises that every read that crosses a network boundary is bounded (max results, max visited, timeout). With a first-class budget, the query layer passes the per-request limits straight through, reports "limit reached" consistently across embedded, gRPC and REST, and doesn't carry closure-based workarounds for each traversal kind.

---

## 2. `serde` for `Expr` / `CmpOp`, and a round-tripping `Display` for `Pattern`

**Problem**

`Expr` and `CmpOp` (`expr.rs`) derive `Clone, Debug, PartialEq` but not `Serialize` / `Deserialize`. `Pattern` (`query/pattern.rs`) can be parsed from text with `Pattern::parse` but has no way back to text. Anything that wants to send a filter over the network, or store one, has to define its own mirror of the AST and keep it in sync with the core.

**Proposal**

- Derive `Serialize` / `Deserialize` for `Expr` and `CmpOp` (behind the existing `serde` dependency; no new dependency). The existing `Expr::depth` can back a depth limit on deserialization, like the file format's `MAX_DEPTH`.
- Implement `Display` for `Pattern` so that `Pattern::parse(&p.to_string())` gives back an equal pattern, with a property test for the round trip.

**Why the database needs it**

Remote queries in Ironweaver DB use `Expr` filters and `match` patterns instead of code ("no lambdas over the wire"). Filters travel as JSON over REST and as bytes over gRPC, and index or constraint definitions in the catalog may contain them. With serde on `Expr` the database uses the core's type directly, and with a round-tripping `Display` it can log, `EXPLAIN` and store patterns as text.

---

## 3. Streaming binary loader (`LoadGraph` from `impl Read`)

**Problem**

`LoadGraph::from_binary_slice` (and so `format::from_binary`) needs the whole file in a byte slice, because the loaded structs borrow strings from it. Loading a large file therefore needs the file's bytes and the built graph in memory at the same time, roughly twice the graph's size at peak.

**Proposal**

Add a loader that reads from `impl Read` (for example `LoadGraph::build_from_reader(reader, make_node, make_edge)`), decoding with owned strings and building the graph as it goes. The CRC32 can be computed while reading and checked at the trailer; since the trailer comes last, the loader must discard the partly built graph if the checksum or length doesn't match, and never return it. The JSON path can stay slice-based.

**Why the database needs it**

Recovery loads the newest checkpoint and then replays the WAL; the background checkpointer does the same on its own copy of the graph. Both happen while the server is also holding the live graph. A streaming loader removes the 2x peak, which decides how large a graph fits on a given machine.

---

## 4. Deterministic attribute order when saving

**Problem**

`Record.attr`, `Record.meta` and `Value::Dict` are `HashMap`s, and `RecordCodec` serializes them in map iteration order (`TaggedMap` in `format/save.rs`). Two equal graphs, or the same graph saved twice in two processes, can produce different bytes.

**Proposal**

Sort map keys when saving (in `TaggedMap`, and so for graph meta too). Optionally add a flag if the sort's cost matters for very large saves. Document in `docs/format.md` that saves are byte-identical for equal graphs with equal slot order.

**Why the database needs it**

Not needed for correctness. Byte-identical checkpoints make content-addressed backup deduplication work, keep compatibility fixtures stable across runs, and let `verify` compare checkpoints by hash.

---

## 5. Incremental memory accounting

**Problem**

`Graph::memory_usage()` walks every node (id, labels, adjacency lists) and the indexes, so it is O(n). It is fine for occasional reporting but too slow to call on every write.

**Proposal**

Maintain the variable part of the estimate as a counter updated on insert/remove of nodes, edges, labels and index entries, and have `memory_usage()` return it in O(1) (plus the fixed-size parts). Payload sizes can stay out of it (the caller knows its payload), or be reported through an optional trait method.

**Why the database needs it**

Ironweaver DB enforces a per-namespace memory limit so that one tenant can't take the whole server down. The check has to run on every commit, inside the single writer, so it must be O(1).

---

## 6. No panic in `apply_all` rollback

**Problem**

On failure, `Graph::apply_all` undoes the ops already applied with `self.apply(op).expect("undoing an applied op succeeds")` (`ops.rs`). If a bug ever made an undo op fail, the process panics in the middle of a rollback, with the graph half-restored. `apply` itself also has several `expect("live")` calls on paths that are believed unreachable.

Two related behaviours should be documented, because callers can observe them:

- rollback can change adjacency order (re-added edges go last), which the module comment already says;
- rollback does **not** lower the edge id counter: if the failed batch contained an `AddEdge` with an explicit id, `next_edge_id()` stays raised afterwards. Ids are still never reused, so this is harmless, but the doc says "a failing op leaves the graph unchanged" and the counter is observable through `next_edge_id()` (confirmed by our smoke test at `02cefab`).

**Proposal**

Return an error instead of panicking, for example `GraphError::Internal(String)` from `apply_all` when an undo fails, marking the graph as possibly inconsistent. Document both behaviours above on `apply_all`.

**Why the database needs it**

The database applies every transaction with `apply_all` while holding its write lock. A panic there poisons the lock and takes down the whole server. With an error, the database can fail the transaction, mark the namespace for reload from checkpoint + WAL, and keep serving other namespaces.

---

## 7. (Optional) Save index definitions in the file format

**Problem**

Property indexes are not part of the saved file. After loading, each index has to be rebuilt with `create_index`, which is O(n log n) per index.

**Proposal**

Store the list of indexed attribute paths in the file (for example under `metadata`), and optionally the index contents, and recreate them on load. Keep the format version rules: readers of format 2 without this field keep working.

**Why the database needs it**

Only worth doing if benchmarks show that index rebuild dominates restart time. Until then, the database keeps index definitions in its catalog and rebuilds them after loading a checkpoint. We'll file this one only once we have numbers.
