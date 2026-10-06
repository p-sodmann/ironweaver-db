# Step 15d: Resource limits per client and namespace

Status: todo
Milestone: M4 Production 1.0
Depends on: step 15a (the principal limits are counted against), step 16d (memory accounting and the process-wide limit), step 16c (the request registry and metrics)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

One client or namespace over its limits can't starve the others.

## Tasks

- [ ] Rate limits per principal (requests per second, token bucket) and concurrency limits (running requests) per principal and per namespace, refused with `resource_exhausted` (the code 16d chose, ADR 0054) before work starts.
- [ ] Query budgets per principal: caps on `max_visited`, `max_results` and timeout below the server's caps, set per user or role.
- [ ] Max memory per namespace on 16d's accounting: writes to a namespace above its limit are refused before they are logged, reads go on (a fault-injection test as in 16d, design rule 3).
- [ ] Limits stored with the users and grants of 15a (one write path), configurable through the admin RPCs and `iwctl`; defaults in `[limits]`.
- [ ] Fairness: a client over its limit waits or is refused without holding a namespace's lock or a worker thread.
- [ ] Metrics for refusals per principal and namespace (bounded labels: principals are few, or a top-N), from 16c's instrumentation.
- [ ] The stress test: one client flooding over its limits, another within them; the second's latency stays within a stated bound and none of its requests fail.
- [ ] ADR (limit model and refusal code); errors.md, config.md, guarantees.md.

## Acceptance criteria

- One client over its limits can't starve another (stress test).
- A namespace above its memory limit refuses writes and keeps serving reads; a refused write leaves nothing in the WAL.

## Non-goals

- Billing or quota accounting over time.
- Limits across a cluster (step 18).
