# Upstream issue drafts for Ironweaver

Status: drafts 1–7 are **done upstream**. They were implemented in [PR #25](https://github.com/p-sodmann/Ironweaver/pull/25) (merge commit `a14149e`), reviewed, and we moved to that revision (see the [core review](ironweaver-core-review.md#recommended-upstream-changes)). Draft 8 comes from that review, is **drafted, pending review** and **not filed**. Draft 9 comes from step 2 and is to be filed (see AGENTS.md, *Findings in ironweaver-core*). After review, file it in [p-sodmann/Ironweaver](https://github.com/p-sodmann/Ironweaver/issues) and replace "not filed" with the issue link, here and in the core review.

Drafts 1–7 were checked against `ironweaver-core` at `02cefab`, drafts 8 and 9 against `a14149e`. Titles are ready to paste; the text below each title is the issue body.

| # | Title | Status |
|---|---|---|
| 1 | [Visit budgets for traversals, path expansion and random walks](#1-visit-budgets-for-traversals-path-expansion-and-random-walks) | done upstream (PR #25, `a14149e`) |
| 2 | [`serde` for `Expr` / `CmpOp`, and a round-tripping `Display` for `Pattern`](#2-serde-for-expr--cmpop-and-a-round-tripping-display-for-pattern) | done upstream (PR #25, `a14149e`) |
| 3 | [Streaming binary loader (`LoadGraph` from `impl Read`)](#3-streaming-binary-loader-loadgraph-from-impl-read) | done upstream (PR #25, `a14149e`) |
| 4 | [Deterministic attribute order when saving](#4-deterministic-attribute-order-when-saving) | done upstream (PR #25, `a14149e`) |
| 5 | [Incremental memory accounting](#5-incremental-memory-accounting) | done upstream (PR #25, `a14149e`) |
| 6 | [No panic in `apply_all` rollback](#6-no-panic-in-apply_all-rollback) | done upstream (PR #25, `a14149e`) |
| 7 | [(Optional) Save index definitions in the file format](#7-optional-save-index-definitions-in-the-file-format) | done upstream (PR #25, `a14149e`) |
| 8 | [Edge budget and per-edge cancellation in `bfs` and `expand`](#8-edge-budget-and-per-edge-cancellation-in-bfs-and-expand) | drafted, not filed |
| 9 | [JSON loader reads `-0.0` back as `0.0`](#9-json-loader-reads--00-back-as-00) | not filed yet (no GitHub credentials in the session that found it) |

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

---

## 8. Edge budget and per-edge cancellation in `bfs` and `expand`

**Problem**

`Budget::max_visited` counts nodes entered, not edges examined. In `traversal::bfs_limited` and `expand_limited`, a node counts once and then its whole edge list is scanned. The cancel token is also polled only once per node, before its edges. So one high-degree node defeats both limits: with a hub of 1M parallel edges and `Budget::default().max_visited(1)`, `bfs_limited` calls the `edge_ok` filter 1M times, and it still does when the token is cancelled from inside the filter on the first edge. `dfs_limited` and `expand_paths_limited` poll cancellation per edge, so a timeout does stop them, but their budget also doesn't count edges.

A caller can bound `bfs_limited` by counting in `edge_ok` and returning an error from it. `expand_limited` takes no closure, so there is no workaround there.

**Proposal**

- Add `max_edges: Option<usize>` to `Budget` (edges examined, whether or not they pass the filter), counted in every `*_limited` search and reported the same way as the other limits (`BudgetExceeded`, or `truncated` with `OnLimit::Truncate`). `Limited` could report `edges` next to `visited`.
- Poll the cancel token inside the edge loops of `bfs` and `expand` (as `dfs` and `expand_paths` already do), for example every 1024 edges if a per-edge poll is too costly.
- A test with a hub of many parallel edges: `max_edges(k)` stops after `k` edges, and a cancel from inside the filter stops within one poll interval.

**Why the database needs it**

Ironweaver DB promises that every remote read is bounded. Graphs with supernodes (a popular account, a shared category) are common, and a neighbourhood query from one of them is exactly the request that must be bounded. Until this lands, the database counts edges in the `bfs` filter itself and doesn't offer `expand` to remote callers.

---

## 9. JSON loader reads `-0.0` back as `0.0`

**Problem**

A `Value::Float(-0.0)` doesn't survive a JSON round trip. The saver writes `{"Float":-0.0}` correctly, but `LoadGraph::from_json_slice` (so also `format::from_json`) reads it back as `Float(0.0)`. The binary format keeps the sign.

The cause is the JSON parser: sonic-rs 0.5.10 parses `-0.0`, `-0` and `-0e0` as positive zero (serde_json keeps the sign). Reproduction, with only sonic-rs:

```rust
let x: f64 = sonic_rs::from_str("-0.0").unwrap();
assert!(x.is_sign_negative()); // fails with sonic-rs 0.5.10
```

Through the core (`a14149e`):

```rust
let mut g: Graph<Record, Record> = Graph::new();
g.add_node("a", Record::with_attr([("x", Value::Float(-0.0))])).unwrap();
let json = format::to_json(&g, &Attrs::new(), false).unwrap(); // contains {"Float":-0.0}
let (h, _) = format::from_json(&json).unwrap();
// h's "x" is Float(0.0): the sign is lost
```

**Proposal**

- Report it to sonic-rs and bump once fixed. Until then, work around it in the loader: parse float tokens that are a negative zero (`-0`, `-0.0`, `-0e0`, ...) as `-0.0`, for example by checking the sign of the raw number text for a zero result.
- Add `-0.0` (and the other special floats that the format can hold) to the JSON round-trip tests.

**Why the database needs it**

Not for correctness of checkpoints: they use the binary format. But JSON export and import, and the JSON compatibility fixtures, should reproduce values exactly. Ironweaver DB compares graphs bit for bit in its round-trip tests (`Float(-0.0)` and `Float(0.0)` differ in the canonical form), and currently has to leave `-0.0` out of its random JSON tests.
