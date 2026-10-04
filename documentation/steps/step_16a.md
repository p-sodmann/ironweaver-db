# Step 16a: Operator console

Status: done
Milestone: M4 Production 1.0
Depends on: step 12 (REST) to run against a server; nothing to run on mock data

Added on 2026-10-04 as a part of step 16 that can be built before the server has status views (see step 16's "Plan change").

## Goal

A web interface for operators, in the IronWeaver::DB design system: explore a graph like Neo4j's browser, browse and edit a namespace like phpMyAdmin, and see the server's state at one glance. It runs on mock data, or against a server through a small Flask proxy.

## Tasks

- [x] Vendor the design system (tokens, component source, stylesheet) into `console/design-system/`, generate `tokens.css` and the bundle from it, and make the demo-wired components show real data through optional props, each marked and listed ([README](../../console/design-system/README.md)).
- [x] The Source contract (`console/src/source.js`): every read and write of the pages, in the REST API's shapes, and a mock Source with generated namespaces and a simulated server (`console/src/mock.js`), with a degraded scenario.
- [x] Explorer (`console/index.html`): schema navigator (namespaces, labels, edge types, indexes, constraints); graph, table, plan and structure views of one result; peek and expand on the canvas with real neighbourhoods; the inspector with the node's properties, relationships and storage; the query line with `iwctl shell`'s `match`, `find`, `explain`, `node` and `neighbours`; paging and CSV/JSON export; staged edits committed together; go to anything (`⌘K`); create an index from a plan's hint or the structure view.
- [x] Status (`console/status.html`): health and problems, the last 90 s of commits, commit and fsync p99, query p99, active requests and memory; namespaces with their state, sizes, unsynced and since-checkpoint counts, indexes and marks; memory against the limit, WAL and disk; latency by operation; active requests with cancel; change-stream consumers; jobs and index builds; the log. Live, with pause.
- [x] Light and dark themes, reduced motion, phone widths for the status page.
- [x] Tests: `node --test` for the mock (filters, patterns, paging, explain, commits, constraints, read-only, index builds, server status), the query line and the helpers; the pages call only the Source's methods; CI checks the generated files and runs the tests.
- [x] [ADR 0037](../adr/0037-operator-console.md).
- [x] Against a real server (added on 2026-10-04, see "Plan change"): the REST Source (`console/src/rest.js`), `console/serve.py` (Flask: the pages, and `/v1` passed through, so they share the server's origin), `console/tools/seed.mjs` (the sample namespaces into a server), and tests of both.

## Plan change

2026-10-04: the owner wanted to try the console against the server in Docker, so the REST Source and a Flask server that passes `/v1` through were pulled in from step 16. A proxy, not `iwdb-server` serving the files, because the console isn't part of the server yet and the proxy needs no server change. What still needs the server (the status views, metrics, cancel, the log, a schema read) stays in step 16; the status page says so in place.

## Notes

- The console's query line runs the shell's commands (ADR 0036) and no query language of its own; the mock matches single chains only.
- What the mock offers that the server doesn't yet, and step 16 has to add (or the console has to drop): `schema` (labels and edge types with counts, attribute keys per label), `server` (the status views and metrics), `cancel`, the server log, `total` for `find`, `matched` in explain, `lastCheckpointMs` in a namespace's status. They are marked "new" in `source.js`.
- On a server a node's neighbours are a depth-1 `subgraph` in both directions (`neighbourhood` returns nodes only), bounded by `maxVisited`; the induced edges of a result are a depth-0 `subgraph` of its nodes.

## Acceptance criteria

- Both pages open from the file system with no build step and no network beyond the fonts, in light and dark.
- From the first look, a user can expand a node, browse a label as rows across pages, see a find's plan, edit a property, commit, and see the new seq; and on the status page see every namespace's state and each problem of the degraded scenario without a click.
- Against a server (`serve.py`, the sample data from `seed.mjs`): the same, and the commit is on the server.
- `npm run check` and `npm test` pass in `console/`, and `pytest console/test`.

## Non-goals

- The server's status views, metrics, cancel and log; serving the pages from `iwdb-server`; authentication (steps 15 and 16).
- Deleting nodes, managing namespaces or constraints, backups from the console (later, through the same Source).
- A query language beyond the shell's commands.
