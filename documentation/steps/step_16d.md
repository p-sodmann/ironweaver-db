# Step 16d: Memory limit

Status: done
Milestone: M4 Production 1.0
Depends on: step 16c (memory accounting and its metrics)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

The server warns, then refuses writes, before the operating system kills it for memory, and keeps serving reads.

## Tasks

- [x] Memory accounting good enough to act on: the graph's `memory_usage` doesn't count payloads (attribute maps), projections held by `analyze` or index builds. Find what the core can report; if it lacks payload accounting, follow AGENTS.md "Findings in ironweaver-core" (pin, draft, file) and use a marked estimate meanwhile.
- [x] `[memory] limit_bytes`, `warn_at` and `refuse_writes_at` (fractions), with a default derived from the cgroup limit when there is one.
- [x] Above `warn_at`: a metric, a status flag and a log event. Above `refuse_writes_at`: commits and catalog changes fail with a documented code (`read_only` with a reason, or a new `resource_exhausted`; decide in the ADR) before they are logged, reads go on, and writes resume below the line with hysteresis.
- [x] Fault-injection test (design rule 3): a refused write leaves nothing in the WAL and nothing visible after recovery; a write accepted just below the line is durable.
- [x] ADR (memory limit policy), guarantees.md, errors.md.

## Outcome

- **Accounting** (measured first, ADR 0054). The core's `memory_usage` sees 35–50 % of the heap of a graph with attributes. The rest is payloads, which the core can't count (upstream [#61](https://github.com/p-sodmann/Ironweaver/issues/61), draft 25, pinned by `memory_usage_leaves_out_payloads`). `Namespace::payload_bytes` estimates them per apply, and core plus estimate came to 103–108 % of the measured heap on the console's sample data and on 100k-node graphs. Checkpointer copies, analytics projections and index builds are counted too, the last two by a formula per node and edge.
- **Policy** (ADR 0054): warn at 0.8, refuse at 0.9, a 5 % band, the default limit from the cgroup on Linux, and a new code `resource_exhausted` rather than `read_only` with a reason, because the two need different advice to clients.
- **One check** in `LoggedNamespace::commit_now` before the WAL append, plus the same `Memory::check_write` before an index build, a namespace creation and an import. Removals, drops, keyed repeats and the system namespace pass.
- **Visible:** `MemoryStatus` with its parts, the limit, the lines, the state and the limit's source; five `iwdb_memory_*` metrics; a log event per change of state. The console uses the server's lines and state (`U.MEMORY_WARN` is gone), in the mock and the REST Source.
- **Tests:** `crates/iwdb-storage/tests/memory.rs` (with failpoints: no refused write reaches the WAL, a commit one byte below the line is durable, the exemptions, the band), `crates/iwdb/tests/memory.rs` (crash and recovery after a refusal, creates and imports, deletes bring writes back, a growing load refused below the limit, copies and projections counted), `crates/iwdb-server/tests/memory.rs` (the error, status and metrics over gRPC and REST), the payload estimate in the commit model's proptest (equal to a recount after every step, and in the replayed copy), and the console's tests.

## Acceptance criteria

- [x] Under a growing write load the server refuses writes at the limit instead of being killed, and recovers when memory falls (`a_growing_write_load_is_refused_below_the_limit_and_resumes_when_memory_falls`, `deleting_brings_the_store_back_below_the_line`). It refuses at `refuse_writes_at` of its counted memory, which is an estimate (see guarantees.md).

## Non-goals

- Per-namespace and per-client memory limits (step 15 builds them on this accounting).
- Evicting data to disk.
