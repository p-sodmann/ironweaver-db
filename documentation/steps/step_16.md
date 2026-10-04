# Step 16: Operability

Status: todo
Milestone: M4 Production 1.0
Depends on: step 11 (can run in parallel with steps 12 to 15)

## Goal

Operators can configure, monitor and administer the server without reading the code.

## Tasks

- [ ] Config file plus environment overrides, validated at startup with clear errors.
- [ ] Health and readiness endpoints (ready only after recovery finished).
- [ ] Prometheus metrics: commit latency, fsync time, WAL size, checkpoint duration and lag, memory per namespace, query latency per operation, rejected and timed-out requests, lock hold times.
- [ ] OpenTelemetry traces and structured JSON logs (`tracing`).
- [ ] `status` views (like `pg_stat_*`): namespaces, sizes, indexes, active requests, replication/stream consumers.
- [ ] `iwctl` against a running server: status, checkpoint, backup, restore, verify, index and constraint management, namespaces, cancel a request.
- [ ] Memory-limit behaviour: reject writes and alert before the OS kills the process.
- [ ] Windows (moved here from step 7, [ADR 0013](../adr/0013-python-bindings.md)): a directory fsync (`FILE_FLAG_BACKUP_SEMANTICS` and `FlushFileBuffers`, checked on NTFS), a CI job that builds and tests the workspace and the Python bindings on Windows, a crash harness mode that kills with `TerminateProcess`, then Windows wheels and the platform row in [guarantees.md](../guarantees.md).
- [ ] A WAL archive pruning command (`iwctl archive prune --before <backup>`), and optionally recording the store's archive in the data directory so that `iwctl checkpoint` needn't be told ([ADR 0012](../adr/0012-iwctl.md)).
- [ ] Throttling for online backups of large stores (checkpoints wait for a backup's copy, ADR 0009).
- [ ] Managed analytics jobs (moved here from step 10, [ADR 0022](../adr/0022-analytics-jobs.md)): jobs that outlive a request's timeout, with an id, progress, cancellation, results kept for a while (and limited), and `iwctl` to list and cancel them. Step 10's `Database::analyze` runs a job within one request.
- [ ] Finish connecting the operator console (step 16a, [ADR 0037](../adr/0037-operator-console.md)). Its REST Source and Flask proxy exist; add the reads `console/src/source.js` marks "new" (`schema` with label and type counts, `server` from the status views and metrics, `cancel`, the server log) and use them in `console/src/rest.js`, and decide whether `iwdb-server` serves `console/` itself (behind step 15's authentication).
- [ ] Operations guide `documentation/operations.md`.

## Plan change

2026-10-04: the operator console's interface was split off into [step 16a](step_16a.md), built on a mock so its design can be reviewed before the status views exist. Its REST Source and a Flask proxy were added there too; the rest of connecting it stays here (the task above), because it needs the status views and metrics this step adds.

## Acceptance criteria

- Every metric is exported and documented.
- All admin tasks can be done with `iwctl`.
