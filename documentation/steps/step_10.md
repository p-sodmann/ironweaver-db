# Step 10: Query layer and the `Database` service trait

Status: todo
Milestone: M2 Service
Depends on: step 9

## Goal

One protocol-independent service interface that every access method uses, with bounded, well-defined read operations built on the core's query features.

## Tasks

- [x] First: run the [upstream check](upstream-check.md). Issues this step depends on: #27. *(Done 2026-10-02 at core `3b15149`: #27 is fixed.)*
- [ ] Create `crates/iwdb-query`.
- [ ] `Database` trait (async): `commit`, `get`/`multi_get` (nodes, edges), `neighbourhood(seeds, depth, direction, edge types, filter)`, `traverse` (BFS/DFS), `shortest_path` (BFS/Dijkstra/A*), `random_walks`, `subgraph`, `match(pattern, where, limit)`, `find` (index lookup / `Expr` filter), analytics jobs on a `Projection` (PageRank, components, Leiden, ...), catalog and namespace admin.
- [ ] Limits on every read: max results, max visited (the core `Budget`), max edges examined (the core's `Budget::max_edges`, since `3b15149`; traversals also check cancellation per edge, so `expand` can be exposed), timeout, cursor pagination. Server-side defaults and hard caps.
- [ ] `explain`: which index (if any) `index_candidates` would use, and the estimated scan size.
- [ ] Error model: typed errors with stable codes (not found, conflict, constraint violation, budget exceeded, timeout, invalid argument, unavailable), documented in `documentation/api/errors.md`.
- [ ] The embedded facade (`iwdb`) implements the trait; Python embedded bindings are moved onto it.

## Notes from step 9

- **Every operation names a namespace.** `Store::namespace(name) -> Ns` is the handle the `Database` trait should wrap: `Ns` has commit (with and without keys), catalog changes, `read`, `analyze`, `wait_for_seq`, `status`, `index_entries` and `checkpoint`; `Store` has `create_namespace`, `drop_namespace`, `namespaces`, `status` and `backup`. The shorthands on `Store` (`store.commit`, `store.node`, ...) act on `default` and exist for the embedded API; the trait shouldn't have them (design rule 8: one way to say it).
- **Seqs and `min_seq` are per namespace**, `ReadOptions.history` is the store's. A read token is `(namespace, seq)`. A cursor must carry the namespace and the seq it was made at.
- **Errors to map**: `NoSuchNamespace` and `NamespaceDropped` (not found), `NamespaceExists` and `IdempotencyKeyReused` (conflict), `AmbiguousTarget` (invalid argument), `NamespaceDamaged` and `InvalidNamespaceLog` (corrupt). A read in progress on a dropped namespace finishes; a wait or commit fails with `NamespaceDropped`.
- **Index state in `explain`.** `Ns::status().indexes` has `ready` or `building (scanned/total)`. A building index isn't in the catalog and `find_nodes` can't use it, so `explain` must say "no index" until it is installed. Per-index entry counts come from `Ns::index_entries` (range scans, O(entries); upstream #35).
- **Index builds are not free**: the final insertion holds the namespace write lock (about 100 ms at 500 000 nodes) and a unique constraint's validation holds the writer for its whole scan (ADR 0019). Server limits and timeouts for catalog operations should expect that; the core can't do better until #34.
- **Namespace operations are keyed** (`create_namespace` / `drop_namespace` take an `Option<&IdempotencyKey>`); the trait should expose that.
- **Drop and the archive.** `drop_namespace` archives the namespace's remaining WAL if the store has an archive; `iwctl` forces the caller to say so (`--archive` or `--no-archive`). A server should do the same through its configuration.
- **Limits for step 15** (auth and limits): the number of namespaces, names, and memory per namespace are not limited yet; `NamespaceStatus.memory_bytes` is whole-graph memory (indexes included) and is cheap (O(1)), so a per-namespace memory limit can be checked on every commit.

## Acceptance criteria

- No read operation can run unbounded (test per operation).
- The same conformance test suite runs against the trait implementation; later adapters reuse it.
- M2 is done: update [README.md](README.md).
