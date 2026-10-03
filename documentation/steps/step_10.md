# Step 10: Query layer and the `Database` service trait

Status: done
Milestone: M2 Service
Depends on: step 9

## Goal

One protocol-independent service interface that every access method uses, with bounded, well-defined read operations built on the core's query features.

## Tasks

- [x] First: run the [upstream check](upstream-check.md). Issues this step depends on: #27. *(Done 2026-10-02 at core `3b15149`: #27 is fixed. Since `cd09ea0` no upstream issue is open.)*
- [x] Create `crates/iwdb-query`. *(It holds the trait, the reads as functions over a `Namespace`, the error model, limits and cursors, a std-only executor, and the conformance suite; `iwdb` depends on it, not the reverse: [ADR 0020](../adr/0020-database-trait.md).)*
- [x] `Database` trait (async): `commit`, `get`/`multi_get` (nodes, edges), `neighbourhood(seeds, depth, direction, edge types, filter)`, `traverse` (BFS/DFS), `shortest_path` (BFS/Dijkstra/A*), `random_walks`, `subgraph`, `match(pattern, where, limit)`, `find` (index lookup / `Expr` filter), analytics jobs on a `Projection` (PageRank, components, Leiden, ...), catalog and namespace admin. *(Names: `get_nodes` / `get_edges` (multi-get; a single get is a one-id call), `match_pattern` (`match` is a keyword), plus `explain`, `wait_for_seq`, `commit_catalog`, `namespace_status`. Native `impl Future + Send` methods, no `async-trait`. `traverse` follows outgoing edges only, as the core's `bfs`/`dfs` do ([#49](https://github.com/p-sodmann/Ironweaver/issues/49)). Analytics jobs are synchronous requests; managed background jobs are deferred to step 16: [ADR 0022](../adr/0022-analytics-jobs.md).)*
- [x] Limits on every read: max results, max visited (the core `Budget`), max edges examined (the core's `Budget::max_edges`, since `3b15149`; traversals also check cancellation per edge, so `expand` can be exposed), timeout, cursor pagination. Server-side defaults and hard caps. *(`LimitConfig`, keyset cursors valid at one seq (`cursor_expired` otherwise), `partial` answers: [ADR 0021](../adr/0021-bounded-reads-and-cursors.md). Where the core takes no budget (paths, matching, walk planning: [#48](https://github.com/p-sodmann/Ironweaver/issues/48)) we count through its hooks; see the upstream check.)*
- [x] `explain`: which index (if any) `index_candidates` would use, and the estimated scan size. *(Estimates from `index_stats`; `analyze` gives the exact candidate count; index builds in progress are listed. The plan mirrors `index_candidates` until [#50](https://github.com/p-sodmann/Ironweaver/issues/50).)*
- [x] Error model: typed errors with stable codes (not found, conflict, constraint violation, budget exceeded, timeout, invalid argument, unavailable), documented in `documentation/api/errors.md`. *(Also `cancelled`, `cursor_expired`, `read_only`, `io` (a commit's outcome is unknown), `corrupt`, `internal`: 13 codes, with the gRPC and HTTP mapping for steps 11–12.)*
- [x] The embedded facade (`iwdb`) implements the trait; Python embedded bindings are moved onto it. *(`iwdb::Embedded`. Python's reads, commits, catalog and namespace calls use it; backups, checkpoints, syncs and the store status stay on `Store`, which the trait doesn't cover. Python query methods (`find`, `match`, ...) come with step 14's client, which mirrors them.)*

## Notes from step 9

- **Every operation names a namespace.** `Store::namespace(name) -> Ns` is the handle the `Database` trait should wrap: `Ns` has commit (with and without keys), catalog changes, `read`, `analyze`, `wait_for_seq`, `status`, `index_entries` and `checkpoint`; `Store` has `create_namespace`, `drop_namespace`, `namespaces`, `status` and `backup`. The shorthands on `Store` (`store.commit`, `store.node`, ...) act on `default` and exist for the embedded API; the trait shouldn't have them (design rule 8: one way to say it).
- **Seqs and `min_seq` are per namespace**, `ReadOptions.history` is the store's. A read token is `(namespace, seq)`. A cursor must carry the namespace and the seq it was made at.
- **Errors to map**: `NoSuchNamespace` and `NamespaceDropped` (not found), `NamespaceExists` and `IdempotencyKeyReused` (conflict), `AmbiguousTarget` (invalid argument), `NamespaceDamaged` and `InvalidNamespaceLog` (corrupt). A read in progress on a dropped namespace finishes; a wait or commit fails with `NamespaceDropped`.
- **Index state in `explain`.** `Ns::status().indexes` has `ready` or `building (scanned/total)`. A building index isn't in the catalog and `find_nodes` can't use it, so `explain` must say "no index" until it is installed. Per-index sizes are in `IndexStatus::size` (entries, distinct keys, memory) and `Ns::index_entries`, both O(1) since the core's `index_stats` (#35); use them for the estimated scan size.
- **Index builds are not free**: since the core's off-graph build (#34) and the starvation fix, an index build stalls commits only for about one scan chunk (about 5 ms at 500 000 nodes), but the build itself takes as long as a scan (about 170 ms at that size), and a unique constraint's validation still holds the writer for its whole scan (about 430 ms) (ADR 0019 and its update). Server limits and timeouts for catalog operations should expect that.
- **Namespace operations are keyed** (`create_namespace` / `drop_namespace` take an `Option<&IdempotencyKey>`); the trait should expose that.
- **Drop and the archive.** `drop_namespace` archives the namespace's remaining WAL if the store has an archive; `iwctl` forces the caller to say so (`--archive` or `--no-archive`). A server should do the same through its configuration.
- **Limits for step 15** (auth and limits): the number of namespaces, names, and memory per namespace are not limited yet; `NamespaceStatus.memory_bytes` is whole-graph memory (indexes included) and is cheap (O(1)), so a per-namespace memory limit can be checked on every commit.

## Outcome

- **Crate direction** ([ADR 0020](../adr/0020-database-trait.md)): `iwdb-query` (trait, reads, errors, limits, cursors, conformance) depends on the core, engine and storage; `iwdb` depends on it and implements the trait (`Embedded`). `Node`, `Edge`, `NamespaceStatus`, `CommitOptions` and `ProjectionSpec` moved there (re-exported by `iwdb`).
- **Async**: a worker pool per `Embedded` runs every request; futures are std-only (`Pending`), `block_on` serves Python and the tests. No async runtime in any library crate. `Ns::read_with` now runs its closure under the deadline's cancel token, so a timeout bounds the whole read.
- **Bounds** ([ADR 0021](../adr/0021-bounded-reads-and-cursors.md)): defaults 1 000 results / 100 000 visited / 1 000 000 edges / 30 s, caps 100 000 / 10 M / 100 M / 5 min.
- **Behaviour change for Python**: an existing namespace or index and a reused idempotency key raise `ConflictError` (was `InvalidError`); dropping a missing index raises `NotFoundError` ([python-api.md](../python-api.md)).
- **Upstream**: [#48](https://github.com/p-sodmann/Ironweaver/issues/48), [#49](https://github.com/p-sodmann/Ironweaver/issues/49), [#50](https://github.com/p-sodmann/Ironweaver/issues/50) filed, none blocking; workarounds in the [upstream check](upstream-check.md).
- **Deferred**: managed analytics jobs (job ids, progress, results kept, cancel) to step 16 ([ADR 0022](../adr/0022-analytics-jobs.md)); Python query methods to step 14; `traverse` in other directions until #49.

## Notes for step 11

- Serve any `D: Database` (the trait isn't object safe); the server holds an `iwdb::Embedded` built with a `QueryConfig` from its config file. Await the futures on tokio directly: they don't block (the work runs on `Embedded`'s workers), and dropping one cancels its read.
- Map the codes as in [errors.md](../api/errors.md). Cursors are opaque strings (`Cursor::new`, `Cursor::as_str`); send them back unchanged.
- The conformance suite runs against the server through a gRPC client that implements `Database`: `iwdb_query::conformance_tests!(...)` with a fixture that starts a server on a fresh store. It assumes the default `LimitConfig`.
- Requests are plain data, but `Job` holds the core's `PageRank` / `Leiden` options and `PathMethod::AStar` its `Coords` / `Metric`: the protos need messages for them.

## Acceptance criteria

- No read operation can run unbounded (test per operation). *(Met: a `*_is_bounded` conformance case per read, `every_read_times_out`, and `a_timeout_stops_a_read_in_progress` in `crates/iwdb/tests/query.rs`.)*
- The same conformance test suite runs against the trait implementation; later adapters reuse it. *(Met: `iwdb_query::conformance_tests!` (feature `conformance`), 26 cases, run in `crates/iwdb/tests/conformance.rs`.)*
- M2 is done: update [README.md](README.md). *(Done.)*
