# ADR 0020: The `Database` trait: crate direction, and async over a synchronous engine

Status: accepted
Date: 2026-10-03

## Context

Step 10 adds the service interface every access method uses (design rule 8): embedded Rust, Python, gRPC (step 11), REST (step 12), and the clients of step 14. Two questions shape it.

**Where the query logic lives.** `iwdb` is the embedded facade: `Store`, namespaces (`Ns`), the commit pipeline over the WAL, deadlines (ADR 0016). The reads (`find`, `neighbourhood`, `match`, ...) need only a namespace's graph and catalog. Either `iwdb-query` depends on `iwdb` and implements the trait on `Store`, or the trait and the reads live in `iwdb-query` and `iwdb` implements the trait. Rule 8 wants each operation implemented once; rule 1 keeps `pyo3` in `iwdb-python` only.

**How async methods run a synchronous engine.** A read holds the namespace's `RwLock` and runs the core's loops under a *thread-local* cancel token (`cancel::run`); a commit blocks on an fsync. Neither may run on an async executor's threads. The trait must be usable from tokio (server), from Python (no event loop), and from tests.

## Decision

**Crate direction: `iwdb` depends on `iwdb-query`.** `iwdb-query` depends on the core, `iwdb-engine` and `iwdb-storage` (for error mapping, history ids and namespace types), never on `iwdb`. It holds:

- the `Database` trait, the request and answer types, the error model (`Code`, documented in `documentation/api/errors.md`), limits and cursors (ADR 0021);
- the read operations as plain functions over `&Namespace` plus a `ReadContext` (`iwdb_query::read`). They don't lock or wait; the caller holds the read lock and the cancel token;
- the conformance suite (feature `conformance`), written against the trait only.

`iwdb` implements the trait as `iwdb::Embedded` (wrapping `Arc<Store>`): it resolves limits, waits for `min_seq`, takes the read lock and runs the `read::*` function. The shared value types (`Node`, `Edge`, `NamespaceStatus`, `CommitOptions`, `ProjectionSpec`, ...) moved to `iwdb-query` and are re-exported by `iwdb`, so each has one definition. A remote client (steps 11, 12, 14) implements the same trait without pulling in storage code beyond the shared types, and the server serves any `D: Database`.

**Async without a runtime dependency.** The trait's methods return `impl Future<Output = ...> + Send` (return-position `impl Trait` in traits, stable since Rust 1.75, our MSRV is 1.85). No `async-trait` crate and no boxing; the price is that the trait isn't object safe, so adapters take it as a generic (`Server<D: Database>`). `Send` is in the signature so tokio can spawn the futures.

`Embedded` runs every request on its own worker pool (`iwdb_query::exec::Pool`: N threads, default the available parallelism, a bounded queue, default 1024, beyond which requests fail with `unavailable`). `Pool::submit` returns a `Pending<T>` future (a slot and a waker, std only). For callers without an executor, `exec::block_on` parks the thread until the future is ready (Python releases the GIL around it). No async runtime is a dependency of any library crate; step 11 adds tokio to the server binary only.

**Deadlines and cancellation.** The timeout counts from the call (queueing included). The worker passes what is left of it to the store's `ReadOptions`, whose timer cancels the request's token at the deadline (ADR 0016); a deadline already passed fails at once with `timeout`. `Ns::read_with` now runs its closure under `cancel::run` with that token, so the deadline bounds the read itself, not only the `min_seq` wait; the core's loops stop at their next check, the partial result is dropped, and the read fails with `timeout`. Dropping a `Pending` cancels its token, so a read whose caller went away (a dropped gRPC stream, a cancelled task) stops too.

Commits run on the same workers, without a timeout (ADR 0016): dropping a commit's future doesn't undo it; its outcome is then unknown and an idempotency key makes the retry safe.

## Consequences

- One implementation per operation: `iwdb::Embedded` only translates (namespace lookup, limits, deadline, error mapping), and Python calls the trait (`iwdb-python` is moved onto it). The conformance suite runs unchanged against every implementation.
- Reads still hold the namespace's read lock while they run (no MVCC): commits wait for them. The budgets (ADR 0021) keep that short; analytics run on a projection without the lock.
- Every request costs a thread hop (tens of microseconds). The pool is a concurrency limit too; per-client limits come with step 15.
- One pool serves reads and commits, so a flood of slow reads can delay commits in the queue. If that shows in step 14's benchmarks, give commits their own workers.
- Python's `timeout=inf` keeps meaning "none": the bindings configure `LimitConfig::max_timeout` to `Duration::MAX`. A server keeps the default cap (5 minutes).

## Update (step 11): deadlines while queued

Testing the gRPC server found that a read waiting in the pool's queue didn't fail at its deadline: the timeout was checked when a worker took the job, so a read queued behind a slow one waited until that one ended (21 s instead of 200 ms in the test), and a remote client without a deadline of its own waited with it. `Pool::submit_until` now takes the deadline: a timer thread per pool cancels the job's token and resolves its future with `timeout` at the deadline, whether the job is running or still queued; a queued job then runs with a cancelled token and does nothing. `Embedded` submits every read, `wait_for_seq` and `analyze` with its deadline; commits have none. `a_read_waiting_for_a_worker_times_out_at_its_deadline` (`iwdb/tests/query.rs`) and `a_queued_job_ends_at_its_deadline` (`exec.rs`) test it.
