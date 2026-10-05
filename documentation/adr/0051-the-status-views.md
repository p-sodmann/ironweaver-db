# ADR 0051: The status views

Status: accepted
Date: 2026-10-05

## Context

Step 16c asks for `status` views like PostgreSQL's `pg_stat_*`: the server's state, every namespace's sizes, indexes and checkpoint lag, the running requests, the change-stream readers, and a tail of the log for the operator console.

Design rule 8 asks for one implementation behind a service trait, with gRPC, REST, Python, `iwctl --server` (step 16e) and the console only translating. Design rule 5 bounds every read. Points to decide: where the reads live, their shapes and limits, who may call them, and how the log tail is filled without ever holding a secret.

## Decision

**A sibling trait, `iwdb_query::Admin`**, not more methods on `Database`. `Database` stays the data API that every access method and the conformance suite share; `Admin` is the operator's. It is implemented:

- once, by `iwdb::Embedded`;
- again, over the wire, by the gRPC and REST clients;
- by `Authorized<D>`, which checks and narrows.

Its RPCs are their own service, `AdminService` in `admin.proto`, as `Accounts` has `AuthService`. A separate conformance macro (`admin_conformance_tests!`) runs the same cases against the embedded store (through `Authorized`, which registers calls, ADR 0052), gRPC and REST.

**The reads.**

| Method | RPC / REST | What | Bound |
|---|---|---|---|
| `server_status` | `GetServerStatus`, `GET /v1/status` | version, start time, readiness, fsync policy, memory (graphs, and the limit of step 16d), disk (WAL, checkpoints, free space through `statvfs`), request counts, every namespace's status | O(namespaces) |
| `active_requests(user, limit)` | `ListRequests`, `GET /v1/requests` | the running requests, oldest first | `limit`, at most 1000 |
| `cancel_request(id, user)` | `CancelRequest`, `POST /v1/requests/{id}/cancel` | ADR 0052 | O(1) |
| `consumers` | `ListConsumers`, `GET /v1/consumers` | change-stream readers and their lag | at most 1024 |
| `metrics` | `GetMetrics`, `GET /v1/metrics`, `GET /metrics` | ADR 0050 | O(operations + namespaces) |
| `log(after, limit)` | `GetLog`, `GET /v1/log` | the last log events | `limit`, at most 1000 |

`namespace_stats` didn't become a separate read. `GetNamespaceStatus` already reports sizes, indexes and builds, so it gained what was missing: `unsynced` (applied but not known durable), `since_checkpoint` (what recovery would replay) and `last_checkpoint`, the time the newest checkpoint was written. That time is the file's modification time for one written before the store opened, and the end of the run for later ones. Two reads answering nearly the same would have drifted apart.

**Readers** are remembered from their polls: each successful `GetChanges` (and each round of `Watch`) records its namespace, user, client and the seq it reads from next. Entries live for a minute after the last poll, and the table holds at most 1024, the least recently polled giving way. Lag is the namespace's streamable seq minus what the reader has read. Projection marks stay in the namespace status, where they are durable.

**Who may call what** (rows of the operation table, ADR 0045, with their audit column, ADR 0049):

| Operation | Requirement | Audited |
|---|---|---|
| `GetServerStatus` | authenticated; namespaces narrowed to the caller's | refusals |
| `ListRequests` | authenticated; a non-admin sees only its own | refusals |
| `CancelRequest` | authenticated; a non-admin cancels only its own | always |
| `ListConsumers` | authenticated; narrowed to readable namespaces | refusals |
| `GetMetrics` | authenticated; namespace series narrowed | refusals |
| `GetLog` | server admin | refusals |

Narrowing happens in `Authorized`, the one place that decides. The role test covers every cell over gRPC and REST, including that a reader doesn't see `default`'s status or series.

**Admin reads don't wait.** The request list, cancel, the readers, the metrics and the log take no namespace lock and run on the caller's task, not the worker pool. An operator can list and cancel requests while every worker is busy with them. The server status reads each namespace's status (a read lock for an instant), so it runs on a worker like `GetNamespaceStatus`.

**The log tail.**

- A `LogRing` (in `iwdb_query::log`, pure Rust) keeps the last `[log] tail_events` events (default 1000, at most 100 000, 0 for none).
- The server's subscriber fills it through one more `tracing-subscriber` layer, `RingLayer`, behind the same level filter as stderr. So it holds exactly what stderr gets, audit entries included, and nothing else writes to it.
- Each event keeps its time, level, target, message and fields as text, cut at 2 KiB, plus a sequence number, so a reader asks for the events after the last one it saw (the console polls). Spans aren't kept.
- **No secrets.** The logs hold none: secrets travel as `Secret`, audit entries hold names (ADR 0044, ADR 0049), and errors are logged by code with messages that never quote a token. The tail can hold no more than the logs, and only a server admin reads it. `no_secret_reaches_the_logs` reads the whole tail of a server driven through every secret-bearing path and finds none of the secrets in it.

## Consequences

- `Served`, the server's trait bound, includes `Admin`. A database served by `iwdb-server` has to implement it; `Embedded` does.
- `iwctl` commands for these reads (step 16e) and the console (16c's second part) only translate `Admin` calls.
- The status is a snapshot built from per-namespace reads, not one consistent cut across namespaces. Each namespace's part is consistent on its own.
- An embedded store without a server has an empty log tail: only the server's subscriber fills the ring.
