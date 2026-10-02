# Step 12: REST/JSON API

Status: todo
Milestone: M3 Network access
Depends on: step 11

## Goal

An HTTP/JSON API for browsers, scripts and curl, with the same semantics as gRPC.

## Tasks

- [ ] First: run the [upstream check](upstream-check.md). Issues this step depended on: #46 (`Expr` constants in JSON filters with NaN or infinities), fixed at `cd09ea0`.
- [ ] axum router in `iwdb-server` over the same `Database` trait (no second implementation of any operation).
- [ ] Resource-style routes, e.g. `POST /v1/{ns}/commit`, `GET /v1/{ns}/nodes/{id}`, `POST /v1/{ns}/neighbourhood`, `POST /v1/{ns}/match`, `POST /v1/{ns}/jobs/pagerank`, `GET /v1/namespaces`.
- [ ] JSON bodies use the proto message shapes (`pbjson`), so both APIs share one schema; generate an OpenAPI document and publish it with the docs.
- [ ] Filters in JSON: the core's serde for `Expr` (available since `a14149e`).
- [ ] Streaming results as NDJSON; change stream as Server-Sent Events (step 13).
- [ ] Error bodies with the same codes as gRPC; HTTP status mapping documented.
- [ ] Run the conformance suite over REST.

## Acceptance criteria

- Conformance suite green over REST.
- The OpenAPI document validates and matches the implemented routes (test).
