# Step 14b: Benchmarks

Status: todo
Milestone: M3 Network access
Depends on: step 14a

Split out of step 14 on 2026-10-04 (see its "Plan change").

## Goal

Published performance numbers, and a regression gate in CI.

## Tasks

- [ ] Benchmarks (criterion, plus a load generator over gRPC): 100k / 1M / 10M nodes; load, commit throughput per fsync policy, neighbourhood depth 2, shortest path, `match`, PageRank, memory per node and edge.
- [ ] A regression gate on the 100k set in CI.
- [ ] Results and the machines they were measured on in `documentation/benchmarks.md`.

## Acceptance criteria

- Benchmark targets set and met.
- M3 is done: update [README.md](README.md).

## Non-goals

- Tuning beyond fixing what the benchmarks show to be clearly wrong; larger optimisations get their own step.
