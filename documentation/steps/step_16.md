# Step 16: Operability

Status: done (split into 16a to 16i)
Milestone: M4 Production 1.0
Depends on: step 11 (can run in parallel with steps 12 to 15)

## Goal

Operators can configure, monitor and administer the server without reading the code.

## Tasks

- [x] Config file plus environment overrides, validated at startup with clear errors (16b).
- [x] Health and readiness endpoints (ready only after recovery finished) (16b).
- [x] Prometheus metrics: commit latency, fsync time, WAL size, checkpoint duration and lag, memory per namespace, query latency per operation, rejected and timed-out requests, lock hold times (16c).
- [x] OpenTelemetry traces (16g) and structured JSON logs (`tracing`; done in 16b).
- [x] `status` views (like `pg_stat_*`): namespaces, sizes, indexes, active requests, replication/stream consumers (16c).
- [x] `iwctl` against a running server: status, checkpoint, backup, restore, verify, index and constraint management, namespaces, cancel a request (16e; restore stays offline, ADR 0055).
- [x] Memory-limit behaviour: reject writes and alert before the OS kills the process (16d).
- [x] Windows (moved here from step 7, [ADR 0013](../adr/0013-python-bindings.md)): a directory fsync (`FILE_FLAG_BACKUP_SEMANTICS` and `FlushFileBuffers`, checked on NTFS), a CI job that builds and tests the workspace and the Python bindings on Windows, a crash harness mode that kills with `TerminateProcess`, then Windows wheels and the platform row in [guarantees.md](../guarantees.md). (16h, [ADR 0058](../adr/0058-windows.md))
- [x] A WAL archive pruning command (`iwctl archive prune --before <backup>`), and optionally recording the store's archive in the data directory so that `iwctl checkpoint` needn't be told ([ADR 0012](../adr/0012-iwctl.md)) (16e; the archive isn't recorded: `iwctl --server ... checkpoint` asks the store, ADR 0055).
- [x] Throttling for online backups of large stores (checkpoints wait for a backup's copy, ADR 0009) (16e).
- [x] Managed analytics jobs (moved here from step 10, [ADR 0022](../adr/0022-analytics-jobs.md)): jobs that outlive a request's timeout, with an id, progress, cancellation, results kept for a while (and limited), and `iwctl` to list and cancel them. Step 10's `Database::analyze` runs a job within one request (16f, [ADR 0056](../adr/0056-managed-analytics-jobs.md); progress is the phase until upstream #62).
- [x] Finish connecting the operator console (step 16a, [ADR 0037](../adr/0037-operator-console.md)). Its REST Source and Flask proxy exist; add the reads `console/src/source.js` marks "new" (`schema` with label and type counts, `server` from the status views and metrics, `cancel`, the server log) and use them in `console/src/rest.js`, and decide whether `iwdb-server` serves `console/` itself (behind step 15's authentication). Decided in 16b: it does, opt-in (ADR 0041). Done in 16c-2 ([step_16c.md](step_16c.md), "The console"; ADR 0053); this box was left unticked then.
- [x] Operations guide [`documentation/operations.md`](../operations.md) (16i).

## Plan change

2026-10-04: the operator console's interface was split off into [step 16a](step_16a.md), built on a mock so its design can be reviewed before the status views exist. Its REST Source and a Flask proxy were added there too; the rest of connecting it stays here (the task above), because it needs the status views and metrics this step adds.

2026-10-04: the rest of step 16 is too big for one PR, so it was split into ordered sub-steps, like step 14 into 14a and 14b. Step 16 is done when they are. The tasks below stay as the overview; each is ticked when its sub-step is done.

| Sub-step | Covers | Why here |
|---|---|---|
| [16b](step_16b.md) | config file and environment overrides, health and readiness, JSON logs | Everything else reports through it: metrics and status need the lifecycle (ready, draining), traces build on `tracing`, and 16c's log tail for the console reads its events. Readiness means moving the store's open behind the listener, which reshapes `main.rs` before others build on it. |
| [16c](step_16c.md) | metrics, status views, active requests and cancel; the console hookup | Needs 16b's lifecycle and logs (it adds a ring of recent log events for the console). The request registry and admin reads it adds are what `iwctl`, the memory limit and jobs use. |
| [16d](step_16d.md) | memory limit | Needs 16c's memory accounting and metrics to warn and refuse on. |
| [16e](step_16e.md) | `iwctl` against a server, archive pruning, backup throttling | Needs 16c's admin reads and cancel in the trait; `iwctl` only translates. |
| [16f](step_16f.md) | managed analytics jobs (ADR 0022) | Needs 16c's registry and cancel, 16d's accounting (jobs hold projections) and 16e's `iwctl` to list and cancel. |
| [16g](step_16g.md) | OpenTelemetry traces | Needs only 16b; can run in parallel with 16c to 16f. Last of the server work because nothing depends on it. |
| [16h](step_16h.md) | Windows | Storage and CI only; independent, can run in parallel with any of them. |
| [16i](step_16i.md) | `documentation/operations.md` | Documents what the others built, so it comes last. |

Step 15d's "max memory per namespace" builds on 16d's accounting; 16d only sets a process-wide limit.

## Acceptance criteria

- Every metric is exported and documented.
- All admin tasks can be done with `iwctl`.

## Outcome

Done on 2026-10-10 with step 16i; sub-steps 16a to 16i are each described in their own file.

- Every metric is exported and documented: metrics.md's table is compared with the exported `METRICS` (`every_metric_is_documented_and_every_documented_one_exported`), and the operations guide's metrics and alert rules with the same list (`crates/iwdb-server/tests/operations.rs`).
- All admin tasks can be done with `iwctl`: the admin writes (checkpoint, backup, verify, archive pruning), restore (offline), namespaces, indexes and constraints, users, grants and tokens, the server status, running requests and cancel, and jobs (list, show, cancel, results), each tested in `crates/iwctl/tests/`. Three status reads have no `iwctl` command yet (change-stream readers, the log tail, the metrics); they are read over REST and in the console, and are a recorded follow-up of [step 16i](step_16i.md#follow-ups). Jobs are started through the API (ADR 0056).
- The operator's procedures are in [operations.md](../operations.md), whose examples a test runs.
