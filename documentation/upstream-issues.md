# Upstream issue drafts for Ironweaver

Status: drafts 1–7 are **done upstream**. They were implemented in [PR #25](https://github.com/p-sodmann/Ironweaver/pull/25) (merge commit `a14149e`), reviewed, and we moved to that revision (see the [core review](ironweaver-core-review.md#recommended-upstream-changes)). Drafts 8–12 are findings from the `a14149e` bump and step 2, draft 13 from step 3, draft 14 from step 5, draft 15 from step 7; all are filed (links in the table).

Drafts 1–7 were checked against `ironweaver-core` at `02cefab`, drafts 8–17 against `a14149e`. Titles are ready to paste; the text below each title is the issue body.

| # | Title | Status |
|---|---|---|
| 1 | [Visit budgets for traversals, path expansion and random walks](#1-visit-budgets-for-traversals-path-expansion-and-random-walks) | done upstream (PR #25, `a14149e`) |
| 2 | [`serde` for `Expr` / `CmpOp`, and a round-tripping `Display` for `Pattern`](#2-serde-for-expr--cmpop-and-a-round-tripping-display-for-pattern) | done upstream (PR #25, `a14149e`) |
| 3 | [Streaming binary loader (`LoadGraph` from `impl Read`)](#3-streaming-binary-loader-loadgraph-from-impl-read) | done upstream (PR #25, `a14149e`) |
| 4 | [Deterministic attribute order when saving](#4-deterministic-attribute-order-when-saving) | done upstream (PR #25, `a14149e`) |
| 5 | [Incremental memory accounting](#5-incremental-memory-accounting) | done upstream (PR #25, `a14149e`) |
| 6 | [No panic in `apply_all` rollback](#6-no-panic-in-apply_all-rollback) | done upstream (PR #25, `a14149e`) |
| 7 | [(Optional) Save index definitions in the file format](#7-optional-save-index-definitions-in-the-file-format) | done upstream (PR #25, `a14149e`) |
| 8 | [Edge budget and per-edge cancellation in `bfs` and `expand`](#8-edge-budget-and-per-edge-cancellation-in-bfs-and-expand) | filed: [#27](https://github.com/p-sodmann/Ironweaver/issues/27) |
| 9 | [JSON loader reads `-0.0` back as `0.0`](#9-json-loader-reads--00-back-as-00) | filed: [#26](https://github.com/p-sodmann/Ironweaver/issues/26) |
| 10 | [`expect` on the op apply path in `remove_node` / `rename_node`](#10-expect-on-the-op-apply-path-in-remove_node--rename_node) | filed: [#28](https://github.com/p-sodmann/Ironweaver/issues/28) |
| 11 | [`Expr` depth-limit errors lose their message under postcard](#11-expr-depth-limit-errors-lose-their-message-under-postcard) | filed: [#29](https://github.com/p-sodmann/Ironweaver/issues/29) |
| 12 | [Small API and dependency cleanups: public attribute lookup, optional bincode, doc comments](#12-small-api-and-dependency-cleanups-public-attribute-lookup-optional-bincode-doc-comments) | filed: [#30](https://github.com/p-sodmann/Ironweaver/issues/30) |
| 13 | [`Value` serde rejects empty containers at the depth limit that the file format accepts](#13-value-serde-rejects-empty-containers-at-the-depth-limit-that-the-file-format-accepts) | filed: [#31](https://github.com/p-sodmann/Ironweaver/issues/31) |
| 14 | [`write_atomic` ignores a failed directory fsync after the rename](#14-write_atomic-ignores-a-failed-directory-fsync-after-the-rename) | filed: [#32](https://github.com/p-sodmann/Ironweaver/issues/32) |
| 15 | [Binary format: the header's flags and reserved bytes are never checked](#15-binary-format-the-headers-flags-and-reserved-bytes-are-never-checked) | filed: [#33](https://github.com/p-sodmann/Ironweaver/issues/33) |
| 16 | [No way to build an index off the graph and install it in O(1)](#16-no-way-to-build-an-index-off-the-graph-and-install-it-in-o1) | filed: [#34](https://github.com/p-sodmann/Ironweaver/issues/34) |
| 17 | [No per-index entry count or memory accessor](#17-no-per-index-entry-count-or-memory-accessor) | filed: [#35](https://github.com/p-sodmann/Ironweaver/issues/35) |

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

---

## 10. `expect` on the op apply path in `remove_node` / `rename_node`

**Problem**

PR #25 made `Graph::apply` / `apply_all` panic-free: undo failures and "just looked up" nodes now return `GraphError::Internal`. But `Graph::remove_node` and `Graph::rename_node`, which `apply` calls for `Op::RemoveNode` and `Op::RenameNode`, gained a new `expect` (`graph.rs`, a14149e):

```rust
let (old_key, _) = self.index.remove_entry(&old).expect("live nodes are indexed");  // rename_node
let (key, _) = self.index.remove_entry(&node.id).expect("live nodes are indexed");  // remove_node
```

It is believed unreachable (every live node is in the id index), but if an invariant bug ever broke that, the process panics in the middle of an op instead of returning an error.

**Proposal**

Return `GraphError::Internal("a live node is missing from the id index")` from these paths (`rename_node` already returns `Result`; `remove_node` returns `Option`, so either change it to `Result` or add an internal `try_remove_node` that `apply` uses). Optionally, audit the remaining `expect`s reachable from `apply` (e.g. `node_ref_mut`, `edge_ref` in graph.rs) the same way.

**Why the database needs it**

Ironweaver DB applies every transaction with `apply_all` while holding its write lock. A panic there poisons the lock and takes down every namespace on the server; an `Internal` error lets the database fail one transaction and reload one namespace from checkpoint + WAL.

---

## 11. `Expr` depth-limit errors lose their message under postcard

**Problem**

`Expr`'s serde helpers (`expr.rs`, module `nested`) raise the depth-limit error with `serde::ser::Error::custom` / `serde::de::Error::custom`. The binary encoding (postcard) drops custom messages, so a too-deeply nested expression fails with a generic postcard error instead of "expression nested more than 100 levels deep". JSON keeps the message. `Value` already avoids this by going through `format::ser_error` / `format::de_error`, which remember the message.

Reproduction (a14149e):

```rust
let mut e = Expr::Const(true);
for _ in 0..200 { e = Expr::Not(Box::new(e)); }
let err = postcard::to_stdvec(&e).unwrap_err().to_string();
assert!(err.contains("nested more than"));  // fails
```

**Proposal**

Use `format::ser_error` / `format::de_error` in `expr.rs`'s `nested` module (and check `Pattern`'s serde for the same pattern), so callers that encode with postcard can report the real reason, as `write_binary` does for values.

**Why the database needs it**

Filters reach the database over gRPC in a binary encoding. A request with an over-deep filter should be rejected with "expression nested too deep", not an opaque decoding error, so clients know what to fix.

---

## 12. Small API and dependency cleanups: public attribute lookup, optional bincode, doc comments

Three small, independent items; low priority.

1. **Public attribute-path lookup.** `Record::at` (the path rules behind `Attributes`: name, then keys into nested dicts, `None` counts as missing) is private. Payload types that want identical semantics have to copy it; Ironweaver DB's `DbRecord` does, with a property test that it matches `Record`. Proposal: a public `record::lookup(attrs: &Attrs, path: &[String]) -> Option<&Value>` that `Record` uses too.
2. **bincode behind a feature.** bincode 1.x is unmaintained (RUSTSEC-2025-0141) and is only used to read format-1 binary files. It is an unconditional dependency, so every downstream `cargo deny` / `cargo audit` has to ignore the advisory. Proposal: a `format-v1` feature (default on, if compatibility matters), so users who only read format 2 can drop bincode.
3. **Doc comments say "bincode".** `format::to_binary` ("Encode ... with bincode") and `format::from_binary` ("Decode a bincode document") describe format 1; format 2 is postcard. Update the comments.

---

## 13. `Value` serde rejects empty containers at the depth limit that the file format accepts

Found in step 3 (filed as [#31](https://github.com/p-sodmann/Ironweaver/issues/31)).

**Problem**

`Value`'s serde and the file format disagree by one level on how deep an attribute value may be nested, when the innermost container is empty.

- The file format counts value depth: a scalar is depth 1, and `MAX_DEPTH` (100) is the deepest allowed (`format/load.rs`, `tagged::check_depth` in `format/save.rs`).
- `Value`'s serde (`value.rs`, module `nested`) counts containers entered, and fails once `MAX_DEPTH` containers are open (`enter_level`: `open >= max`).

With a scalar innermost, both allow 99 lists around it (depth 100). With an empty list or dict innermost, the file format accepts 100 nested containers (depth 100), but `Value`'s serde rejects them. `Value`'s doc comment says serde "fails for values nested more than `MAX_DEPTH` levels", so it rejects a value that is within the documented limit.

Reproduction (`a14149e`):

```rust
let v = (0..99).fold(Value::List(vec![]), |v, _| Value::List(vec![v])); // depth 100
let mut g: Graph<Record, Record> = Graph::new();
g.add_node("a", Record::with_attr([("k", v.clone())])).unwrap();
let bytes = format::to_binary(&g, &Attrs::new(), false).unwrap();   // ok
format::from_binary(&bytes).unwrap();                               // ok
postcard::to_stdvec(&v).unwrap_err();     // "attribute values nested more than 100 levels deep"
serde_json::to_string(&v).unwrap_err();   // same
```

The same holds for an empty `Dict`, and for `Op`s carrying such a value (`Op`'s serde goes through `Value`'s).

**Proposal**

Make `Value`'s serde count depth like the file format: fail when a value's depth exceeds `MAX_DEPTH`, where the attribute's own value is depth 1 and a container's items are one deeper. For example, count levels per `Value` (in `Value`'s own `Serialize` / `Deserialize`) rather than around a container's contents. Add the empty-container case to the depth tests of both encoders.

**Why the database needs it**

Ironweaver DB writes its log with `Value`'s serde (postcard) and saves checkpoints through a custom `Codec` that also serializes attributes with `Value`'s serde. So a graph file that holds an empty list at depth 100 loads, but can't be checkpointed or logged again: saving it fails with "nested more than 100 levels deep". Our commit pipeline avoids this by counting an empty container as if it held a scalar (one level stricter than the file format), and imports will have to apply the same check. With one depth rule for both encoders, everything that loads can be saved and logged again.

## 14. `write_atomic` ignores a failed directory fsync after the rename

Found in step 5 (filed as [#32](https://github.com/p-sodmann/Ironweaver/issues/32)), checked against `a14149e`.

**Problem**

`format::write_atomic` makes the rename durable only on a best-effort basis: after `fs::rename(&tmp, path)` it opens the parent directory and calls `sync_all`, but ignores both errors (`if let Ok(d) = File::open(dir) { let _ = d.sync_all(); }`). So `Ok(())` means the new file's contents are durable, but not that the directory entry pointing to it is. After a power loss the old file (or no file) can reappear, and the caller had no way to know.

Reproduction (`a14149e`, Unix, not as root): a directory that can be written to but not opened for reading (mode `0o300`). Creating the temporary file and renaming it work, opening the directory to fsync it fails, and the save still returns `Ok`:

```rust
use std::os::unix::fs::PermissionsExt;
std::fs::create_dir("sub").unwrap();
std::fs::set_permissions("sub", std::fs::Permissions::from_mode(0o300)).unwrap();
let result = ironweaver_core::format::write_atomic("sub/file", |out| out.write_all(b"data"));
assert!(result.is_ok()); // the directory was never synced
```

A failing `fsync` of the directory (EIO) is ignored the same way. After a failed fsync, a retry can succeed without writing anything (the "fsyncgate" behaviour of Linux), so an ignored failure can't be made up for later by syncing again.

**Proposal**

Return the error of the directory sync on Unix, as for every other step. Where it isn't possible (Windows), document that it isn't done. If the best-effort behaviour is wanted for some callers, keep it as a separate function or an option (for example `write_atomic_with(path, SyncDir::Required | SyncDir::BestEffort, write)`), with the strict one as the default. The doc comment should say what `Ok` guarantees: contents fsynced, rename done, rename durable.

**Why the database needs it**

Ironweaver DB writes checkpoints with `write_atomic` and then deletes the WAL segments and older checkpoints the new checkpoint covers. That is only safe if the new checkpoint's directory entry is durable before anything is deleted. If the rename were lost in a crash after the deletions, the data would be gone. Until this is fixed, we call our own directory fsync after `write_atomic` and check its result, which means a second directory sync per checkpoint and a failure mode that `write_atomic` could report itself.

## 15. Binary format: the header's flags and reserved bytes are never checked

Found in step 7 while building `verify` (filed as [#33](https://github.com/p-sodmann/Ironweaver/issues/33)), checked against `a14149e`.

**Problem**

`docs/format.md` describes the binary header as `b"IRONWEAV", u16 format version (2), u16 flags (0), u32 reserved (0)`, and says the framing detects "a truncated or corrupted file before anything is parsed". But the loaders check only the magic and the version: `check_version` reads bytes 8..10, and nothing reads bytes 10..16. The CRC32 in the trailer covers the payload only. So:

- damage in the `flags` or `reserved` bytes goes unnoticed: the file loads as if it were intact, and an integrity check built on the loader can't see it;
- a flag that a newer writer sets (the reason to have a flags field) would be silently ignored by this reader instead of refused, so a file whose meaning the flag changes would be misread.

Reproduction (`a14149e`):

```rust
use ironweaver_core::{format, Attrs, Graph, Op, Record};
let mut g: Graph<Record, Record> = Graph::new();
g.apply(Op::AddNode { id: "a".into(), labels: vec![], data: Record::default() }).unwrap();
let mut bytes = format::to_binary(&g, &Attrs::new(), false).unwrap();
bytes[10] = 0x01;                                            // a flag
bytes[12..16].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);   // reserved
assert!(format::from_binary(&bytes).is_ok());               // loads
assert!(format::from_binary_reader(&bytes[..]).is_ok());    // streaming too
```

**Proposal**

In `check_version` (used by both `binary_payload` and `build_from_reader`), reject a file whose `flags` has a bit this reader doesn't know, or whose `reserved` field isn't 0, with a `GraphError::Format` that says so (for flags, like a newer version: "written by a newer ironweaver?"). Alternatively, or as well, let the trailer's CRC cover the header too, which would need a format version bump; checking the fields is enough for format 2 and needs no bump, since the writer always writes zeros.

**Why the database needs it**

Ironweaver DB's checkpoints are binary files, and `verify` (like SQLite's `PRAGMA integrity_check`) promises to find any damaged byte in them. Until the loader checks the header, we read the 16 header bytes ourselves in `verify` and report non-zero `flags` or `reserved` as damage. Recovery keeps loading such a checkpoint: its data is intact.

## 16. No way to build an index off the graph and install it in O(1)

Found in step 9 while building online index builds (filed as [#34](https://github.com/p-sodmann/Ironweaver/issues/34)), checked against `a14149e`.

**Problem**

An index can only be built through `&mut Graph`. `create_index` reads every node's payload and inserts every key; `create_index_with_keys(path, keys)` lets the caller read the keys beforehand, but the keys are still inserted into the new index inside the `&mut` call, which is O(n log n) for a `BTreeMap`-backed index. A database that keeps the graph behind an `RwLock` therefore holds the write lock (blocking every reader and the next commit) for the whole insertion, however the keys were read. Measured with 500 000 nodes (release build): the build inside one lock hold takes 183 ms; reading the keys outside the lock and inserting them under it still stalls writers for about 105 ms.

Building incrementally through the dirty set doesn't help: while any node is dirty, every index lookup scans all dirty nodes, so a half-built index makes every lookup O(backlog).

Reproduction: `create_index_with_keys` needs `&mut self`, leaves nodes missing from `keys` dirty, and inserts the given keys during the call (see `index.rs`).

**Proposal**

A way to build an index without touching the graph, then install it in O(1): for example `PropertyIndex::build(path, keys) -> PropertyIndex` (or `Graph::build_index(&self, path) -> BuiltIndex`, taking only `&self`), plus `Graph::install_index(&mut self, BuiltIndex) -> Result<..>` that checks the index was built from the current node set (or lists the nodes that changed, which are then marked dirty) and swaps it in.

**Why the database needs it**

Creating an index or a unique constraint on a large namespace must not stall commits and reads for the whole build. Until this exists we scan keys in chunks under the read lock and insert them under the write lock (ADR 0019), which shortens but doesn't remove the stall.

## 17. No per-index entry count or memory accessor

Found in step 9 while adding catalog status views (filed as [#35](https://github.com/p-sodmann/Ironweaver/issues/35)), checked against `a14149e`.

**Problem**

The core exposes `Graph::memory_usage()` for the whole graph (indexes included) and `index_paths()`, but nothing per index: neither the number of entries nor the memory an index uses. `has_index` and `find_nodes` are the only other accessors.

**Proposal**

Add `Graph::index_stats(path) -> Option<IndexStats { entries, memory_bytes, ... }>` (O(1) if the index keeps a counter, otherwise documented as O(entries)), or an iterator over the index's entries.

**Why the database needs it**

Status views list each index with its size and memory. We currently count entries with range scans over the index (a range below and one above each key kind enumerate an index completely), which is O(entries) and ignores dirty nodes, and report only whole-graph memory.
