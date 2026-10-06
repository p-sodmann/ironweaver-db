# ADR 0052: Request ids and cancel

Status: accepted
Date: 2026-10-05

## Context

Step 16c asks for a registry in which every running trait call has an id, its operation, namespace and start time, and a cancel token, so that operators can list and cancel requests. Steps 16e (`iwctl cancel`) and 16f (managed jobs) build on it.

A read can already be stopped: the worker pool cancels a job's `ironweaver_core::cancel::Token` when the caller drops its future (ADR 0020), at its deadline, and when a client disconnects. Points to decide: what an id is, where calls are registered, how the registry reaches a call's token, what a cancelled caller gets, what can't be cancelled, and what happens when a cancel races the end of a call.

## Decision

**Ids** are a per-process counter starting at 1 (`u64`; a decimal string in JSON). They are unique while the server runs, not across restarts. They aren't secret and needn't be unguessable: only the request's owner and server admins may see or cancel it.

**Calls are registered at the authorisation point.** `Authorized` knows the operation, the namespace, the user and the client, and every remote call passes it, so it registers each call it allows (`iwdb_query::requests::Requests::begin`) and unregisters it when the call ends. The registry belongs to the database (`Admin::registry`; `Embedded` owns one). A refusal is only counted. A call made on the embedded store in-process isn't registered: there is no one remote to cancel it for.

**The registry holds a cancel handle per call, not the core's token.** The handle is a flag and the waker of the call's future. `Call::run` wraps the inner future; when the handle fires, the wrapper answers `cancelled` and drops the inner future. Dropping it is what a client going away does already: the pool's `Pending` cancels the core token, so a running algorithm stops at its next check, and a long poll waiting without a worker just ends. This covers every read without threading tokens through the trait, and it reuses the tested path rather than adding one.

**What a cancelled caller gets:** the error `cancelled`, "the request was cancelled (CancelRequest)". That is gRPC `CANCELLED` with `iwdb-code: cancelled`, and REST 499 with an `Error` body (errors.md's existing mapping). The message doesn't name who cancelled. The cancel itself answers with the request as it was.

**What can't be cancelled:** commits, catalog changes, namespace, user, grant and token changes, logins, logouts and cancels (`Operation::cancellable`). Dropping such a call doesn't undo it; its outcome would only become unknown. They are listed with `cancellable: false`, and cancelling one fails with `invalid_argument`. An index build runs inside its catalog commit and is a managed job's business (step 16f).

**Who may cancel:** any authenticated caller its own requests, a server admin anyone's. Another user's request is `not_found`, so a non-admin learns nothing about requests that aren't theirs. Every cancel is audited (`Always`, ADR 0049), with the request's id (the entry's new `request` field) and, once found, its owner as `subject` and its namespace.

**A cancel that races the end of the call.** The registry's mutex orders them:

- If the call has ended, its entry is gone and the cancel answers `not_found`.
- Otherwise the handle fires and the cancel answers. The call then answers `cancelled`, unless its result was ready at the moment its future was next polled: the wrapper polls the inner future first, so a finished answer wins over a late cancel.

A cancel that succeeds therefore means the call was running when the cancel took effect. It doesn't promise the call's work was undone, which for reads is nothing to undo.

**Counted when it ends.** Each call's end (outcome code and duration) feeds `iwdb_requests_total` and `iwdb_request_duration_seconds` (ADR 0050). A call whose future is dropped before it ends (the client went away) counts as `cancelled`.

## Consequences

- A request's "visited" count isn't reported while it runs. The core's `Budget` counts visits inside an algorithm and reports them only when it ends. A live counter needs the core; it is drafted as an upstream proposal, not filed.
- `ListRequests` lists itself, which also proves the registry works on every path (the conformance suite cuts it with `limit: 0`).
- The cost per call is one counter increment, two mutex-protected map operations and two `Arc` allocations, which is small against a network round trip.
- Step 16f's jobs can take ids from the same counter, so that `iwctl cancel` has one id space.
