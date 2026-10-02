# Step 11: gRPC server

Status: todo
Milestone: M3 Network access
Depends on: step 10

## Goal

A server process that serves the `Database` trait over gRPC, with the proto files as the versioned contract.

## Tasks

- [ ] First: run the [upstream check](upstream-check.md). Issues this step depended on: #27, #28, #29, all fixed at `3b15149`. For #29, report a filter's decode error with the core's message (`format::take_error()` after a postcard failure). For #28, reconsider whether the server should still abort on a panic in the commit path ([ADR 0008](../adr/0008-panics-in-the-commit-path-abort.md)), now that the core's apply path returns `GraphError::Internal` instead of panicking.
- [ ] `proto/ironweaver_db/v1/*.proto`: values, entities, mutations, commit, reads, match, analytics jobs, catalog, errors (status codes + details). `buf lint` and `buf breaking` in CI.
- [ ] Create `crates/iwdb-server` (tonic): config file, data directory, adapters from protos to the trait.
- [ ] Server-streaming for large results (subgraph, match, traversal).
- [ ] Map error codes to gRPC status codes consistently (conflict → `ABORTED`, budget → `RESOURCE_EXHAUSTED`, timeout → `DEADLINE_EXCEEDED`); honour client deadlines.
- [ ] Graceful shutdown: stop accepting, drain, flush WAL, optional checkpoint.
- [ ] Run the conformance suite against the server through a gRPC client.

## Acceptance criteria

- Conformance suite green over gRPC with concurrent clients.
- `buf breaking` guards the v1 contract.
