# ADR 0050: Metrics without a library

Status: accepted
Date: 2026-10-05

## Context

Step 16c asks for Prometheus metrics: commit latency, fsync time, WAL size, checkpoint duration and lag, memory per namespace, query latency per operation, rejected and timed-out requests, and lock hold times. Every metric must be documented, and a test must keep the documentation and the code equal.

Design rule 1 keeps the engine, storage and query crates pure Rust with few dependencies. SECURITY.md promises that the server opens no connection it wasn't configured for. Points to decide: a metrics crate or none, the buckets, the names, how labels stay bounded, and who may read the metrics.

## Decision

**No metrics crate.** Exporter crates (`prometheus`, `metrics` with an exporter) bring a registry, global state and dependencies into the crates that record. What we need is small:

- `iwdb_engine::metrics::Histogram`: fixed buckets of `AtomicU64`, a sum, relaxed adds, and a snapshot that merges with others.
- Counters are plain atomics where they are counted.
- `iwdb_query::metrics` holds the one list of metrics (`METRICS`: name, kind, labels, unit, help), a snapshot type (`Metrics`: families of samples), and the Prometheus text format (version 0.0.4, about 80 lines).

Each namespace owns its histograms (`LoggedNamespace`, the WAL writer, the store's namespace state), as it already owned its lock statistics. The registry owns those of the requests (ADR 0052). A scrape merges them, so no global state exists and parallel tests don't share counters.

**Buckets.** One set for every duration: 0.5 ms to 30 s (0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, +Inf). They span an fsync on fast storage up to the default read timeout. Because every histogram has them, snapshots of several namespaces add up, and a client can check them on the wire.

**Names.** Prometheus conventions: the prefix `iwdb_`, base units (`_seconds`, `_bytes`), `_total` for counters, and `commits` as the unit of seq distances. A unit test checks these.

**Bounded labels.** `operation` (one of `Operation::ALL`), `code` (`ok` or one of `Code::ALL`), `lock` (`read`, `write`), `version`, and `namespace` on gauges only:

- The gauges are computed at scrape time from the live namespaces, so a dropped namespace's series disappear.
- Only a server admin creates namespaces.
- Histograms have no namespace label: commit latency is server-wide, so the number of series doesn't grow with namespaces times buckets.
- Never an id, a user, a client or a value of the data.

**Reading them takes no namespace lock.** Node and edge counts and memory use are published as atomics with each apply. The checkpoint seq is an atomic readable while a checkpoint runs. The unsynced count is the applied minus the streamable seq. The namespace list comes from the open set, not the namespace log's mutex. So a scrape answers while a commit waits for an fsync, while a backup holds the namespace log, and while every worker is busy. Those are exactly the times an operator looks.

**Exposure.** `GetMetrics` in the `Admin` trait (ADR 0051), served three ways:

- gRPC `AdminService.GetMetrics`
- REST `GET /v1/metrics` (JSON)
- `GET /metrics` in Prometheus' text format. The gate serves this route in every build, also without the REST API, like health.

Metrics are only pulled, never pushed.

**Who may read them.** Any authenticated caller, through the authorisation point. Per-namespace series are narrowed to the namespaces the caller has a role on, the rule `ListNamespaces` follows. `/metrics` needs a token like any route; Prometheus sends an API token (`authorization.credentials_file`) over TLS. There is no unauthenticated mode: a monitoring user without grants sees the server-wide series, and granting it `read` adds a namespace's. That trades a little convenience for not leaking namespace names, and avoids a new role (ADR 0043).

**Documented once.** `documentation/api/metrics.md` has the table (name, type, labels, unit, meaning). Two tests keep it equal to `METRICS` in both directions: a unit test compares the table, and a binary test scrapes `/metrics` and finds exactly those metrics, every sample belonging to one of them.

## Consequences

- No new dependency. The text format is ours to keep right; a unit test pins its escaping, bucket lines, sums and counts.
- Percentiles come from bucket counts (`histogram_quantile`), with the precision of the buckets.
- The request metrics count calls that pass the authorisation point and the gate. A caller of the embedded store in-process (Python's `Store.open`) isn't counted. Its commits still are, in the namespace histograms.
- Memory per namespace is the core's estimate (`Graph::memory_usage`, payloads not counted) as of the last commit. Step 16d's memory limit builds on it.
