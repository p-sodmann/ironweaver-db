# Step 16f: Managed analytics jobs

Status: todo
Milestone: M4 Production 1.0
Depends on: steps 16c (registry, cancel, status) and 16e (`iwctl` against a server)

Split out of step 16 on 2026-10-04 (see its "Plan change"); moved to step 16 from step 10 by [ADR 0022](../adr/0022-analytics-jobs.md).

## Goal

Analytics that outlive a request's timeout: started, watched, cancelled and collected by id.

## Tasks

- [ ] A job registry: start a job (the `Job`s of `Database::analyze`), get its state and progress, cancel it, fetch its result; results kept for a configured time and capped in count and bytes.
- [ ] Progress from the core's algorithms where it reports any; otherwise phases only (collecting, running, done). If the core lacks a progress hook we need, file it upstream (AGENTS.md).
- [ ] Proto, REST, OpenAPI; `iwctl jobs list|show|cancel|result`; the jobs in the status views and the console.
- [ ] Jobs count against the memory limit (step 16d).
- [ ] ADR (extends ADR 0022).

## Acceptance criteria

- A job longer than the maximum request timeout runs to the end, reports progress, can be cancelled, and its result can be fetched until it expires.

## Non-goals

- Persisting jobs or results across restarts.
