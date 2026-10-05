# Step 16c: Metrics, status views, cancel and the console

Status: in progress (16c-1)
Milestone: M4 Production 1.0
Depends on: step 16b (the lifecycle and logs these report on)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Plan change

2026-10-05: 16c is too big for one PR (about the size of 15a), so it lands in two, in this order:

| Part | Covers | Acceptance criteria |
|---|---|---|
| 16c-1 | the instrumentation, the request registry and cancel, the `Admin` reads, their proto, gRPC, REST, OpenAPI and clients, `GET /metrics`, the log tail, the ADRs and docs | the first three |
| 16c-2 | the `schema` read, `rest.js` and the mock agreeing with the server, the status page against a real server | the fourth |

16d, 16e and 16f need only 16c-1. The step is done when 16c-2 is.

Decided before the work started (ADRs 0050 to 0052):

- `namespace_stats` is not a separate read: `namespace_status` gains the unsynced and since-checkpoint counts and `last_checkpoint`, rather than two reads answering nearly the same.
- A running request's visited count is not reported: the core's `Budget` counts visits only inside a read and reports them at its end. A live counter needs the core (an upstream proposal, not filed).
- Commits are listed but can't be cancelled (dropping a commit's future doesn't undo it).

## Goal

An operator sees what the server is doing: Prometheus metrics, `status` views like `pg_stat_*`, the running requests with a way to cancel one, and the operator console showing all of it from a real server.

## Tasks

### Instrumentation (pure Rust)
- [ ] Counters and fixed-bucket histograms in the engine, storage and query crates without an exporter dependency (atomics; a snapshot type): commit latency, fsync time, WAL size, checkpoint duration and lag, memory per namespace, query latency per operation, rejected and timed-out requests, lock hold times. Labels are bounded: namespace and operation, never ids.
- [ ] A request registry in `iwdb-query`: every running trait call gets an id, its operation, namespace, start time and visited count, and holds its cancel token (`ironweaver_core::cancel`), so it can be listed and cancelled.

### The admin reads (design rule 8)
- [ ] Behind the `Database` trait or a sibling `Admin` trait in `iwdb-query`, implemented once for `Embedded`: `server_status` (version, uptime, readiness, fsync policy, memory, disk, request counts), `namespace_stats` (sizes, indexes and builds, unsynced and since-checkpoint counts, `last_checkpoint`), `active_requests`, `cancel(request_id)`, `consumers` (change-stream readers and their lag), `metrics` (the snapshot), and a bounded `log` tail (a ring of the last N events from 16b's subscriber). Every read is O(1) or O(namespaces), with a limit (design rule 5).
- [ ] Proto first (versioned contract), then the gRPC handlers and REST routes through `ops`, then the OpenAPI document; errors.md, rest.md and openapi.json stay equal to the code (their tests).
- [ ] `GET /metrics` in Prometheus' text format, from the snapshot.
- [ ] One metric name list in code, documented as a table (name, type, labels, unit, meaning) in `documentation/api/metrics.md`; a test that every exported metric is documented and every documented one exported.

### The console
- [ ] For each read `console/src/source.js` marks "new" (`schema` with label and type counts and keys, `server` beyond the readiness 16b added, `cancel`, the log, `find` total, explain `matched`, `lastCheckpointMs`): add it on the server and use it in `console/src/rest.js`, or drop it from the pages and the mock, and update `source.js`. Expected: `find` total and explain `matched` are dropped (unbounded counts, design rule 5); label and type counts come from the core's label index if it has an O(1) count; per-label keys stay sampled.
- [ ] The mock's `server()` shape and the server's answer agree (change both where needed); `npm test`, `npm run check`, `pytest console/test` green; `status.html` checked with `serve.py` against a real server.
- [ ] Check the console on the server's own `/console/` (step 16b serves it, ADR 0041) as well as through `serve.py`.

### Docs
- [ ] ADRs: metrics library (or none: hand-written text exposition), the status-view API shape, request ids and cancel.

## Acceptance criteria

- Every metric is exported and documented (test).
- Every status read is bounded and implemented once; gRPC and REST give the same answers (conformance suite).
- A running request can be listed and cancelled over gRPC and REST, and its caller gets `cancelled`.
- The console's status page shows a real server with nothing faked; `source.js` has no "new" left.

## Non-goals

- `iwctl` commands for these (step 16e); the memory limit (step 16d); traces (step 16g); managed jobs (step 16f: the console's jobs panel keeps index builds only until then).
