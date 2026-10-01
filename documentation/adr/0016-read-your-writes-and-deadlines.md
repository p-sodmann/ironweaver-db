# ADR 0016: Read-your-writes (`min_seq`), deadlines and cancellation

Status: accepted
Date: 2026-10-01

## Context

Step 8 asks for `min_seq` on reads (wait, with a timeout, until a seq is applied) and for requests that run under a `cancel::Token` with a timer that cancels them at their deadline. Step 10's `Database` trait and steps 11–14's servers and clients build on both: a client tracks the seq of its last commit and sends it with its reads, which a server (or, in step 18, a replica) may not have applied yet.

Facts:

- The core's algorithms check a thread-local cancel token (`cancel::run`): `Stop::requested` is an atomic load, also inside rayon workers. Nothing in the core sets a token on a deadline; someone has to.
- A seq is meaningful only within one history: after a restore to `N`, commits `N + 1, ...` are not the original's (ADR 0009).
- In the embedded store a commit is applied before it returns, so a client's own `min_seq` is always reached at once. Waiting matters for seqs from elsewhere (another thread's commit in flight, a server, a replica).

## Decision

- **`ReadOptions`** (`iwdb::ReadOptions`): `min_seq`, `history`, `timeout` (`None`: 30 s, `DEFAULT_TIMEOUT`; `Duration::MAX`: none) and `cancel` (a token the caller may cancel). `Store::read_with`, `wait_for_seq` and `analyze` take them. Step 10 moves them behind the trait, with server defaults and caps.
- **Waiting**: `LoggedNamespace` publishes the applied seq after each apply and wakes waiters (a condition variable). A waiter returns as soon as its seq is applied, fails with `Timeout` at the deadline, with `Cancelled` when the token is cancelled (checked every 10 ms), and at once with `ReadOnly` if the store is read-only below the seq: it will never get there, so waiting would only hide the failure.
- **Another history**: `min_seq` may come with the history id it belongs to. If that differs from the store's, the read fails with `OtherHistory` instead of waiting or reading: the seq says nothing about this store, and a seq at or below the store's would otherwise silently read a state without the client's write. Without a history id the caller vouches for the seq. The step 14 client sends both (the history from `status`).
- **Deadlines**: the store has a timer thread (started on first use) holding deadlines in a sorted map; each request registers its token and deadline and unregisters when it ends (a guard), and the thread cancels tokens whose deadline passed. A request runs on the caller's thread — in a server, a blocking thread (`spawn_blocking`) — under `cancel::run`, so the core's loops stop at their next check, and an interrupted job fails with `Timeout` (or `Cancelled` if the caller cancelled it and the deadline hadn't passed). One thread per store rather than one per request; `std` only.
- **Commits** take no timeout yet: a commit can't be abandoned safely once its record may be in the log, and waiting for the writer is bounded by the commit before it. Step 10 can bound the wait for the writer's lock.

## Consequences

- Read-your-writes works across threads and, from step 11, across connections: a reader waits for the writer it depends on, at most until its deadline.
- A stuck analytics job ends at its deadline without killing a thread; the store stays usable.
- Python's `close` waits for calls in progress, so it may wait up to a read's timeout for a `min_seq` wait to end.
