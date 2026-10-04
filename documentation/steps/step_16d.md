# Step 16d: Memory limit

Status: todo
Milestone: M4 Production 1.0
Depends on: step 16c (memory accounting and its metrics)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

The server warns, then refuses writes, before the operating system kills it for memory, and keeps serving reads.

## Tasks

- [ ] Memory accounting good enough to act on: the graph's `memory_usage` doesn't count payloads (attribute maps), projections held by `analyze` or index builds. Find what the core can report; if it lacks payload accounting, follow AGENTS.md "Findings in ironweaver-core" (pin, draft, file) and use a marked estimate meanwhile.
- [ ] `[memory] limit_bytes`, `warn_at` and `refuse_writes_at` (fractions), with a default derived from the cgroup limit when there is one.
- [ ] Above `warn_at`: a metric, a status flag and a log event. Above `refuse_writes_at`: commits and catalog changes fail with a documented code (`read_only` with a reason, or a new `resource_exhausted`; decide in the ADR) before they are logged, reads go on, and writes resume below the line with hysteresis.
- [ ] Fault-injection test (design rule 3): a refused write leaves nothing in the WAL and nothing visible after recovery; a write accepted just below the line is durable.
- [ ] ADR (memory limit policy), guarantees.md, errors.md.

## Acceptance criteria

- Under a growing write load the server refuses writes at the limit instead of being killed, and recovers when memory falls.

## Non-goals

- Per-namespace and per-client memory limits (step 15 builds them on this accounting).
- Evicting data to disk.
