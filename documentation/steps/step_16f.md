# Step 16f: Managed analytics jobs

Status: done
Milestone: M4 Production 1.0
Depends on: steps 16c (registry, cancel, status) and 16e (`iwctl` against a server)

Split out of step 16 on 2026-10-04 (see its "Plan change"); moved to step 16 from step 10 by [ADR 0022](../adr/0022-analytics-jobs.md).

Starts with the [upstream check](upstream-check.md): the core's algorithms report no progress ([#62](https://github.com/p-sodmann/Ironweaver/issues/62), filed in this step), so jobs report phases only until it is fixed.

## Goal

Analytics that outlive a request's timeout: started, watched, cancelled and collected by id.

## Tasks

- [x] A job registry: start a job (the `Job`s of `Database::analyze`), get its state and progress, cancel it, fetch its result; results kept for a configured time and capped in count and bytes.
- [x] Progress from the core's algorithms where it reports any; otherwise phases only (collecting, running, done). If the core lacks a progress hook we need, file it upstream (AGENTS.md).
- [x] Proto, REST, OpenAPI; `iwctl jobs list|show|cancel|result`; the jobs in the status views and the console.
- [x] Jobs count against the memory limit (step 16d).
- [x] ADR (extends ADR 0022): [ADR 0056](../adr/0056-managed-analytics-jobs.md).

## Acceptance criteria

- A job longer than the maximum request timeout runs to the end, reports progress, can be cancelled, and its result can be fetched until it expires.

## Outcome

- The registry is `iwdb_query::jobs` (pure Rust, threads of its own); jobs are five `Admin` methods implemented once in `iwdb::Embedded`, over gRPC (`AdminService`), REST, both Rust clients, `iwctl --server ... jobs` and the console's status page. Python doesn't get them: its bindings have no `Admin` methods yet, and an embedded application runs `analyze` with its own timeout.
- Progress is the phase (and the projection's size): the core reports nothing from inside an algorithm. Filed as [#62](https://github.com/p-sodmann/Ironweaver/issues/62) ([draft 26](../upstream-issues.md#26-algorithms-report-no-progress-while-they-run)), pinned by `algorithms_report_no_progress`; the [upstream check](upstream-check.md) lists what to change when it lands.
- The acceptance criterion is `a_job_outlives_the_request_timeout_over_grpc` and `..._over_rest` (`crates/iwdb-server/tests/jobs.rs`), with a 50 ms maximum request timeout.

## Non-goals

- Persisting jobs or results across restarts.
