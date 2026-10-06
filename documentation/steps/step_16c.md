# Step 16c: Metrics, status views, cancel and the console

Status: done
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

16c-1 done on 2026-10-05 (ADRs 0050, 0051, 0052): the boxes of the instrumentation, the admin reads and the docs are ticked; the console's stay for 16c-2.

16c-2 done on 2026-10-06 (ADR 0053). Two things turned out differently from the expectation in the first console task:

- **The core can count a label but not list the labels**: `label_count` is O(1), but nothing enumerates the labels a graph has, and edge types have no count at all. So the new `schema` read (a `Database` method, `GetSchema`, `GET /v1/namespaces/{ns}/schema`) takes label names from a sample of nodes and gives each its exact count, and counts types in a sample of edges. Upstream draft 24 proposes the missing API, filed as [#60](https://github.com/p-sodmann/Ironweaver/issues/60).
- **The series need no server history**: the console's REST Source derives them from the differences between two polls of the metrics, and the latency per operation from the histograms.

The Python bindings don't expose `schema` (they don't have the `Admin` reads either); that is left for whoever adds those.

## Goal

An operator sees what the server is doing: Prometheus metrics, `status` views like `pg_stat_*`, the running requests with a way to cancel one, and the operator console showing all of it from a real server.

## Tasks

### Instrumentation (pure Rust)
- [x] Counters and fixed-bucket histograms in the engine, storage and query crates without an exporter dependency (atomics; a snapshot type): commit latency, fsync time, WAL size, checkpoint duration and lag, memory per namespace, query latency per operation, rejected and timed-out requests, lock hold times. Labels are bounded: namespace and operation, never ids.
- [x] A request registry in `iwdb-query`: every running trait call gets an id, its operation, namespace, start time and visited count, and holds its cancel token (`ironweaver_core::cancel`), so it can be listed and cancelled. Done without the visited count (the core reports it only when a search ends; upstream draft 23), and with a cancel handle that ends the call's future, which drops the pool's job and so cancels its core token (ADR 0052).

### The admin reads (design rule 8)
- [x] Behind the `Database` trait or a sibling `Admin` trait in `iwdb-query`, implemented once for `Embedded`: `server_status` (version, uptime, readiness, fsync policy, memory, disk, request counts), `namespace_stats` (sizes, indexes and builds, unsynced and since-checkpoint counts, `last_checkpoint`), `active_requests`, `cancel(request_id)`, `consumers` (change-stream readers and their lag), `metrics` (the snapshot), and a bounded `log` tail (a ring of the last N events from 16b's subscriber). Every read is O(1) or O(namespaces), with a limit (design rule 5). Done as `Admin`; `namespace_stats` is `namespace_status` with the counts and `last_checkpoint` added (see the plan change).
- [x] Proto first (versioned contract), then the gRPC handlers and REST routes through `ops`, then the OpenAPI document; errors.md, rest.md and openapi.json stay equal to the code (their tests).
- [x] `GET /metrics` in Prometheus' text format, from the snapshot.
- [x] One metric name list in code, documented as a table (name, type, labels, unit, meaning) in `documentation/api/metrics.md`; a test that every exported metric is documented and every documented one exported.

### The console
- [x] For each read `console/src/source.js` marks "new" (`schema` with label and type counts and keys, `server` beyond the readiness 16b added, `cancel`, the log, `find` total, explain `matched`, `lastCheckpointMs`): add it on the server and use it in `console/src/rest.js`, or drop it from the pages and the mock, and update `source.js`. Expected: `find` total and explain `matched` are dropped (unbounded counts, design rule 5); label and type counts come from the core's label index if it has an O(1) count; per-label keys stay sampled. Done: `schema` added on the server (sampled, label counts exact; ADR 0053), `server` from the status views and metrics, `cancel` and the log from 16c-1's reads, `lastCheckpointMs` replaced by the status' `lastCheckpointMicros`, `unsynced` and `sinceCheckpoint`; `find` total and explain `matched` dropped.
- [x] The mock's `server()` shape and the server's answer agree (change both where needed); `npm test`, `npm run check`, `pytest console/test` green; `status.html` checked with `serve.py` against a real server. Done: a test compares the keys of both Sources' answers; checked with Playwright against a seeded server, under commit load, cancelling a long poll from the page (its caller got 499 `cancelled`).
- [x] Check the console on the server's own `/console/` (step 16b serves it, ADR 0041) as well as through `serve.py`. Done: the same check on `/console/status.html?source=rest`.

### Docs
- [x] ADRs: metrics library (or none: hand-written text exposition), the status-view API shape, request ids and cancel.

## Acceptance criteria

- Every metric is exported and documented (test).
- Every status read is bounded and implemented once; gRPC and REST give the same answers (conformance suite).
- A running request can be listed and cancelled over gRPC and REST, and its caller gets `cancelled`.
- The console's status page shows a real server with nothing faked; `source.js` has no "new" left.

## Non-goals

- `iwctl` commands for these (step 16e); the memory limit (step 16d); traces (step 16g); managed jobs (step 16f: the console's jobs panel keeps index builds only until then).
