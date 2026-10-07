# ADR 0056: Managed analytics jobs

Status: accepted
Date: 2026-10-06
Extends: [ADR 0022](0022-analytics-jobs.md)

## Context

ADR 0022 gave analytics a synchronous form, `Database::analyze`, bounded by the request's timeout (5 minutes at most by default), and deferred managed jobs: started, watched, cancelled and collected by id, so that a job can outlive any request. Step 16f asks for them, on top of the request registry and cancel of step 16c (ADR 0052) and the admin writes of step 16e (ADR 0055).

Points to decide: where jobs live and run, their ids, who may do what, their states and progress, their bounds and what happens at each, how results are fetched, how they count against the memory limit (ADR 0054), and what drain and restart do to them.

**What the pinned core (`7e7b7fa`) reports while an algorithm runs: nothing.** Its algorithms fetch the thread's cancel flag (`cancel::stop()`) and check it in their loops; that is all they read from outside. PageRank, label propagation and Leiden iterate, but nobody can see which iteration they are on. Even the poll hook of `cancel::run_polling` isn't called by them: they check `Stop::requested`, not `Stop::poll` (pinned by `algorithms_report_no_progress`, `core_smoke.rs`). So a job can report its phase, not how far its algorithm has got. Filed as [upstream #62](https://github.com/p-sodmann/Ironweaver/issues/62) ([draft 26](../upstream-issues.md#26-algorithms-report-no-progress-while-they-run)).

*Update, upstream check of 2026-10-07: #62 was fixed in core `c69ef51`. Algorithms now report to a `cancel::Progress` given with `cancel::run_with_progress`: a phase, the units done and their total. Jobs read it; see "States and progress".*

## Decision

### Jobs are `Admin` methods, implemented once

Five methods on the `Admin` trait (ADR 0051), implemented once in `iwdb::Embedded`, translated by gRPC (`AdminService`), REST, both Rust clients, `iwctl` and the console, which decide nothing (design rule 8):

| Method | RPC / REST | What |
|---|---|---|
| `start_job(namespace, AnalyticsRequest, QueryOptions, owner)` | `StartJob`, `POST /v1/namespaces/{ns}/jobs` | queue the job; answer at once with its id and state |
| `jobs(user, limit)` | `ListJobs`, `GET /v1/jobs` | the jobs kept, newest first |
| `job(id, user)` | `GetJob`, `GET /v1/jobs/{id}` | one job's state and progress |
| `cancel_job(id, user)` | `CancelJob`, `POST /v1/jobs/{id}/cancel` | cancel a queued or running job |
| `job_result(id, user, offset, limit)` | `GetJobResult`, `GET /v1/jobs/{id}/result` | a page of a finished job's rows |

**The owner** is an argument because only the authorisation point knows the caller: `Authorized` always sets it to its principal's user and client, whatever it is given, and the clients don't send it (their server's `Authorized` sets it). Called in-process without `Authorized`, a job belongs to the unauthenticated principal.

`Admin`, not `Database`: `Database` is the data API every access method and the Python bindings share. Jobs are something an operator lists and cancels beside the running requests, with which they share ids (below). An embedded application has no server timeout to outlive: it calls `analyze` with the timeout it chooses.

**The registry and its bounds are pure Rust, in `iwdb_query::jobs::Jobs`**: entries, states, the queue, the caps, retention, stored results and their pages. It also owns the threads that run jobs. `Embedded` gives it the work: a closure that runs the job through `Ns::analyze` and `read::run_job`, the same code as `analyze`. No adapter holds any of it.

**The `Job` enum stays as it is.** A job is the `AnalyticsRequest` that `analyze` takes, with the same projection, the same `max_visited` and `max_edges` checks (the projection must fit), and the same ranking and `max_results` (ADR 0022).

### Ids: one space with the requests

A job's id comes from the request registry's counter (`Requests::next_id`, ADR 0052), so a request and a job never share an id, and `iwctl cancel <id>` and `iwctl jobs cancel <id>` mean the same thing for a job.

**While a job is queued or running, `ListRequests` lists it** as a `StartJob` request with the job's id, owner, client, namespace and creation time, `cancellable: true`. `CancelRequest` on that id cancels the job exactly like `CancelJob`. Once the job has ended it leaves the request list and stays in the job list. Jobs aren't counted in the request metrics when they end (the `StartJob` call that created one already was, and jobs have their own metrics).

Ids are unique while the server runs, not across restarts (ADR 0052). A client that keeps an id over a restart can find a new job under it, its own or (as `not_found`) someone else's: it compares the job's `created` time.

### Who may: the operation table

| Operation | Requirement | Audited | Cancellable |
|---|---|---|---|
| `StartJob` | `read` on the namespace | always | no (it answers at once; the job is cancelled by its own id) |
| `ListJobs` | authenticated; a non-admin sees only its own | refusals | yes |
| `GetJob` | authenticated; its owner or a server admin, `not_found` for others | refusals | yes |
| `CancelJob` | authenticated; its owner or a server admin, `not_found` for others | always | no |
| `GetJobResult` | authenticated; its owner or a server admin, `not_found` for others; and `read` on the job's namespace now | refusals | yes |

- **Starting needs `read`**, like `Analyze`: a job reads what `analyze` reads.
- **Others' jobs are `not_found`**, as others' requests are for `CancelRequest`: a non-admin learns nothing about jobs that aren't theirs.
- **Fetching a result checks the role again.** A result holds node ids of the namespace. If the owner has lost `read` on it since starting the job, the fetch is `permission_denied` (and audited as a refusal). State and progress hold no data and stay visible to the owner.
- **Starting and cancelling are audited**, every call: a job runs for long and holds memory, and who started or stopped one is worth knowing. The entry names the namespace and, as `request`, the job's id (the field `CancelRequest` uses); a cancel's entry also names the owner as `subject`. Listing and fetching are reads: only refusals are audited (ADR 0049).

### States and progress

| State | Meaning |
|---|---|
| `queued` | waiting for a job thread |
| `collecting` | waiting for `min_seq`, then collecting the projection under the namespace's read lock |
| `running` | the algorithm runs on the projection, without a lock, then the rows are ranked |
| `done` | the result is stored and can be fetched |
| `failed` | it ended with an error (its code and message are kept): `budget_exceeded`, `timeout` (the job's own limit), `not_found` (the namespace was dropped), a projection or algorithm error |
| `cancelled` | by `CancelJob` or `CancelRequest`, or because the server drains |
| `expired` | it was `done`, and its result was dropped to make room for newer results (below) |

**Progress is the phase**, with what is known by then: the projection's node and edge counts once collecting starts, the seq it saw and the number of rows once done, and the creation, start and end times. The core reports nothing from inside an algorithm, so there is no fraction. When upstream #62 lands, a progress field per iteration is added (a new proto field, a minor change).

*Update, 2026-10-07 (core `c69ef51`, #62 fixed):* `JobInfo` has `progress`: the core's `phase` (`pagerank`, `leiden`, `label propagation`, ...), the units `done` in it (iterations for PageRank and label propagation, runs for Leiden, nodes or sources for the others) and their `total` if known. The job closure runs the algorithm through `Ns::analyze_reporting`, which runs it under `cancel::run_with_progress` with the job's `Progress`, and the registry reads `Progress::snapshot` whenever it reports the job. When the job ends, the last report is kept (a cancelled job's thread may go on briefly; what it reports after that isn't shown). It is absent before the algorithm's first report: while queued and collecting, and for work that reports none. It is the core's count, not a time estimate: PageRank and label propagation can end below their total, and Leiden's runs differ in length. Proto field 19 (`JobProgress`), a minor change; `iwctl jobs` and the console's jobs table show it.

A cancel takes effect at once in the registry: the job is `cancelled` when `CancelJob` answers, and its thread stops at the core's next check of the token (for a queued job: it never starts). A job that ended just before the cancel keeps its outcome; the cancel then answers with it, as a request's cancel does with a finished answer (ADR 0052).

### Where jobs run, and their bounds (design rule 5)

**Jobs run on threads of their own**, `[jobs] running` of them (default 2), not on the query workers. A long job never holds a worker, so it can't starve reads; the query pool stays the request path. Two, because the core's PageRank, components and Leiden use the process's rayon pool: two jobs already keep many cores busy.

| Setting (`[jobs]`) | Default | At the cap |
|---|---|---|
| `running` | 2 | further jobs wait in the queue |
| `queued` | 16 | `StartJob` fails with `unavailable` ("too many jobs: 16 are queued") |
| `per_user` | 4 | `StartJob` fails with `unavailable` when the caller has this many queued or running (server admins included) |
| `timeout_secs` | 3600 | the job fails with `timeout`; a request's `timeout` can only lower it |
| `retention_secs` | 3600 | a job that ended this long ago is removed: `not_found` |
| `max_finished` | 100 | the oldest ended job is removed when one more ends: `not_found` |
| `result_bytes` | 64 MiB | the oldest stored results are dropped (their jobs `expired`) to make room for a new one; a result alone larger than this fails its job with `budget_exceeded` |

- **`unavailable`, not `resource_exhausted`**, for full queues: like the worker pool's full queue, it says "busy, retry later". `resource_exhausted` is the memory refusal of writes (ADR 0054).
- **A job's timeout counts from when it leaves the queue.** The wait in the queue is bounded by the queue and the running jobs' timeouts. `0 < timeout_secs`, and `running`, `queued`, `per_user` and `max_finished` are at least 1.
- **A result's size is estimated** from its rows: the ids' lengths plus a fixed overhead per row and per id (as `String`s in a `Vec`), so a result counts about what it holds.
- **Expiry is checked whenever the registry is used** (a start, a list, a get, a fetch, the metrics, the status), not by a timer. A result may outlive its retention until the next such call; it still counts in memory until then, and is never served after its time.

### Results: the same rows, paged

A `done` job's result holds the rows `analyze` would answer: ranked, at most `max_results` (the request's, capped by `[limits]`), with `truncated` if rows were cut. `GetJobResult(id, offset, limit)` answers rows `offset..`, at most `limit` (default and maximum 10 000), and stops early before an estimated 4 MiB (at least one row), with the offset of the next page if there are more. Results don't change once stored, so pages are consistent.

Fetching a job that isn't `done` fails: `invalid_argument` while it is queued or running ("job 7 is running"), the job's own error for a failed job, `cancelled` for a cancelled one, and `not_found` for an expired or removed one ("job 7's result has expired").

### Memory (ADR 0054)

- **A job's projection counts in `working`** while it is collected and while the algorithm runs, exactly as for `analyze` (`Ns::analyze` charges it).
- **Stored results count in `working` too**, by their estimate, until they expire or are dropped. They are capped by `result_bytes`, so with the defaults jobs add at most two projections and 64 MiB.
- **Starting a job while the server refuses writes is allowed.** Reads are (ADR 0054), and a job is the remote form of `analyze`. What it adds is bounded by `running` and `result_bytes`, and counted, so it can push memory up only by a known amount. Refusing would take analytics away just when an operator investigates. `Verify` is refused there because it builds an uncounted copy of a whole namespace; a job doesn't.

### Drain and restart

- **Drain cancels every queued and running job** as it starts (when the server turns readiness off, `Admin::set_ready(false)`), with the message "the server is shutting down", and refuses `StartJob` with `unavailable`. During the unready delay and the drain a client can still read the jobs; they show `cancelled`. Closing the database joins the job threads, which stop at the core's next check, so the drain still ends in time.
- **Nothing persists** (the step's non-goal). After a restart every job is gone: `GetJob` answers `not_found`. A client that needs the result starts the job again.

### Visible

- **`GetServerStatus`** gains `jobs`: queued, running, kept (ended and still listed), and the stored results' bytes.
- **Metrics** (bounded labels, ADR 0050): `iwdb_jobs_queued` and `iwdb_jobs_running` (gauges), `iwdb_jobs_total{outcome}` (`done`, `failed`, `cancelled`), and `iwdb_job_result_bytes` (gauge).
- **The console's status page** gets a jobs table (id, namespace, kind, user, state, elapsed, rows) with a cancel button for queued and running jobs. The mock `Source` answers the same shapes.
- **`iwctl jobs list|show|cancel|result`** with `--server` only: jobs live in a running server's memory, and a data directory has none. Without `--server` the command is refused at parsing (exit code 2). It has no `start`: a job's request (projection, job, options) is built through the API or the Rust clients.
- **Python: not now.** The bindings have no `Admin` methods at all (status, requests). An embedded Python application runs `analyze` with its own timeout; the remote client gains jobs with the rest of `Admin`.

## Consequences

- An analytics job can run for up to an hour by default (longer if configured), past any request timeout, and be watched, cancelled and collected over gRPC, REST, `iwctl --server` and the console.
- ~~Progress is by phase only until upstream #62.~~ Since core `c69ef51` an operator sees how far a job's algorithm has got, in the core's units.
- Jobs and their results are lost on restart and on drain. A client that needs them restarts them.
- Five new rows in the operation table and the role test. `CancelRequest` reaches jobs too, through the registry's cancel hook.
- The memory jobs hold is bounded and counted in `working`: `running` projections plus `result_bytes`.
