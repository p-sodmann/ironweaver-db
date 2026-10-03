# ADR 0026: Deadlines over gRPC

Status: accepted
Date: 2026-10-03

## Context

gRPC clients send their deadline in the `grpc-timeout` header. The trait has `QueryOptions::timeout`, counted from the call, capped by the database's `LimitConfig::max_timeout` (ADR 0016, ADR 0021). A request message can also say how long it may take, which matters for clients that can't set headers (REST in step 12) and for the conformance suite, which passes a timeout of zero.

Commits run without a timeout (ADR 0016): once a commit's record may be in the WAL it can't be abandoned, and dropping its future doesn't undo it (ADR 0020).

## Decision

**Reads.** `QueryOptions` has `optional uint32 timeout_ms`. The adapter takes the smaller of `timeout_ms` and `grpc-timeout` (either may be missing) as `QueryOptions::timeout`; with neither, the database's default applies. The database caps it at its maximum (5 minutes by default), as it caps limits: a request can only lower them. A read that runs out of time fails with `timeout`, which is `DEADLINE_EXCEEDED`. `wait_for_seq` is a read.

The Rust client sends `timeout_ms` only (rounded up to whole milliseconds, so a positive timeout never becomes 0) and no `grpc-timeout`: the server's deadline is the one the trait defines, and a client-side deadline would race it and turn `timeout` into a transport error. A `grpc-timeout` the server can't parse is ignored.

The deadline starts when the handler runs; network time before it isn't counted. The server doesn't use tonic's `transport::Server`: that server wraps every service in a layer that enforces `grpc-timeout` itself and, when its timer wins the race against the database's, answers `CANCELLED` ("Timeout expired") without our code. The server serves the tonic service on hyper connections of its own instead (`iwdb_server::serve`, ADR 0027), so a deadline is always the trait's `timeout`: `DEADLINE_EXCEEDED` with `iwdb-code: timeout`.

**Commits** (`Commit`, `CommitCatalog`, `CreateNamespace`, `DropNamespace`) ignore `grpc-timeout`. If the client's deadline passes or the client goes away, the handler is dropped, but a commit that was accepted runs to the end on the database's workers, even if it was still queued (the pool runs every queued job; ADR 0020). The client sees `DEADLINE_EXCEEDED` or `CANCELLED` from its own transport and doesn't know the outcome: the commit may have been applied. Retrying with the same idempotency key returns the original result if it was, and applies it once if it wasn't (ADR 0015). `documentation/api/grpc.md` says this, and recommends a key for every commit sent with a deadline.

## Consequences

- A slow read can't hold a worker longer than the server's maximum timeout, whatever the client asks. A read still waiting for a worker ends at its deadline too (found while testing this step; ADR 0020's update).
- A client deadline never leaves a half-applied commit; it can leave an unknown outcome, which idempotency keys resolve.
- Bounding the time a commit waits for the writer (ADR 0016 left it open) stays open: no client deadline cuts it.
