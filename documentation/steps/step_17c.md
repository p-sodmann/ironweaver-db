# Step 17c: Soak test

Status: todo
Milestone: M4 Production 1.0
Depends on: step 17b (the order only)

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

The server runs for 24 hours under a mixed load without errors, leaks or unbounded growth, and anyone can repeat the run before a minor release.

## Tasks

- [ ] A soak driver: writes (keyed and plain, several namespaces), bounded reads, matches, the change stream, analytics jobs, backups and checkpoints, over gRPC and REST, against `iwdb-server` with auth and TLS on.
- [ ] What it checks: no failed request beyond the expected ones, memory, WAL size and open files stay bounded (from the status views and metrics), latencies don't drift, and the final state verifies and matches the driver's record.
- [ ] A short version in CI (minutes); the 24 h run on our own machine, since GitHub's hosted runners stop a job after 6 hours.
- [ ] A runbook (`documentation/soak.md`): how to run it, what to watch, where results go, and that it runs before each minor release.

## Acceptance criteria

- One full 24 h run passes before 1.0, and its results are recorded.

## Non-goals

- Performance targets: the benchmarks (step 14b) measure speed; the soak looks for drift and leaks.
