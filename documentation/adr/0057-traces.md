# ADR 0057: Traces

Status: accepted
Date: 2026-10-07
Amends: [ADR 0042](0042-structured-logs.md) (the library crates gain `tracing`)

## Context

Step 16g asks that a request can be followed through the server in a tracing backend: spans for each request with its queue wait, its execution, and for writes the commit and the fsync; W3C trace context from gRPC metadata and HTTP headers; OTLP export behind a server feature, off by default; sampling.

The server logs with `tracing` (ADR 0042), but only `iwdb-server` depends on it: the library crates log through `log`. Every remote call passes two places once: the authorisation point `iwdb_query::Authorized` (ADR 0045) with the request registry (ADR 0052), and, for the work, the worker pool `iwdb_query::exec` (ADR 0014, 0020). Commits run through `LoggedNamespace` in `iwdb-storage`.

Points to decide: where spans are opened, whether the library crates take `tracing`, names and attributes, trace context, the exporter and its crates, configuration, the feature and the image, and whether log lines carry trace ids.

## Decision

### Spans are opened where the work already passes, once (design rule 8)

| Span | Opened by | What it covers |
|---|---|---|
| the request, named by its operation (`Find`, `Commit`, `StartJob`, ...) | `Authorized` (and `auth::login` for `Login`) | the call: its check, registration, the inner call and the answer |
| `iwdb.queue` | the worker pool (`Pool::submit_until`), a job's queue | waiting for a worker or a job thread |
| `iwdb.execute` | the worker pool, `exec::spawn` (admin writes) | the work on its thread |
| `iwdb.commit` | `LoggedNamespace` (storage) | the commit pipeline: waiting for the writer, then its phases |
| `iwdb.prepare`, `iwdb.wal.append`, `iwdb.wal.fsync`, `iwdb.apply` | `LoggedNamespace`, the WAL writer | resolve and validate (read lock); the WAL write; its fsync, when this commit pays one; the apply (write lock) |
| `iwdb.changes.wait` | `Embedded::changes` | a change-stream long poll waiting for commits, without a worker |
| `iwdb.collect`, `iwdb.algorithm` | `Ns::analyze` | an analytics projection collected under the read lock; the algorithm and ranking, without a lock |
| `iwdb.checkpoint` with `iwdb.checkpoint.replay`, `.write`, `.prune` | the checkpointer (storage) | one namespace's checkpoint run |
| `iwdb.backup`, `iwdb.verify` with `iwdb.verify.namespace` | the store | an online backup; a verify, per namespace |
| `iwdb.job` | the job registry | a managed job, from queued to ended; linked to the `StartJob` request that queued it |
| `iwdb.import` | the store | a bulk import (in-process only; the server has no import RPC) |

The tree of a request is small and fixed by its kind: a read is `Find` → `iwdb.queue`, `iwdb.execute`; a commit adds `iwdb.commit` → `iwdb.prepare`, `iwdb.wal.append` (→ `iwdb.wal.fsync`), `iwdb.apply`. There is no span per node, edge, row, WAL record or algorithm iteration (tracing inside the core's algorithms is a non-goal).

**The request span** is made in `Authorized::check`, before the role check, so a refusal is a span too (with its outcome). The inner call's future is built inside it, because `Embedded` submits to the pool when a method is called, not when its future is first polled. Requests the gate refuses before an operation runs (no credentials, an unknown token) have no span: they never reach the authorisation point, and their audit entry and `iwdb_requests_total` count them.

**The pool's spans.** `submit_until` opens `iwdb.queue` (a child of the current span) and, on the worker, `iwdb.execute` with the submitting span as its explicit parent. When no subscriber wants trace spans the pool doesn't even look up the current span.

**A managed job** is a trace of its own (`iwdb.job`, a root), linked to the `StartJob` request span ("follows from"): a job may run for an hour after its request answered, and a backend shows long-lived children of a finished request badly. It has `iwdb.queue`, then `iwdb.collect` and `iwdb.algorithm`.

**Work that no request started** is a trace of its own too: the background checkpointer's runs (`iwdb.checkpoint`) and a projection's commits (`iwdb.commit`). The group-commit timer's fsync (`sync_due`, up to every `group_max_delay_ms`) isn't traced: it would be a trace per interval. Recovery isn't traced: it runs before the server is ready.

**Group commit.** Under `group` no commit waits for an fsync it shares (ADR 0005): a commit either returns after its write, or is the one that closes the batch and pays the fsync for all of it. So there is no waiter to show. The commit that pays has `iwdb.wal.fsync` with `iwdb.wal.batch`, the records the fsync made durable; the others' `iwdb.wal.append` has `iwdb.wal.synced = false`. Under `always` every commit has its fsync; under `off` none.

### The library crates take `tracing`

`iwdb-storage`, `iwdb-query` and `iwdb` depend on `tracing` (`default-features = false`, `std`: no proc macros). It is pure Rust, already in the tree through tokio and hyper, and the facade costs nothing when nobody subscribes. The alternatives were worse: a hook trait of our own threaded through storage would duplicate `tracing`'s span stack, and spans only in the server could not see the queue, the WAL or the fsync. The crates still log with `log` (ADR 0042 is unchanged there); only spans use `tracing`.

The OpenTelemetry crates stay in `iwdb-server`, behind the feature. Nothing pyo3-related moves. `iwdb-engine` gets nothing: it has no phase worth a span.

All trace spans have the target `iwdb::trace` and level `INFO`. The log filter turns that target off (unless `[log] level` names it), so spans never reach log lines, and with no OpenTelemetry layer installed no subscriber is interested: a span's callsite then costs one atomic load. The same holds without the feature, and in the embedded library and Python (traces there are a non-goal).

### Names and attributes

**Names are fixed strings**: the `iwdb.*` names above, and for the request span the operation's name from the operation table (`Operation::name`, a closed set, through `otel.name`). Never an id, a namespace name or a value. A test collects every exported name.

**Attributes** (absent ones left out):

| Attribute | On | Value |
|---|---|---|
| `db.system.name` | the request | `ironweaver_db` |
| `db.operation.name` | the request | the operation |
| `db.namespace` | the request, `iwdb.checkpoint`, `iwdb.verify.namespace`, `iwdb.job` | the namespace's name |
| `iwdb.outcome` | the request, `iwdb.job` | `ok` or the error code (`timeout`, `permission_denied`, ...) |
| `otel.status_code` | the request, `iwdb.job` | `error` when the outcome isn't `ok`; the status has no description (an error message can quote data) |
| `iwdb.request_id` | the request, `iwdb.job` | the registry's id (ADR 0052), the job's id |
| `iwdb.max_results`, `iwdb.max_visited`, `iwdb.max_edges`, `iwdb.timeout_ms` | the request | the read's bounds after the server's defaults and caps |
| `iwdb.seq`, `iwdb.visited`, `iwdb.edges`, `iwdb.truncated` | the request; `iwdb.seq` also on `iwdb.commit` | what the answer reports: the seq it saw or committed, its work counts, whether a limit cut it |
| `iwdb.ops` | `iwdb.commit` | the record's resolved ops (a mutation can resolve to several) |
| `iwdb.wal.bytes`, `iwdb.wal.synced` | `iwdb.wal.append` | the frame's size; whether it is durable when the append returns |
| `iwdb.wal.fsync_policy`, `iwdb.wal.batch` | `iwdb.wal.fsync` | `always`, `group` or `off`; the records it made durable |
| `iwdb.job.kind`, `iwdb.job.nodes`, `iwdb.job.edges` | `iwdb.job` | the algorithm; the projection's size |

**The user isn't in a span.** Traces leave the process for a collector and a backend that more people can usually read than the audit log or `ListRequests`, and a user name is personal data. The audit log (ADR 0049) remains the record of who did what; a trace's `iwdb.request_id` and time lead to it. Neither is the client's address in a span.

**Never** in a span: an attribute value, a filter, a pattern, a node or edge id, a mutation, an error message, a token or a password. The spans record counts and codes only.

### W3C trace context

The gate reads `traceparent` and `tracestate` (gRPC metadata is HTTP/2 headers, so one reader serves both APIs) with OpenTelemetry's W3C `TraceContextPropagator`, and runs the request with that context as the current OpenTelemetry context. The request span, made in `Authorized`, takes it as its parent; `iwdb-query` sees no OpenTelemetry type.

- **A malformed or missing `traceparent`** is ignored: the request starts a new root. Never an error.
- **The parent's sampled flag** decides (parent-based sampling): a request whose caller didn't sample isn't sampled here either.
- **Responses echo nothing**: no `traceparent`, no `traceresponse` (a draft), no trace id in errors. A caller who wants the link sends a `traceparent`.
- Without the feature, or with tracing off, the headers aren't read.

### Export: OTLP over gRPC or HTTP, with an exporter of our own

The pieces from the OpenTelemetry project: `opentelemetry` (the API, `trace`), `opentelemetry_sdk` (spans, the tracer provider, the sampler and the W3C propagator), `tracing-opentelemetry` (turns `tracing` spans into OpenTelemetry spans) and `opentelemetry-proto` (the OTLP messages, the conversion from the SDK's spans, and the generated gRPC client, on tonic 0.14, our version). All Apache-2.0, all without their default features.

**The exporter and the batching are ours** (`iwdb_server::otel`, about 300 lines), instead of `opentelemetry-otlp` and the SDK's `BatchSpanProcessor`:

- `opentelemetry-otlp` reads `OTEL_EXPORTER_OTLP_*` variables on its own (headers from the environment are merged with ours; compression, timeouts and TLS switches too). ADR 0039 says every setting has one source that `--check-config` shows; a library that reads the environment behind it breaks that.
- The SDK's batch processor exports from a thread without a tokio runtime, which tonic's channel and hyper's client need, and it keeps its count of dropped spans to itself, so it couldn't feed a metric.
- `opentelemetry-otlp`'s HTTP transport wants reqwest or a client of its own; we have hyper's client and our rustls connector already (feature `client`).

**Protocols**: `grpc` (OTLP/gRPC, `TraceService/Export`, port 4317 by convention) and `http/protobuf` (`POST /v1/traces` with a protobuf body, port 4318). Both over plain HTTP or TLS (`https://`, verified against the system's roots, as the clients do). Not `http/json`: collectors accept protobuf everywhere.

**Batching**: ended spans go to a bounded queue (2048 spans). A thread of its own takes batches of up to 512 every 5 seconds, or sooner when 512 are waiting, and exports each on a small tokio runtime of its own, with a timeout of 10 seconds per batch (the OpenTelemetry SDK's defaults). The server's runtime does no export work.

**When the collector is down** spans are dropped, never waited for: a full queue drops the new span, a failed or timed-out export drops its batch. A request never waits for the collector and never fails because of it (ending a span is a queue push under a mutex). The first failure after a success is logged at `warn` with the cause, once until exports succeed again (then at `info`), so a dead collector doesn't flood the log.

**Its health is visible** in the metrics (ADR 0050): `iwdb_trace_spans_exported_total` and `iwdb_trace_spans_dropped_total{reason}` (`queue_full`, `export_failed`). They are 0 when tracing is off.

**Shutdown flushes**: after the store has closed (so the final checkpoint's spans are included), the server exports what is queued and waits at most what is left of the drain's deadline (`[server] drain_timeout_secs` from the shutdown signal), and at least one second. What doesn't make it is dropped and counted.

**Resource**: `service.name` (`[tracing] service_name`) and `service.version`. Nothing else about the host.

### Configuration: `[tracing]`

| Key | Default | |
|---|---|---|
| `enabled` | `false` | export traces |
| `endpoint` | `http://127.0.0.1:4317` (`grpc`), `http://127.0.0.1:4318/v1/traces` (`http/protobuf`) | the collector; for `http/protobuf`, `/v1/traces` is added to an endpoint without a path |
| `protocol` | `grpc` | `grpc` or `http/protobuf` |
| `sample_ratio` | `1.0` | the share of new traces sampled, 0 to 1; requests with a parent follow its decision |
| `service_name` | `iwdb-server` | `service.name` |
| `headers` | unset | `name=value,name=value`: sent with each export (a vendor's API key). `--check-config` prints the names, not the values |

Each has its `IWDB_TRACING_*` variable (ADR 0039), and `IWDB_TRACING_` is a section prefix (a typo is an error).

**The standard `OTEL_*` variables aren't read.** One configuration with one source per value, shown by `--check-config`, is the rule (ADR 0039); honouring a second family of names would need rules for which wins, and an injected `OTEL_EXPORTER_OTLP_ENDPOINT` would silently redirect a server whose file says otherwise. The mapping is one to one (`OTEL_EXPORTER_OTLP_ENDPOINT` → `IWDB_TRACING_ENDPOINT`, `OTEL_SERVICE_NAME` → `IWDB_TRACING_SERVICE_NAME`, `OTEL_TRACES_SAMPLER_ARG` → `IWDB_TRACING_SAMPLE_RATIO`, ...), and config.md has it. A server that finds `OTEL_*` variables set warns at start that it ignores them.

**Sampling** is parent-based, with `TraceIdRatioBased(sample_ratio)` for new roots. At 1.0, with no backend's own sampling, every request is a trace: a busy server sends that many spans, and the queue's drops show it.

### The feature `otel`

A server feature, off by default for `cargo build` (ADR 0034: a deployment can do without it). It implies `client` (hyper's client, tonic's TLS and the rustls connector the exporter uses). Without it the OpenTelemetry crates aren't in the tree (`cargo tree -p iwdb-server -e normal` has none; CI checks this next to the pyo3 check), the spans in the library crates have no subscriber, and a config with `[tracing] enabled = true` is refused before the store opens: "this iwdb-server was built without the otel feature", exit 2, as `console` and `postgres` are. `--version` lists it.

**The Docker image includes it** (`FEATURES="rest postgres console otel"`), off unless `IWDB_TRACING_ENABLED=true`: an image serves many deployments, and pointing one at a collector shouldn't need a rebuild. The image's binary grows by the OpenTelemetry crates; the server opens no connection for them unless tracing is on, so SECURITY.md's promise (no connection that isn't configured) holds.

### Log lines don't carry trace ids

Writing `trace_id` and `span_id` into JSON log lines needs an event formatter of our own instead of `tracing-subscriber`'s, which reads the OpenTelemetry context of the current span for every event. That is neither cheap in code nor at run time, and it isn't required. Log lines keep their request `span.path` (ADR 0042); a trace and a log line meet by time and route. Revisit if operators ask.

## Consequences

- A request can be followed from the caller's span through the server's queue, execution, commit, WAL and fsync in any OTLP backend (Jaeger, Tempo, an OpenTelemetry Collector).
- With tracing off or the feature out, spans cost a disabled callsite each. The criterion benchmarks of commit and find latency before and after are in the step file's outcome.
- `iwdb-storage`, `iwdb-query` and `iwdb` depend on `tracing`; ADR 0042's "no crate below the server gains a dependency" is amended for this one, for spans only.
- New crates with `otel` (all Apache-2.0): `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-proto`, `tracing-opentelemetry` and what they bring (listed in the PR). Bumping them is deliberate; their span model has changed in most minor versions.
- The exporter and its batching are ours to keep right: tests export to an in-memory exporter and to a collector that is down, and check the counters and the shutdown flush.
- The span tree is documented in guarantees.md ("Traces"); a new phase worth a span gets a row there and in this ADR's table.
