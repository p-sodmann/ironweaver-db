# Step 16g: OpenTelemetry traces

Status: todo
Milestone: M4 Production 1.0
Depends on: step 16b (`tracing`); can run in parallel with steps 16c to 16f

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

A request can be followed through the server in a tracing backend.

## Tasks

- [ ] Spans for each request (operation, namespace, outcome), its queue wait, execution, commit and fsync, with W3C trace context taken from gRPC metadata and HTTP headers.
- [ ] OTLP export behind a server feature `otel` (ADR 0034: a deployment can do without it), configured in `[tracing]` and the environment; sampling configurable; off by default.
- [ ] Tests with an in-memory exporter: span tree and attributes; no ids or attribute values in span names.
- [ ] ADR.

## Acceptance criteria

- With `otel` on, one request yields one trace with the documented spans; without it, nothing is linked in.

## Non-goals

- Metrics over OTLP (Prometheus is step 16c's).
