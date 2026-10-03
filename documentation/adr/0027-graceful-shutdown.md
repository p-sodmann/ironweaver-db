# ADR 0027: Graceful shutdown of the server

Status: accepted
Date: 2026-10-03

## Context

The server process owns the store. On SIGTERM (a deploy, a restart) it should lose nothing it acknowledged and leave a store that opens without replaying a long WAL. Requests may be running when the signal comes: reads that take seconds, commits waiting for an fsync, requests queued in the worker pool.

`Store::close` already fsyncs every WAL and writes a checkpoint of each namespace if `checkpoint.on_close` is set; `Embedded::close` first lets the workers finish the queued jobs. Tonic's `transport::Server::serve_with_incoming_shutdown` stops accepting connections when its signal fires, sends HTTP/2 GOAWAY, and waits for the open calls to finish; but it spawns a task per connection, so a server that stops waiting can't stop the calls still running, and it adds a `grpc-timeout` layer the server can't use (ADR 0026).

## Decision

`Server::serve` runs the accept loop itself (hyper's HTTP/2 connections, each a task of a `JoinSet`, watched by hyper-util's graceful shutdown); it returns once every connection is gone, and the caller then closes the database (the binary: `Server::into_database`, then `Embedded::close`). Shutdown, in order:

1. **Stop accepting.** The listener closes and every connection gets GOAWAY: new calls fail with `UNAVAILABLE` and clients may retry them elsewhere or later.
2. **Drain.** Running calls continue, up to `drain_timeout` (config, default 30 s).
3. **Cancel what is left.** At the timeout the connections still open are aborted, which drops their calls: their clients see the connection close (`UNAVAILABLE`), and dropping the handlers cancels their reads (ADR 0025). A commit that was accepted is not cancelled: it runs to the end on its worker (ADR 0026); its client doesn't learn the outcome and retries with its idempotency key.
4. **Finish the workers.** `Embedded::close` stops the pool after its queued jobs ran. A read that was queued sees its cancelled token and fails without reading; queued commits are applied.
5. **Flush the WAL** of every namespace (`Store::close`), whatever the fsync policy: under `group` and `off` this is what makes the last acknowledged commits durable.
6. **Checkpoint** each namespace if `checkpoint_on_shutdown` (config, default true), so the next start replays nothing.
7. **Close** the store and release the data directory's lock. The process exits 0, or 1 if the flush or the checkpoint failed (nothing acknowledged is lost by a failed checkpoint; a failed flush is reported with the policy's guarantee).

A second SIGINT/SIGTERM during the drain skips to step 3.

## Consequences

- Every commit acknowledged before shutdown is durable after it, under every fsync policy. `iwdb-server/tests/shutdown.rs` checks it: clients commit under `off` while shutdown starts, the WAL is cut to its last fsync after shutdown (an OS crash), and every acknowledged commit is there after reopening.
- A request still running at the drain timeout gets no answer. For a read that only means a retry; for a commit it means an unknown outcome, as with any lost connection.
- Shutdown takes at most the drain timeout plus the time the workers need for their queue and the checkpoints.
