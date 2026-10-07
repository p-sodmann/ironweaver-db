# Metrics

`iwdb-server` exports its metrics in Prometheus' text format at `GET /metrics` (version 0.0.4), and as `GetMetrics` over gRPC and `GET /v1/metrics` over REST ([rest.md](rest.md#operator-reads)). Decisions: [ADR 0050](../adr/0050-metrics-without-a-library.md).

- **Pulled, never pushed.** The server opens no connection to send them anywhere ([SECURITY.md](../../SECURITY.md)). `/metrics` is served in every build, also without the REST API.
- **Credentials.** `/metrics` needs a token like any route: give Prometheus an API token (`authorization: { credentials_file: ... }` in the scrape config), over TLS (`scheme: https`, `tls_config: { ca_file: ... }`). A caller sees the series of the namespaces it has a role on, and those of no namespace: a monitoring user without grants sees the server-wide ones; grant it `read` on a namespace for that namespace's.
- **Labels are bounded**: `operation` (an RPC's name), `code` (`ok` or an error code of [errors.md](errors.md)), `lock` (`read`, `write`), `namespace` (a live namespace; its series go when it is dropped), `part` (`graph`, `checkpoint`, `working`), `outcome` (`ok`, `failed`), `version`. Never an id, a user, a client or a value of the data.
- **Histograms** have the same buckets, in seconds: 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30 and `+Inf`.
- **Since when.** Counters and histograms count from when the database started serving (`iwdb_start_time_seconds`); gauges are read when scraped. The per-namespace values are read without any lock of the namespace, so a scrape answers while commits wait for an fsync and while every worker is busy.

```yaml
scrape_configs:
  - job_name: iwdb
    scheme: https
    tls_config: { ca_file: /etc/prometheus/iwdb-ca.pem }
    authorization: { credentials_file: /etc/prometheus/iwdb-token }
    static_configs: [{ targets: ["iwdb:7600"] }]
```

## The metrics

A test keeps this table equal to the metrics the server exports (`METRICS` in `crates/iwdb-query/src/metrics.rs`).

| Name | Type | Labels | Unit | Meaning |
|---|---|---|---|---|
| `iwdb_build_info` | gauge | `version` | – | Always 1; the server's version is its label. |
| `iwdb_start_time_seconds` | gauge | – | seconds | When the database started serving, in seconds since 1970 (UTC). |
| `iwdb_ready` | gauge | – | – | 1 while the server serves requests, 0 while it drains. |
| `iwdb_requests_active` | gauge | – | – | Requests running now. |
| `iwdb_requests_total` | counter | `operation`, `code` | – | Requests that ended, by operation and outcome: `ok` or the error code (`timeout`, `unavailable` for rejected ones, `permission_denied`, ...). |
| `iwdb_request_duration_seconds` | histogram | `operation` | seconds | How long requests ran, by operation, queueing included. |
| `iwdb_commit_duration_seconds` | histogram | – | seconds | Commits (data and catalog) in the commit pipeline, from the call to the answer: waiting for the writer, the WAL append and fsync, and the apply. |
| `iwdb_wal_fsync_duration_seconds` | histogram | – | seconds | Fsyncs of the WALs. |
| `iwdb_checkpoint_duration_seconds` | histogram | – | seconds | Checkpoint runs that wrote a checkpoint. |
| `iwdb_lock_hold_seconds` | histogram | `lock` | seconds | How long namespace locks were held: `write` by commits (apply and index flush), `read` by reads. |
| `iwdb_namespace_nodes` | gauge | `namespace` | – | Nodes in the namespace. |
| `iwdb_namespace_edges` | gauge | `namespace` | – | Edges in the namespace. |
| `iwdb_namespace_memory_bytes` | gauge | `namespace` | bytes | Approximate bytes the namespace's graph uses, indexes included (attribute payloads not counted; see `iwdb_memory_used_bytes`). |
| `iwdb_wal_bytes` | gauge | `namespace` | bytes | Bytes of the namespace's WAL segments on disk. |
| `iwdb_checkpoint_bytes` | gauge | `namespace` | bytes | Bytes of the namespace's checkpoints on disk. |
| `iwdb_checkpoint_lag_commits` | gauge | `namespace` | commits | Commits since the namespace's newest checkpoint: what recovery would replay. |
| `iwdb_last_checkpoint_timestamp_seconds` | gauge | `namespace` | seconds | When the namespace's newest checkpoint was written, in seconds since 1970 (UTC); no sample without one. |
| `iwdb_unsynced_commits` | gauge | `namespace` | commits | Commits applied but not known to be durable; no sample under the `off` fsync policy before a sync. |
| `iwdb_namespace_read_only` | gauge | `namespace` | – | 1 if the namespace is read-only after a failure (until the server restarts). |
| `iwdb_checkpoint_failed` | gauge | `namespace` | – | 1 if the namespace's last checkpoint failed. |
| `iwdb_disk_free_bytes` | gauge | – | bytes | Bytes free for the server on the data directory's file system; no sample where it can't be read. |
| `iwdb_memory_used_bytes` | gauge | `part` | bytes | Memory the server counts against its limit, by part: `graph` (the live graphs, their indexes and attributes: the core's figure), `checkpoint` (the checkpointers' copies), `working` (analytics projections and index builds, and the managed jobs' stored results). |
| `iwdb_memory_limit_bytes` | gauge | – | bytes | The memory limit (`[memory] limit_bytes`, or the cgroup's); no sample without one. |
| `iwdb_memory_warn_bytes` | gauge | – | bytes | From here on the server warns (`warn_at` of the limit); no sample without a limit. |
| `iwdb_memory_refuse_writes_bytes` | gauge | – | bytes | From here on the server refuses writes with `resource_exhausted` (`refuse_writes_at` of the limit); no sample without a limit. |
| `iwdb_memory_state` | gauge | – | – | 0 normal, 1 above the warning line, 2 refusing writes. Each state is left 5 % of the limit below its line. |
| `iwdb_backup_running` | gauge | – | – | Backups copying now; checkpoints wait while it is above 0. |
| `iwdb_backup_bytes_total` | counter | – | bytes | Bytes written by backups, counted as they are written (a throttled backup's progress). |
| `iwdb_backups_total` | counter | `outcome` | – | Backups that ended, by outcome: `ok` or `failed`. |
| `iwdb_last_backup_timestamp_seconds` | gauge | – | seconds | When the last successful backup finished, in seconds since 1970 (UTC); no sample without one. |
| `iwdb_jobs_queued` | gauge | – | – | Managed analytics jobs waiting for a job thread. |
| `iwdb_jobs_running` | gauge | – | – | Managed analytics jobs collecting their projection or running. |
| `iwdb_jobs_total` | counter | `outcome` | – | Managed analytics jobs that ended, by outcome: `done`, `failed` or `cancelled`. |
| `iwdb_job_result_bytes` | gauge | – | bytes | The stored job results' estimated size (counted in the `working` memory part). |

## Examples

- Commit latency, 99th percentile over 5 minutes: `histogram_quantile(0.99, rate(iwdb_commit_duration_seconds_bucket[5m]))`.
- Rejected requests (a full queue, or draining): `rate(iwdb_requests_total{code="unavailable"}[5m])`; timed out: `code="timeout"`.
- Memory: `sum(iwdb_memory_used_bytes) / iwdb_memory_limit_bytes` is the fraction of the limit in use ([ADR 0054](../adr/0054-the-memory-limit.md)); alert on `iwdb_memory_state >= 1`, and page on `iwdb_memory_state == 2` (writes refused: `rate(iwdb_requests_total{code="resource_exhausted"}[5m])`).
- Backups ([ADR 0055](../adr/0055-admin-writes-and-iwctl-against-a-server.md)): a backup's copy rate is `rate(iwdb_backup_bytes_total[1m])`; checkpoints are held back while `iwdb_backup_running > 0`; alert on `time() - iwdb_last_backup_timestamp_seconds` above your backup interval, and on `increase(iwdb_backups_total{outcome="failed"}[1d]) > 0`.
- Managed jobs ([ADR 0056](../adr/0056-managed-analytics-jobs.md)): `iwdb_jobs_queued` near `[jobs] queued` means starts are about to be refused (`unavailable`); alert on `increase(iwdb_jobs_total{outcome="failed"}[1h]) > 0` if failed jobs matter; stored results are part of `iwdb_memory_used_bytes{part="working"}`.
- A namespace that went read-only: `iwdb_namespace_read_only == 1`. Checkpoints falling behind: `iwdb_checkpoint_lag_commits`, `time() - iwdb_last_checkpoint_timestamp_seconds`.
