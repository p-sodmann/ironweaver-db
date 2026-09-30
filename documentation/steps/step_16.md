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
- [ ] Operations guide `documentation/operations.md`.

## Acceptance criteria

- Every metric is exported and documented.
- All admin tasks can be done with `iwctl`.
