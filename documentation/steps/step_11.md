# Step 11: gRPC server

Status: in progress
Milestone: M3 Network access
Depends on: step 10

## Goal

A server process that serves the `Database` trait over gRPC, with the proto files as the versioned contract. The server only translates between protos and the trait (design rule 8): every operation, limit and error code is the trait's.

## Decisions

Recorded before the code, refined while writing it:

- [ADR 0023](../adr/0023-wire-encoding-of-values-filters-and-patterns.md): structure is plain proto messages; `Value` and `Expr` carry the core's serde form encoded with postcard; attributes are `map<string, Value>`; a `Pattern` is its text, or postcard for patterns the text can't express.
- [ADR 0024](../adr/0024-the-rust-grpc-client.md): the Rust client (`iwdb_server::client::Remote`, feature `client`) lives in `iwdb-server`; no new crate.
- [ADR 0025](../adr/0025-server-streaming.md): a streaming RPC streams the answer of **one** trait call in chunks; the server never follows a cursor.
- [ADR 0026](../adr/0026-deadlines-over-grpc.md): `grpc-timeout` and the request's `timeout_ms` both bound a read (the smaller wins, the server's cap applies); a commit has no deadline on the server, and an idempotency key makes a retry after a client deadline safe.
- [ADR 0027](../adr/0027-graceful-shutdown.md): stop accepting, drain up to a timeout, cancel what is left, finish accepted commits, flush the WAL, optional checkpoint, close.
- [ADR 0028](../adr/0028-internal-apply-errors-abort.md) (upstream #28): the server keeps ADR 0008's abort; a `GraphError::Internal` from the apply path now aborts too.

## Tasks

- [ ] First: run the [upstream check](upstream-check.md). Issues this step depended on: #27, #28, #29, all fixed at `3b15149`. *(Checked 2026-10-03: #48–#50 fixed in `ace9a0d`, bumped and adopted, so paths, matching and walks served remotely use the core's budgets. Checked again at the start of the step: core `main` is still `d15a7ec`, no issue is open.)*
  - [ ] #29: a value, filter or pattern that fails to decode is `invalid_argument` with the core's message (`format::take_error()` after a postcard failure; ADR 0023).
  - [ ] #28: reconsider ADR 0008 for the server (ADR 0028).
- [ ] `proto/ironweaver_db/v1/*.proto`: values, entities, mutations, commit, every read of the trait, match, analytics jobs, catalog, namespaces, errors. `buf.yaml` with the `STANDARD` lint rules and the `FILE` breaking rules (the strictest: they protect generated code and the JSON names REST uses in step 12).
- [ ] CI: `buf lint` on every push, `buf breaking` against the base branch on pull requests (official `bufbuild/buf-action`).
- [ ] `crates/iwdb-server` (tonic, prost; protos compiled with `protox`, so building needs no `protoc`):
  - [ ] `Server<D: Database>`: one adapter per RPC, translating protos to the trait and back (`convert`), nothing else.
  - [ ] Error mapping in one place (`status`): the gRPC status from [errors.md](../api/errors.md), the code string in the `iwdb-code` trailer; a test for every `Code`. Mark the gRPC column of errors.md as implemented.
  - [ ] Server-streaming for the reads whose answer is a list (`get_nodes`, `get_edges`, `find`, `neighbourhood`, `traverse`, `random_walks`, `subgraph`, `match_pattern`, `analyze`): chunks of at most 1 MiB, the answer's seq, cursor, `truncated` and work in the last message (ADR 0025).
  - [ ] Deadlines: `grpc-timeout` and `timeout_ms` mapped onto `QueryOptions::timeout` (ADR 0026); limits pass through, so the database's `LimitConfig` gives the defaults and the caps and a request can only lower them.
  - [ ] A client that goes away cancels its read (the handler's future, and with it the trait's future, is dropped). Test.
  - [ ] Graceful shutdown (ADR 0027), with a test that every commit acknowledged before shutdown survives a simulated OS crash after it (design rule 3).
  - [ ] The `iwdb-server` binary: `iwdb-server --config <file>`; a minimal TOML config (data directory, listen address, fsync policy, checkpoint on shutdown, drain timeout, workers, queue, message size, limits); SIGINT/SIGTERM shut down gracefully.
  - [ ] `client::Remote` (feature `client`): the `Database` trait over gRPC, usable from any executor (ADR 0024).
- [ ] Run the conformance suite over gRPC: an in-process server on an ephemeral port per test, `Remote` as the fixture; and the whole suite again with all its cases running concurrently, each through its own client, against one server.
- [ ] Wire tests: values, filters and patterns nested 100 levels round-trip, 101 fail with the core's message; a fixture of postcard bytes guards the encoding ADR 0023 fixes.
- [ ] Documentation: `documentation/api/grpc.md` (services, encoding, errors, deadlines, streaming, shutdown, config), design doc and guarantees updated, CHANGELOG.

## Acceptance criteria

- The conformance suite runs unchanged over gRPC, and passes with concurrent clients against one server.
- Every `Code` maps to the gRPC status of errors.md, and back (test).
- Commits acknowledged before a graceful shutdown survive an OS crash after it (test).
- A read whose client goes away stops (test).
- `buf lint` passes and `buf breaking` guards the v1 contract in CI.
- The workspace builds on Rust 1.85 without `protoc`, and `cargo deny` passes.

## Non-goals

- TLS, authentication, per-client limits and quotas: step 15. The server listens on plain TCP; bind it to localhost or a private network until then.
- REST/JSON and the OpenAPI document: step 12 (it reuses these protos through `pbjson`; ADR 0023 says how values and filters look in JSON).
- The change stream: step 13.
- The Python remote client, the query shell over the network, and benchmarks: step 14.
- Health and readiness endpoints, metrics, environment overrides of the config, `iwctl` against a running server, managed analytics jobs: step 16.
- Following a cursor on the server, or snapshot reads across pages (ADR 0021, ADR 0025).
