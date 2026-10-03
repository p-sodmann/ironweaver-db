# Step 12: REST/JSON API

Status: todo
Milestone: M3 Network access
Depends on: step 11

## Goal

An HTTP/JSON API for browsers, scripts and curl, with the same semantics as gRPC.

## Tasks

- [ ] First: run the [upstream check](upstream-check.md). Issues this step depended on: #46 (`Expr` constants in JSON filters with NaN or infinities), fixed at `cd09ea0`. Open: [#57](https://github.com/p-sodmann/Ironweaver/issues/57) (`Value` / `Expr` serde can't be read from JSON beyond 64 levels; found in step 11): until it is fixed, the JSON reader keeps `serde_json`'s limit and the docs state a nesting limit of 64 for values and filters in JSON.
- [ ] axum router in `iwdb-server` over the same `Database` trait (no second implementation of any operation).
- [ ] Resource-style routes, e.g. `POST /v1/{ns}/commit`, `GET /v1/{ns}/nodes/{id}`, `POST /v1/{ns}/neighbourhood`, `POST /v1/{ns}/match`, `POST /v1/{ns}/jobs/pagerank`, `GET /v1/namespaces`.
- [ ] JSON bodies use the proto message shapes (`pbjson`), so both APIs share one schema; generate an OpenAPI document and publish it with the docs.
- [ ] Values, filters and patterns in JSON (ADR 0023): `Value` and `Expr` in the core's serde JSON form (`{"Int": 30}`, `{"Label": "Person"}`, available since `a14149e`), a pattern as its text, through a hand-written serde for these three messages (`pbjson-build`'s `extern_path`); the protos carry them as postcard.
- [ ] Streaming results as NDJSON; change stream as Server-Sent Events (step 13).
- [ ] Error bodies with the same codes as gRPC; HTTP status mapping documented.
- [ ] Run the conformance suite over REST.

## Acceptance criteria

- Conformance suite green over REST.
- The OpenAPI document validates and matches the implemented routes (test).
