# Step 10: Query layer and the `Database` service trait

Status: todo
Milestone: M2 Service
Depends on: step 9

## Goal

One protocol-independent service interface that every access method uses, with bounded, well-defined read operations built on the core's query features.

## Tasks

- [ ] Create `crates/iwdb-query`.
- [ ] `Database` trait (async): `commit`, `get`/`multi_get` (nodes, edges), `neighbourhood(seeds, depth, direction, edge types, filter)`, `traverse` (BFS/DFS), `shortest_path` (BFS/Dijkstra/A*), `random_walks`, `subgraph`, `match(pattern, where, limit)`, `find` (index lookup / `Expr` filter), analytics jobs on a `Projection` (PageRank, components, Leiden, ...), catalog and namespace admin.
- [ ] Limits on every read: max results, max visited (the core `Budget`), max edges examined (a counting `edge_ok` filter until upstream draft 8 lands; `expand` stays internal until then), timeout, cursor pagination. Server-side defaults and hard caps.
- [ ] `explain`: which index (if any) `index_candidates` would use, and the estimated scan size.
- [ ] Error model: typed errors with stable codes (not found, conflict, constraint violation, budget exceeded, timeout, invalid argument, unavailable), documented in `documentation/api/errors.md`.
- [ ] The embedded facade (`iwdb`) implements the trait; Python embedded bindings are moved onto it.

## Acceptance criteria

- No read operation can run unbounded (test per operation).
- The same conformance test suite runs against the trait implementation; later adapters reuse it.
- M2 is done: update [README.md](README.md).
