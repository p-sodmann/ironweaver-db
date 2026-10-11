# ADR 0059: Resource limits per client and namespace

Status: accepted
Date: 2026-10-11

## Context

Step 15d asks that one client or namespace over its limits can't starve the others. The pieces it builds on:

- the principal of every remote call, resolved by the gate and checked once by `Authorized` (ADRs 0043 to 0045);
- the request registry, where every call that passes `Authorized` is registered while it runs (ADR 0052);
- the memory accounting and its refusal before the WAL (ADR 0054), with the per-namespace figure `Graph::memory_usage`;
- the bounded reads, whose limits a client may ask for and the server caps (ADR 0021);
- the metrics without a library (ADR 0050).

Points to decide: what is limited and per what, how a user's value, a role and the `[limits]` defaults combine, whether an over-limit call waits or is refused and what a refusal carries, how query budgets clamp and report, where limits are stored and how their format changes, which metrics count refusals and how their labels stay bounded.

## Decision

### What is limited

Per **principal** (a user), `ClientLimits`:

| Limit | Meaning |
|---|---|
| `requests_per_second`, `burst` | A token bucket: it holds up to `burst` tokens (default: `requests_per_second`) and gains `requests_per_second` a second. Each call admitted takes one. |
| `max_running` | Calls of the user running at once (registered in the request registry), over every namespace. |
| `max_results`, `max_visited`, `timeout_ms` | Query budgets: caps on what a read may ask for, below the server's caps. |

Per **namespace**, `NamespaceLimits`:

| Limit | Meaning |
|---|---|
| `max_running` | Calls on the namespace running at once, of every principal together. |
| `max_memory_bytes` | The namespace's graph memory (`Graph::memory_usage`, payloads and indexes included: 16d's per-namespace figure). Writes that add are refused while it is at or above this. |

Not limited: `max_edges` (the budgets cover what step 15d names; adding it later is a new key in the stored dict, so a format bump, since a reader refuses keys it doesn't know rather than enforce less than was set), bytes per second, and anything over time (billing and quotas are non-goals).

**Which calls count.** Every call through the authorisation point, except those that end work or only identify: `CancelRequest`, `CancelJob`, `Logout` and `WhoAmI` (`Operation::limited`). So a client over its limits can still cancel its own requests and log out. `Login` doesn't pass `Authorized` and keeps its own slowdown (ADR 0044). A `Watch` (and the SSE route) takes a token for its first round only, since it is one request; each round counts as running while it runs, its long-poll wait included. Calls on the embedded store in-process aren't limited, as they aren't registered (ADR 0052).

### How the values combine

- **A stored value is unset, 0 or a number.** Unset: the default applies. 0: no limit, whatever the default. A number: that limit.
- **A user's own value replaces the default.** The more specific setting wins, so an operator can give a trusted service more than the default, as well as less. Taking the most restrictive of the two was considered: it would make the default a ceiling nobody could be raised above.
- **The `[limits.client]` defaults apply to users who aren't server admins.** Roles in this database are per-namespace grants (ADR 0043), not named groups, so the role that limits can be set "per" is the server-wide admin: admins get only the limits set on them, so an operator can still act on a loaded server, as the memory limit still lets logins through (ADR 0054). Named groups of users with shared limits are a follow-up. With authentication off there are no users, and only namespace limits apply.
- **Every dimension applies independently.** A call must pass the user's rate and concurrency limits and the namespace's concurrency limit. A read's budget is the user's, below the server's caps.
- **Namespace values replace the `[limits.namespace]` defaults** the same way.

### Refuse at once, never wait

Every over-limit call is **refused when it is checked**, with `resource_exhausted` (ADR 0054's code). The check runs in `Authorized::check`, synchronously, when the method is called: before the inner call's future is built, so before a worker is asked for, a queue slot is taken or a namespace lock is touched. A refused call holds nothing.

Waiting was rejected. A waiting call holds a connection, a task and its request in memory; bounded waits per user need a timer and a fair order between waiters; and the waiting would itself be work done for the flooding client. Clients already retry `resource_exhausted` with backoff (ADR 0054), and the idempotency keys make retried commits safe.

**What a refusal carries.** The code, and a message naming the limit, the user or namespace, and when to retry: "user 'ann' is over its rate limit of 50 requests per second (burst 50): retry in 20 ms", "user 'ann' has 4 requests running, its limit (max_running): retry when one ends", "namespace 'social' has 32 requests running, its limit (max_running)". There is no structured retry field: `Error` is a code and a message across every adapter, and adding a field is an API change of all of them. That is a follow-up (a `retry_after_ms` on the error, and `Retry-After` over REST).

**Admission is all or nothing.** The user's running count, the namespace's running count and the bucket are checked under the limiter's lock, and only if all pass are the counts raised and a token taken. A refused call takes no token, so a flooding client still gets exactly its rate admitted. The counts fall when the call's registration ends (its `Call` is dropped), whatever the outcome.

### Query budgets clamp silently

`Authorized` lowers a read's `max_results`, `max_visited` and `timeout` to the caller's budget before it delegates: an asked value above the budget is lowered to it, and a missing one becomes the budget if the server's default is higher. That is what the server's caps already do (ADR 0021: "a limit above the cap is lowered to it"), so a clamped read reports what any bounded read reports: `budget_exceeded`, or `truncated: true` with `partial`, or `timeout`. When such an error comes from a read whose limits the budget lowered, its message says so ("... (user 'ann''s budget caps max_visited at 1000)"), so the caller knows that asking for more won't help. Budgets apply to every read that takes `QueryOptions`, `WaitForSeq`, the change stream, `GetCatalog`, `GetSchema` and `StartJob` included.

### Memory per namespace

The namespace's limit is checked where the process-wide one is (ADR 0054), in `LoggedNamespace::commit_now` after the commit is prepared and the idempotency key looked up, before the WAL append; and before an index build starts. While the namespace's memory is at or above its limit:

- data commits that add anything, and catalog changes that add an index or a constraint, are refused (`NamespaceMemoryLimit`, code `resource_exhausted`);
- commits that only remove, repeated keyed commits, dropping indexes and constraints, dropping the namespace, and every read go on.

There are no thresholds and no hysteresis. This is a quota for one namespace, not pressure on the process: the process-wide limit's bands stop the server's state, and its log, from flipping with every commit, while a namespace at its quota simply accepts deletes and then writes again. A commit admitted below the limit can carry the namespace past it by its own size, as with ADR 0054. The system namespace can't be limited.

### Storage: auth format 2

Limits are stored with the users and grants, in the system namespace, and changed through `Users` with one commit each, under the auth mutex (ADR 0043). No second store.

- A `user:<name>` node may have **`limits`**: a dict with any of `requests_per_second`, `burst`, `max_running`, `max_results`, `max_visited`, `timeout_ms`, each an `Int` of at least 0. A key that is absent is unset.
- A namespace's limits are a node **`limits:<namespace id>`** (label `NamespaceLimits`) with `max_running` and `max_memory_bytes`, each an optional `Int` of at least 0. Like grants, they name the namespace by id: a namespace dropped and created again under its name starts without limits. A node whose namespace isn't live is ignored, and removed by the next change of namespace limits.
- **The format becomes 2.** Every change to a store in format 1 rewrites the `auth` node to `{format: 2}` in the same commit, so the format is bumped all or nothing, together with the first change made by a 15d binary. A format-1 store reads as having no limits, and the reader of format 1 stays. A binary from before 15d refuses a format-2 store with `corrupt` ("the store's users are in format 2, this version reads format 1"), never misreads it; downgrading a store with users isn't supported (ADR 0043).
- The fixture `crates/iwdb/tests/fixtures/auth-v2/` holds format 2 with user and namespace limits, in a checkpoint and in the WAL. `auth-v1/` is still read, and the test checks that its first change makes it format 2.

**When limits take effect.** A user's limits are read with its principal, at every request (ADR 0044 reads the user's node anyway), so a change applies to the next request. A namespace's limits are kept on its open state: loaded when the store opens, set when they change, and the defaults when a namespace is created.

### Defaults

`[limits.client]` (`requests_per_second`, `burst`, `max_running`, `max_results`, `max_visited`, `timeout_ms`) and `[limits.namespace]` (`max_running`, `max_memory_bytes`), every value unset by default: a server upgraded to 15d limits nothing until its operator says so. In the embedded API they are `QueryConfig::client_limits` and `StoreOptions::namespace_limits`.

### The admin calls

Three operations, rows of the operation table (ADR 0045), on the `Accounts` trait next to users and grants, and in `AuthService`:

| Operation | RPC / REST | Requirement | Audited |
|---|---|---|---|
| `SetUserLimits` | `SetUserLimits`, `PUT /v1/users/{user}/limits` | server admin | always |
| `SetNamespaceLimits` | `SetNamespaceLimits`, `PUT /v1/namespaces/{ns}/limits` | server admin | always |
| `GetLimits` | `GetLimits`, `GET /v1/limits` | authenticated; namespaces narrowed to the caller's | refusals |

A set replaces all of a user's or namespace's limits; a field left out is unset. `GetLimits` answers the defaults and the namespaces' own limits; a user's own limits are in `ListUsers`, and `WhoAmI` answers the caller's effective ones. `iwctl limits` makes these calls on a data directory or a server; the Rust clients have them. The Python package has no user or grant calls, so none for limits either.

A limit refusal is not an authorisation refusal, so it is audited only for operations audited `Always` (ADR 0049): a flood of reads doesn't flood the audit log.

### Metrics with bounded labels

- `iwdb_limit_refusals_total{user, limit}`, `limit` one of `rate` and `running`. **Labels stay bounded by a fixed table**: the first 64 users refused get a series each, for as long as the server runs, and refusals of any later user are counted under `user="(other)"`. Every series is monotonic, which a top-N recomputed at each scrape wouldn't be (a user falling out of the top would make `(other)` jump). Users are created only by server admins, so 64 covers most servers; past that the totals are still right. A caller who isn't a server admin sees only its own series.
- `iwdb_namespace_limit_refusals_total{namespace, limit}`, `limit` one of `running` and `memory`, for live namespaces, narrowed like every namespace series (ADR 0050).
- `iwdb_namespace_memory_limit_bytes{namespace}`, for namespaces with a memory limit.
- Refusals are also counted in `iwdb_requests_total` with code `resource_exhausted`.

This amends ADR 0050's "never a user" for the one metric above, bounded as described.

## Consequences

- One client over its limits can't take another's workers, queue slots or namespace locks: it is refused before it gets any. The stress test (`crates/iwdb-server/tests/limits.rs`) floods a server of two workers with one user's PageRanks from four connections, retrying every refusal at once, while another user reads within its limits every 20 ms. None of the second user's calls fails, and each answers within 150 ms: about 1 ms measured, against 0.4 to 0.8 s in a control run without limits.
- A namespace at its memory quota refuses writes and keeps serving reads; a refused write leaves nothing in the WAL (`a_namespace_at_its_memory_limit_refuses_writes_and_serves_reads` in `crates/iwdb/tests/memory.rs`, by fault injection, as in 16d).
- Format 2 of the users' records: a store changed by 15d can't be opened by an older binary.
- Each admitted call costs one more mutex-protected map update, next to the registry's.
- Waiting, structured retry hints, named groups and limits across a cluster (step 18) are not provided.
