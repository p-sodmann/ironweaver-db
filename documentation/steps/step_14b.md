# Step 14b: Benchmarks

Status: done
Milestone: M3 Network access
Depends on: step 14a

Split out of step 14 on 2026-10-04 (see its "Plan change").

## Goal

Published performance numbers, and a regression gate in CI.

## Tasks

- [x] Benchmarks (criterion, plus a load generator over gRPC): 100k / 1M / 10M nodes; load, commit throughput per fsync policy, neighbourhood depth 2, shortest path, `match`, PageRank, memory per node and edge.
- [x] A regression gate on the 100k set in CI.
- [x] Results and the machines they were measured on in `documentation/benchmarks.md`.

## Notes

- Everything is in `iwdb-server`, which already depends on every layer: the criterion benches (`benches/graph.rs`), the load generator (`examples/loadgen.rs`) and their shared workload (`benches/support`). The load generator is also the gate (`--check`), so CI needs no criterion output parsing.
- The 10M set runs with degree 1 (10M edges): with degree 4 it needs about 30 GB of memory, more than the measuring machine has. The table says so.
- Targets: [`targets-100k.json`](../../crates/iwdb-server/benches/targets-100k.json), from the measurements with margins for shared runners (see [benchmarks.md](../benchmarks.md#targets)).
- Findings for later steps, in [benchmarks.md](../benchmarks.md#findings): a first page from an index costs O(candidates) (keyset cursors, ADR 0021); `always` doesn't group concurrent commits under one fsync.

## Acceptance criteria

- [x] Benchmark targets set and met.
- [x] M3 is done: update [README.md](README.md).

## Non-goals

- Tuning beyond fixing what the benchmarks show to be clearly wrong; larger optimisations get their own step.
