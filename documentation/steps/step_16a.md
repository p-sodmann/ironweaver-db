# Step 16a: Operator console (on mock data)

Status: done
Milestone: M4 Production 1.0
Depends on: nothing (it runs on a mock); step 16 connects it

Added on 2026-10-04 as a part of step 16 that can be built before the server has status views (see step 16's "Plan change").

## Goal

A web interface for operators, in the IronWeaver::DB design system: explore a graph like Neo4j's browser, browse and edit a namespace like phpMyAdmin, and see the server's state at one glance. It runs on mock data and talks to nothing.

## Tasks

- [x] Vendor the design system (tokens, component source, stylesheet) into `console/design-system/`, generate `tokens.css` and the bundle from it, and make the demo-wired components show real data through optional props, each marked and listed ([README](../../console/design-system/README.md)).
- [x] The Source contract (`console/src/source.js`): every read and write of the pages, in the REST API's shapes, and a mock Source with generated namespaces and a simulated server (`console/src/mock.js`), with a degraded scenario.
- [x] Explorer (`console/index.html`): schema navigator (namespaces, labels, edge types, indexes, constraints); graph, table, plan and structure views of one result; peek and expand on the canvas with real neighbourhoods; the inspector with the node's properties, relationships and storage; the query line with `iwctl shell`'s `match`, `find`, `explain`, `node` and `neighbours`; paging and CSV/JSON export; staged edits committed together; go to anything (`⌘K`); create an index from a plan's hint or the structure view.
- [x] Status (`console/status.html`): health and problems, the last 90 s of commits, commit and fsync p99, query p99, active requests and memory; namespaces with their state, sizes, unsynced and since-checkpoint counts, indexes and marks; memory against the limit, WAL and disk; latency by operation; active requests with cancel; change-stream consumers; jobs and index builds; the log. Live, with pause.
- [x] Light and dark themes, reduced motion, phone widths for the status page.
- [x] Tests: `node --test` for the mock (filters, patterns, paging, explain, commits, constraints, read-only, index builds, server status), the query line and the helpers; the pages call only the Source's methods; CI checks the generated files and runs the tests.
- [x] [ADR 0037](../adr/0037-operator-console.md).

## Notes

- The console's query line runs the shell's commands (ADR 0036) and no query language of its own; the mock matches single chains only.
- What the mock offers that the server doesn't yet, and step 16 has to add (or the console has to drop): `schema` (labels and edge types with counts, attribute keys per label), `server` (the status views and metrics), `cancel`, the server log, `total` for `find`, `matched` in explain, `lastCheckpointMs` in a namespace's status. They are marked "new" in `source.js`.
- Neighbourhoods are `neighbourhood` (depth 1) plus `subgraph` for the edges once connected: `neighbourhood` returns nodes only.

## Acceptance criteria

- Both pages open from the file system with no build step and no network beyond the fonts, in light and dark.
- From the first look, a user can expand a node, browse a label as rows across pages, see a find's plan, edit a property, commit, and see the new seq; and on the status page see every namespace's state and each problem of the degraded scenario without a click.
- `npm run check` and `npm test` pass in `console/`.

## Non-goals

- Connecting to a server, serving the pages from `iwdb-server`, authentication (steps 15 and 16).
- Deleting nodes, managing namespaces or constraints, backups from the console (later, through the same Source).
- A query language beyond the shell's commands.
