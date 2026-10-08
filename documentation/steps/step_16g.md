# Step 16g: OpenTelemetry traces

Status: done
Milestone: M4 Production 1.0
Depends on: step 16b (`tracing`); can run in parallel with steps 16c to 16f

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

A request can be followed through the server in a tracing backend.

## Tasks

- [x] Spans for each request (operation, namespace, outcome), its queue wait, execution, commit and fsync, with W3C trace context taken from gRPC metadata and HTTP headers.
- [x] OTLP export behind a server feature `otel` (ADR 0034: a deployment can do without it), configured in `[tracing]` and the environment; sampling configurable; off by default.
- [x] Tests with an in-memory exporter: span tree and attributes; no ids or attribute values in span names.
- [x] ADR.

## Acceptance criteria

- With `otel` on, one request yields one trace with the documented spans; without it, nothing is linked in.

## Outcome

- [ADR 0057](../adr/0057-traces.md). The spans are opened once, where the work passes: the request at the authorisation point (`Authorized`, named by its operation), the queue wait and execution in the worker pool, the commit pipeline's phases and the fsync in storage, and a managed job as a trace of its own linked to its `StartJob`. `iwdb-storage`, `iwdb-query` and `iwdb` depend on `tracing` for this (spans only, target `iwdb::trace`); the OpenTelemetry crates are only in `iwdb-server`, behind `otel`.
- The exporter and its batching are ours, over `opentelemetry-proto`: `opentelemetry-otlp` reads `OTEL_EXPORTER_OTLP_*` variables behind the config (ADR 0039), and the SDK's batch processor neither exposes its drop count nor exports inside a tokio runtime. Both OTLP/gRPC and `http/protobuf`, plain or TLS.
- `OTEL_*` variables aren't read (the server warns about them); log lines don't carry trace ids (decided in the ADR: not cheap enough).
- Acceptance criterion: `a_grpc_request_is_one_trace_under_its_callers_span` and `a_rest_request_is_one_trace_under_its_callers_span` (`crates/iwdb-server/tests/traces.rs`, the SDK's in-memory exporter) check the tree and attributes; `the_shutdown_sends_the_queued_spans_to_the_collector` (`binary.rs`) checks a real export from the binary. "Nothing is linked in" without the feature: CI's `cargo tree` check, `the_version_lists_the_features`, and the config's refusal.
- Benchmarks with tracing off (no subscriber, as in the embedded library; the same callsites are disabled in a server without `[tracing] enabled`): `cargo bench -p iwdb-server --bench graph`, the commit before this step (`b671692`) and after, run alternately three times each on one laptop (Apple Silicon), medians:

  | Benchmark | Before | After | Change |
  |---|---|---|---|
  | `commit/always` | 4.10 ms | 4.11 ms | +0.3 % |
  | `commit/group` | 119.1 µs | 113.6 µs | −4.6 % |
  | `commit/off` | 4.03 µs | 3.89 µs | −3.5 % |
  | `read/embedded/get_node` | 10.65 µs | 11.03 µs | +3.5 % |
  | `read/embedded/find_indexed_100` | 596 µs | 461 µs | −22.7 % |
  | `read/grpc/get_node` | 73.7 µs | 69.1 µs | −6.3 % |
  | `read/grpc/find_indexed_100` | 806 µs | 798 µs | −1.0 % |

  Every difference is within the spread of the runs of one build (`find_indexed_100` embedded ranged 488 to 631 µs before), so the spans cost nothing measurable. A first after-run against a baseline saved an hour earlier showed −51 % to +130 % swings on this machine, which is why the runs alternate.
- A test that raced on slow runners, unrelated to traces, was made robust: `a_panic_in_the_group_commit_timer_aborts_and_loses_nothing` (`crates/iwdb/tests/panics.rs`) assumed a burst of commits finishes within the group commit's 40 ms, so that only the timer fsyncs. On a macOS runner a commit paid the fsync and took the injected panic. The delay is now 1 s and the bursts shorter.

## Non-goals

- Metrics over OTLP (Prometheus is step 16c's).
