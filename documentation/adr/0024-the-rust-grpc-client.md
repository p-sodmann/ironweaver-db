# ADR 0024: The Rust gRPC client lives in `iwdb-server`

Status: accepted
Date: 2026-10-03

## Context

Step 11's acceptance runs the conformance suite "through a gRPC client", so something has to implement the `Database` trait over gRPC. The suite's fixture is any value that dereferences to a `Database`, and it drives the futures with `iwdb_query::exec::block_on`, not with tokio. Tonic's channel needs a tokio runtime.

AGENTS.md says not to create crates before a step needs them, and its target layout has no client crate. Step 14 is about clients: the Python remote client, the query shell over the network, benchmarks.

Options:

1. **A module of `iwdb-server` behind a feature** (`client`).
2. **A new crate** (`iwdb-client`). It would need the generated protos and the conversions both sides share, so they would move to a third crate (`iwdb-proto`) or be duplicated.

## Decision

Option 1. `iwdb_server::client::Remote` implements `Database` over a tonic channel and is compiled with the `client` feature (off by default; the server's tests turn it on). It shares `proto` and `convert` with the server, so each message has one conversion each way.

`Remote` owns a small tokio runtime of its own (or uses a handle it is given) and spawns each call on it. The future it returns waits for that task, so it can be driven by any executor (`block_on`, Python, tokio). Dropping the future aborts the task, which drops the tonic call; the server sees the stream reset and drops its handler, cancelling the read (ADR 0025).

Step 11 needs only the conformance fixture, but `Remote` is written as a usable client (connect, timeouts, errors with the server's codes), because step 14's benchmarks and shell will use it.

## Consequences

- No new crate in step 11. The cost is that a Rust application that wants only the client depends on `iwdb-server` (and so on the embedded store crates, which `iwdb-query` already pulls in for its shared types).
- If step 14 or a release wants a slim published client, `proto` and `convert` move to their own crate and `client` becomes `iwdb-client`. Nothing in the protos depends on where the client lives.
- tokio stays out of every library crate except `iwdb-server` (ADR 0020).
