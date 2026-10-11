# Step 15d: Resource limits per client and namespace

Status: done
Milestone: M4 Production 1.0
Depends on: step 15a (the principal limits are counted against), step 16d (memory accounting and the process-wide limit), step 16c (the request registry and metrics)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

One client or namespace over its limits can't starve the others.

## Tasks

- [x] Rate limits per principal (requests per second, token bucket) and concurrency limits (running requests) per principal and per namespace, refused with `resource_exhausted` (the code 16d chose, ADR 0054) before work starts.
- [x] Query budgets per principal: caps on `max_visited`, `max_results` and timeout below the server's caps, set per user or role.
- [x] Max memory per namespace on 16d's accounting: writes to a namespace above its limit are refused before they are logged, reads go on (a fault-injection test as in 16d, design rule 3).
- [x] Limits stored with the users and grants of 15a (one write path), configurable through the admin RPCs and `iwctl`; defaults in `[limits]`.
- [x] Fairness: a client over its limit waits or is refused without holding a namespace's lock or a worker thread.
- [x] Metrics for refusals per principal and namespace (bounded labels: principals are few, or a top-N), from 16c's instrumentation.
- [x] The stress test: one client flooding over its limits, another within them; the second's latency stays within a stated bound and none of its requests fail.
- [x] ADR (limit model and refusal code); errors.md, config.md, guarantees.md.

## Outcome

Decisions in [ADR 0059](../adr/0059-resource-limits-per-client-and-namespace.md).

- **The model.** Per user: a token bucket (`requests_per_second`, `burst`), `max_running`, and query budgets (`max_results`, `max_visited`, `timeout_ms`). Per namespace: `max_running` (every user's calls together) and `max_memory_bytes` (the namespace's graph memory, 16d's per-namespace figure). Each value is unset (the default), 0 (no limit) or a number; a user's or namespace's own value replaces the default, so a trusted service can get more than the default, not only less. Every dimension applies on its own.
- **Refuse, never wait.** `Authorized::check` admits each call through the registry's `Limiter` (`iwdb_query::limits`) when the method is called, before its future exists: a refused call holds no worker, queue slot or namespace lock. Admission is all or nothing under one lock, so a refused call takes no token. Running counts come from the registry: a call counts until its `Call` is dropped. `CancelRequest`, `CancelJob`, `Logout` and `WhoAmI` aren't limited; a `Watch` takes a token for its first round only (`Authorized::following`). The code is `resource_exhausted`; the message names the limit and, for a rate, when to retry.
- **Budgets** travel on `QueryOptions::budget` (set by the authorisation point, never on the wire) and are applied in `LimitConfig::resolve`, so a read asking for nothing gets the lower of the server's default and the budget. They clamp silently, like the server's caps (ADR 0021); a `budget_exceeded` or `timeout` from a read under a budget says the budget applies. Managed jobs' timeouts are capped too.
- **Memory per namespace** is checked next to 16d's limit in `LoggedNamespace::commit_now` and before an index build (`Error::NamespaceMemoryLimit`), with the same exceptions, and without thresholds or hysteresis: a quota, not pressure.
- **Storage:** auth format 2. A user's own limits are a `limits` dict on its node; a namespace's are a node `limits:<namespace id>`. Every change to a format-1 store moves it to format 2 in the same commit; format 1 is still read (`auth-v1/`), and `auth-v2/` is the new fixture (user limits, namespace limits set, changed and dropped, before and after a checkpoint). A user's limits are read with its principal at every request; a namespace's are kept on its open state (`Ns::limits`), loaded at open.
- **Interfaces:** `Accounts::set_user_limits`, `set_namespace_limits`, `limits`; `SetUserLimits`, `SetNamespaceLimits`, `GetLimits` in `AuthService`; `PUT /v1/users/{user}/limits`, `PUT /v1/namespaces/{ns}/limits`, `GET /v1/limits`; both Rust clients; `iwctl limits show|user|namespace` on a directory or a server; three rows in the operation table and the role test. Defaults: `[limits.client]`, `[limits.namespace]` (`QueryConfig::client_limits`, `StoreOptions::namespace_limits`).
- **Metrics:** `iwdb_limit_refusals_total{user, limit}`, bounded by a fixed table of the first 64 users refused plus `(other)` (monotonic series, unlike a top-N; a non-admin sees only its own); `iwdb_namespace_limit_refusals_total{namespace, limit}`; `iwdb_namespace_memory_limit_bytes`. Two suggested alert rules in the operations guide.
- **The stress test** (`crates/iwdb-server/tests/limits.rs`): a server with two workers; one user floods with PageRanks from four connections, retrying every refusal at once, over `max_running` 1 and 20 calls a second; another reads one node every 20 ms within the defaults. Bound: none of the calm user's calls fails, and each takes less than 150 ms. Measured (development machine, debug, the CI size of 20 000 nodes, 3 s): the calm user's slowest call 0.8 to 1.1 ms; the flood admitted 14 to 15 calls and refused about 31 000. A control without limits, same size: median 434 ms, slowest 811 ms. The ignored `stress_measured` (release, 100 000 nodes, 30 s each): without limits p50 167 ms, p99 190 ms; with limits p50 0.19 ms, p99 0.45 ms, slowest 32 ms (with other builds running on the machine), 0 failures.
- **The memory test** (`a_namespace_at_its_memory_limit_refuses_writes_and_serves_reads`, `crates/iwdb/tests/memory.rs`): a namespace over its limit refuses a commit and an index while every write fails by fault injection, and none is attempted; a keyed repeat answers; reads, and writes to another namespace, go on; a crash recovers exactly the accepted commits, and the limit still holds; a delete brings writes back at once.

**Deviations, and why:**

- "Set per user or role": roles here are per-namespace grants, not named groups (ADR 0043), so limits are set per user, and the role that matters is the server-wide admin: the `[limits.client]` defaults apply only to users who aren't admins, so an operator can always act. Named groups with shared limits are a follow-up.
- Python has no user or grant calls, so it has no limit calls either; it maps the new refusals to `ResourceExhaustedError` as before.
- No structured retry hint: `Error` is a code and a message across every adapter, so the wait is in the message. A `retry_after_ms` field (and `Retry-After` over REST) is a follow-up.
- `max_edges` isn't a budget (step 15d names `max_visited`, `max_results` and the timeout); adding it is a new key, so a format bump.

**Follow-ups:** named groups of users with shared limits; a structured retry hint; `max_edges` as a budget; the console showing limits; limits across a cluster (step 18).

## Acceptance criteria

- [x] One client over its limits can't starve another (stress test): `one_client_over_its_limits_cannot_starve_another`.
- [x] A namespace above its memory limit refuses writes and keeps serving reads; a refused write leaves nothing in the WAL: `a_namespace_at_its_memory_limit_refuses_writes_and_serves_reads`.

## Non-goals

- Billing or quota accounting over time.
- Limits across a cluster (step 18).
