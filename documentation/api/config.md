# Server configuration

`iwdb-server` reads its settings from a TOML file (`--config <file>`), from `IWDB_*` environment variables, or both; a variable wins over the file ([ADR 0039](../adr/0039-configuration-and-environment-overrides.md)). Only `data_dir` is required, and TLS's certificate and key (or `[tls] enabled = false`), so in a container a few variables are enough and no file needs mounting.

```
iwdb-server --config server.toml                      # file, plus any IWDB_* variables
IWDB_DATA_DIR=/var/lib/iwdb IWDB_TLS_CERT=cert.pem IWDB_TLS_KEY=key.pem \
  IWDB_AUTH_BOOTSTRAP_PASSWORD=... iwdb-server        # first start: creates the admin
iwdb-server --check-config --config server.toml       # validate; print the effective settings and their sources
```

## Rules

- **Names.** A setting's variable is `IWDB_` and its path in the file in upper case, with `.` as `_`: `store.fsync` is `IWDB_STORE_FSYNC`, `limits.max.timeout_ms` is `IWDB_LIMITS_MAX_TIMEOUT_MS`.
- **Values.** Numbers as digits (fractions like `0.9`), booleans as `true`/`false` (or `1`/`0`), choices by name (`group`), paths and addresses as they are.
- **Typos are errors.** A variable that starts like a section (`IWDB_STORE_`, `IWDB_SERVER_`, `IWDB_MEMORY_`, `IWDB_LIMITS_`, `IWDB_LOG_`, `IWDB_CONSOLE_`, `IWDB_AUTH_`, `IWDB_TLS_`, `IWDB_AUDIT_`, `IWDB_BACKUP_`, `IWDB_JOBS_`, `IWDB_TRACING_`) but names no setting stops the server (apart from the two bootstrap variables below). Other `IWDB_*` variables (test harnesses use some) are ignored.
- **Every problem at once.** Startup checks the file and the variables before it opens the store, and lists every problem with where it came from (the file, a variable), then exits with code 2. A TOML syntax or type error in the file is reported with its line; the file's other checks wait until it parses.
- **Relative paths** (`data_dir`, `tls.cert`, `tls.key`, `tls.client_ca`, `audit.dir`, `store.archive`, `backup.dir`). From the file: relative to the file's directory. From a variable: relative to the working directory.
- **Projections** (`[[projection]]`, [projections.md](projections.md)) are set in the file only; a projection's Postgres URL can come from a variable of your choice (`url_env`).

## Settings

| Key | Variable | Default | Meaning |
|---|---|---|---|
| `data_dir` | `IWDB_DATA_DIR` | `required` | The data directory; created if missing. |
| `listen` | `IWDB_LISTEN` | `"127.0.0.1:7600"` | The address of gRPC, REST, health and the console (one port). |
| `store.fsync` | `IWDB_STORE_FSYNC` | `"always"` | `always`, `group` or `off` ([guarantees.md](../guarantees.md)). |
| `store.group_max_delay_ms` | `IWDB_STORE_GROUP_MAX_DELAY_MS` | `10` | Group commit: fsync at least this often. |
| `store.group_max_batch` | `IWDB_STORE_GROUP_MAX_BATCH` | `64` | Group commit: or after this many commits (at least 1). |
| `store.checkpoint_on_shutdown` | `IWDB_STORE_CHECKPOINT_ON_SHUTDOWN` | `true` | Checkpoint every namespace when the server stops, so the next start replays nothing. |
| `store.retain_records` | `IWDB_STORE_RETAIN_RECORDS` | `0` | WAL kept for the change stream: the last N commits ([changes.md](changes.md)). |
| `store.retain_age_secs` | `IWDB_STORE_RETAIN_AGE_SECS` | `0` | ... and the commits younger than this. |
| `store.archive` | `IWDB_STORE_ARCHIVE` | `unset` | The WAL archive: every WAL segment is copied there, durably, before a checkpoint removes it, so a backup plus the archive restores to any later seq ([ADR 0009](../adr/0009-backup-archive-restore.md)). Created if missing; it must belong to the store's history (a restored store needs a new one). Pruned with `iwctl archive prune` (see [Backups](#backups)). |
| `server.drain_timeout_secs` | `IWDB_SERVER_DRAIN_TIMEOUT_SECS` | `30` | How long running calls may finish on shutdown. |
| `server.max_message_bytes` | `IWDB_SERVER_MAX_MESSAGE_BYTES` | `67108864` | Largest request or answer message (REST: request body); at least 1024. |
| `server.workers` | `IWDB_SERVER_WORKERS` | `0` | Threads running requests (0: one per CPU). |
| `server.queue` | `IWDB_SERVER_QUEUE` | `1024` | Requests waiting for a worker; more fail with `unavailable`. |
| `server.unready_delay_ms` | `IWDB_SERVER_UNREADY_DELAY_MS` | `0` | On shutdown, serve this long with readiness off before draining, so load balancers stop sending first ([ADR 0040](../adr/0040-health-and-readiness.md)). |
| `server.plaintext_public` | `IWDB_SERVER_PLAINTEXT_PUBLIC` | `false` | With `tls.enabled = false`: allow a non-loopback `listen` address, where passwords, tokens and data cross the network in clear (and with authentication off, anyone who reaches the port can change everything); without this flag such an address is refused. With TLS on it has no effect, and a warning says so ([ADR 0048](../adr/0048-tls-and-mtls.md)). |
| `memory.limit_bytes` | `IWDB_MEMORY_LIMIT_BYTES` | `the cgroup's, or none` | The memory the server may use ([ADR 0054](../adr/0054-the-memory-limit.md)). Unset: the limit of its cgroup on Linux (v2 `memory.max`, v1 `memory.limit_in_bytes`), none elsewhere; `0`: none. It counts the graphs, their payloads (the core's figure), the checkpointers' copies, and analytics projections and index builds while they run. |
| `memory.warn_at` | `IWDB_MEMORY_WARN_AT` | `0.8` | From this fraction of the limit on, the server warns: a log event, `iwdb_memory_state` 1, the status's `state`. |
| `memory.refuse_writes_at` | `IWDB_MEMORY_REFUSE_WRITES_AT` | `0.9` | From this fraction on, commits, index and constraint creation, namespace creation and imports fail with `resource_exhausted` before they are logged. Deletes, drops, logins and reads go on. Writes resume once memory is 5 % of the limit below the line. `0.05 < warn_at <= refuse_writes_at <= 1`. |
| `limits.default.max_results` | `IWDB_LIMITS_DEFAULT_MAX_RESULTS` | `built in` | What a read gets if it asks for nothing (built in: 1000). |
| `limits.default.max_visited` | `IWDB_LIMITS_DEFAULT_MAX_VISITED` | `built in` | (built in: 100000) |
| `limits.default.max_edges` | `IWDB_LIMITS_DEFAULT_MAX_EDGES` | `built in` | (built in: 1000000) |
| `limits.default.timeout_ms` | `IWDB_LIMITS_DEFAULT_TIMEOUT_MS` | `built in` | (built in: 30000) |
| `limits.max.max_results` | `IWDB_LIMITS_MAX_MAX_RESULTS` | `built in` | What no read can exceed (built in: 100000). |
| `limits.max.max_visited` | `IWDB_LIMITS_MAX_MAX_VISITED` | `built in` | (built in: 10000000) |
| `limits.max.max_edges` | `IWDB_LIMITS_MAX_MAX_EDGES` | `built in` | (built in: 100000000) |
| `limits.max.timeout_ms` | `IWDB_LIMITS_MAX_TIMEOUT_MS` | `built in` | (built in: 300000) |
| `log.format` | `IWDB_LOG_FORMAT` | `"auto"` | `json`, `text`, or `auto`: JSON unless stderr is a terminal ([ADR 0042](../adr/0042-structured-logs.md)). |
| `log.level` | `IWDB_LOG_LEVEL` | `"info"` | A filter: a level, or directives such as `warn,iwdb_storage=debug`. |
| `log.tail_events` | `IWDB_LOG_TAIL_EVENTS` | `1000` | The last log events the server keeps for `GetLog` (`GET /v1/log`) and the console, at most 100 000; 0 keeps none ([ADR 0051](../adr/0051-the-status-views.md)). The same events as stderr, after `log.level`. |
| `console.enabled` | `IWDB_CONSOLE_ENABLED` | `false` | Serve the operator console at `/console/` (needs a build with the `console` feature; [ADR 0041](../adr/0041-console-served-by-the-server.md)). |
| `auth.enabled` | `IWDB_AUTH_ENABLED` | `true` | Every call but login, health, the console's pages and the OpenAPI document needs a token; a store without users refuses to start ([ADR 0047](../adr/0047-auth-bootstrap-and-configuration.md)). Off, every caller is a server-wide admin. |
| `auth.session_lifetime_secs` | `IWDB_AUTH_SESSION_LIFETIME_SECS` | `43200` | How long a login's session lasts (sessions also end at logout, a password change, the user's deletion and a restart). |
| `auth.login_max_failures` | `IWDB_AUTH_LOGIN_MAX_FAILURES` | `5` | Failed logins per user, and per client address, within the window; more are refused until it has passed. |
| `auth.login_window_secs` | `IWDB_AUTH_LOGIN_WINDOW_SECS` | `60` | The window of failed logins. |
| `auth.login_table_size` | `IWDB_AUTH_LOGIN_TABLE_SIZE` | `10000` | Users and addresses the slowdown remembers; the least recently seen is forgotten first. |
| `tls.enabled` | `IWDB_TLS_ENABLED` | `true` | Serve TLS only (gRPC and REST on the one port). Off: plaintext, which a non-loopback address also needs `server.plaintext_public` for ([ADR 0048](../adr/0048-tls-and-mtls.md)). |
| `tls.cert` | `IWDB_TLS_CERT` | `unset` | The certificate chain (PEM), the server's certificate first. Required with TLS on. |
| `tls.key` | `IWDB_TLS_KEY` | `unset` | Its private key (PEM: PKCS#8, PKCS#1 or SEC1). Required with TLS on. |
| `tls.client_ca` | `IWDB_TLS_CLIENT_CA` | `unset` | The CAs (PEM) client certificates are verified against: turns mTLS on. |
| `tls.client_auth` | `IWDB_TLS_CLIENT_AUTH` | `"optional"` | With `tls.client_ca`: `optional` (a client may present a certificate) or `required` (every request but health and the console's pages needs one). |
| `audit.dir` | `IWDB_AUDIT_DIR` | `unset` | Also write the audit log to a file per UTC day here (`audit-YYYY-MM-DD.jsonl`, JSON lines, mode 0600; the directory is made with 0700). Unset: the audit log is only in the log ([ADR 0049](../adr/0049-audit-log.md)). |
| `audit.retention_days` | `IWDB_AUDIT_RETENTION_DAYS` | `30` | Days of audit files to keep, today included; older ones are deleted at start and at each new day. `0`: keep them all. |
| `backup.dir` | `IWDB_BACKUP_DIR` | `unset` | The backup directory: the only place remote backups (`Backup`, `iwctl --server ... backup <name>`) are written, as `<dir>/<name>`, and where `Verify` and `PruneArchive` find backups by name. It must exist and not be inside the data directory. Unset: remote backups are refused ([ADR 0055](../adr/0055-admin-writes-and-iwctl-against-a-server.md)). |
| `backup.max_bytes_per_second` | `IWDB_BACKUP_MAX_BYTES_PER_SECOND` | `0` | Copy backups at most this fast (`0`: as fast as the disks go); a `Backup` request can set another rate. Checkpoints wait for a backup's whole copy, so a slower backup holds them back longer, and the WAL grows meanwhile. |
| `jobs.running` | `IWDB_JOBS_RUNNING` | `2` | Managed analytics jobs running at once, each on a thread of its own, never a query worker (so a long job doesn't hold back reads). Further jobs wait ([ADR 0056](../adr/0056-managed-analytics-jobs.md)). |
| `jobs.queued` | `IWDB_JOBS_QUEUED` | `16` | Jobs that may wait for a job thread; `StartJob` beyond that fails with `unavailable`. |
| `jobs.per_user` | `IWDB_JOBS_PER_USER` | `4` | A user's queued and running jobs at once (server admins included); more fail with `unavailable`. |
| `jobs.timeout_secs` | `IWDB_JOBS_TIMEOUT_SECS` | `3600` | The longest a job runs, from when it leaves the queue; it fails with `timeout` then. A `StartJob`'s `timeout_ms` can only lower it. |
| `jobs.retention_secs` | `IWDB_JOBS_RETENTION_SECS` | `3600` | How long an ended job and its result are kept; then `GetJob` answers `not_found`. |
| `jobs.max_finished` | `IWDB_JOBS_MAX_FINISHED` | `100` | Ended jobs kept at most; the one that ended first goes first. |
| `jobs.result_bytes` | `IWDB_JOBS_RESULT_BYTES` | `67108864` | Stored results' estimated size, all together (64 MiB). A new result drops the oldest (their jobs become `expired`); a result alone larger fails its job with `budget_exceeded`. Counted in the memory limit's `working` part. |

| `tracing.enabled` | `IWDB_TRACING_ENABLED` | `false` | Export traces over OTLP (needs a build with the `otel` feature; one without it refuses to start). See [Traces](#traces) ([ADR 0057](../adr/0057-traces.md)). |
| `tracing.endpoint` | `IWDB_TRACING_ENDPOINT` | `the protocol's` | The collector: `http://` or `https://` (verified against the system's roots). Unset: `http://127.0.0.1:4317` for `grpc`, `http://127.0.0.1:4318/v1/traces` for `http/protobuf`. For `http/protobuf`, `/v1/traces` is added to an endpoint without a path. |
| `tracing.protocol` | `IWDB_TRACING_PROTOCOL` | `"grpc"` | `grpc` (OTLP/gRPC) or `http/protobuf`. |
| `tracing.sample_ratio` | `IWDB_TRACING_SAMPLE_RATIO` | `1.0` | The share of new traces that are sampled, 0 to 1. A request with a `traceparent` follows its caller's decision. |
| `tracing.service_name` | `IWDB_TRACING_SERVICE_NAME` | `"iwdb-server"` | The traces' `service.name`. |
| `tracing.headers` | `IWDB_TRACING_HEADERS` | `unset` | Headers sent with every export, `name=value,name=value` (a vendor's API key: `authorization=Bearer ...`). `--check-config` prints their names, not their values. |

A test keeps this table equal to the code's list (`iwdb_server::config::KEYS`).

`[console] public` (`IWDB_CONSOLE_PUBLIC`) of step 16b is gone since step 15a: the console is behind authentication, and since step 15b the server speaks TLS. Either name is refused with a message that names `[tls]` and the two plaintext flags.

## TLS and mTLS

TLS is on by default ([ADR 0048](../adr/0048-tls-and-mtls.md)): a server without `tls.cert` and `tls.key` doesn't start, and the message names the alternatives. gRPC and REST share the port; ALPN offers `h2` and `http/1.1`. There is no certificate in the Docker image: mount one (`compose.yaml` mounts the one `docker/dev-cert.sh` makes, for development).

| You want | Set |
|---|---|
| TLS (the default) | `tls.cert`, `tls.key` |
| Plaintext on loopback (development, a sidecar) | `tls.enabled = false` |
| Plaintext on another address (behind a TLS-terminating proxy on a private network) | `tls.enabled = false` and `server.plaintext_public = true` |
| Client certificates, optional | also `tls.client_ca` |
| Client certificates, required | also `tls.client_auth = "required"` |

- **Reloading.** `SIGHUP` reads the certificate, key and client CA again and uses them for new connections; open connections keep theirs. A reload that fails (a file missing, a key that doesn't match its certificate) is logged at `error` and the files in use stay. Replace the certificate and key, then send the signal (`docker kill --signal HUP <container>`, `kill -HUP <pid>`).
- **mTLS.** A client certificate is verified against `tls.client_ca` (expired or of another CA: the handshake fails). Its subject's common name (CN) is the user, who must exist (a certificate never creates one); the user's roles apply as with a token. A bearer token or session cookie in the request wins over the certificate. A REST request other than GET or HEAD that only a certificate authenticates needs the `x-iwdb-csrf` header, as with the console's cookie: browsers send certificates on their own.
- **Required certificates.** With `tls.client_auth = "required"`, a request without a client certificate is refused with `unauthenticated` (logging in too); only health and the console's pages are answered, so probes and load balancers need no certificate.
- **Secrets.** A private key is never logged, printed by `--check-config` (it prints the path) or quoted in an error.

## Authentication and the first admin

Authentication is on by default ([ADR 0047](../adr/0047-auth-bootstrap-and-configuration.md)). There is no default password: a store without users refuses to start, with a message that names the two ways to make the first admin.

- **Offline:** `iwctl user create <data-dir> <name> --admin` while the server is stopped (it prompts for the password).
- **On first start:** `IWDB_AUTH_BOOTSTRAP_PASSWORD` (and `IWDB_AUTH_BOOTSTRAP_USER`, default `admin`). A start that finds no users creates that admin; a start that finds users ignores the variables with a warning. They are variables only (a password doesn't belong in a config file) and `--check-config` never prints them. Unset them after the first start.

Clients log in (`POST /v1/auth/login`, the `Login` RPC) or use an API token, and send `authorization: Bearer <token>` ([rest.md](rest.md#authentication), [grpc.md](grpc.md#authentication)). Failed logins are logged at `warn` (target `iwdb::auth`) with the user and the client address, never the password.

## Logs

Logs go to stderr, one event per line. As JSON (the default when stderr isn't a terminal: containers, services, pipes), each line is an object with `timestamp` (RFC 3339, UTC), `level`, `target` (the module), `message` and the event's fields; events during a request carry `span.path`. The library crates' records (recovery warnings, background threads) come out the same way.

Events at `info`: `listening; opening the store (recovery)` (`address`), `created the first admin` (`user`), logins (`user ... logged in from ...`, target `iwdb::auth`), `recovered` per namespace (`namespace`, `checkpoint`, `replayed`, `seq`, `torn_tail`), `recovery finished` (`took_ms`), `serving <dir> on <address>` (`address`, `recovery_ms`: the server is ready), `shutting down` and `no longer ready`, `closed <dir>`. Requests aren't logged, except those that fail on the server's side (`internal`, `corrupt`, `io`): an `error` event with `code`, `error` and the request's `span.path`.

## Health

| | gRPC | REST |
|---|---|---|
| Live | — (any answer) | `GET /v1/health/live`: 200 whenever the server answers |
| Ready | `grpc.health.v1.Health/Check`, service `""` or `ironweaver_db.v1.DatabaseService`: `SERVING` or `NOT_SERVING`; `Watch` follows it | `GET /v1/health/ready`: 200 or 503 |

The REST body is the `Health` message: `{"state": "HEALTH_STATE_RECOVERING"}`, `{"state": "HEALTH_STATE_READY", "ready": true}`, `{"state": "HEALTH_STATE_DRAINING"}`. The health routes are served in every build (also without the `rest` feature) and while the store recovers. Database calls before the server is ready fail with `unavailable`.

`iwdb-server --probe [--config <file>]` asks the readiness route of the configured server (file and `IWDB_*` variables: the `listen` port, a wildcard address as loopback, over TLS unless `tls.enabled = false`) and exits 0 when ready, 1 otherwise, 2 for a configuration it can't read: the Docker image's `HEALTHCHECK`, with no curl needed. `iwdb-server --probe https://<host:port>` (or `http://`) asks that address instead. Over TLS the probe doesn't verify the server's certificate: it sends no credentials and reads only readiness. A Kubernetes pod can use the gRPC probe or `httpGet` on `/v1/health/ready` for readiness and `/v1/health/live` for liveness.

## Backups

An operator backs up, checkpoints, verifies and prunes a running server with `iwctl --server` ([iwctl.md](../iwctl.md)), the `AdminService` RPCs ([grpc.md](grpc.md)) or REST ([rest.md](rest.md#admin-writes)); all need a server-wide admin, and every call is audited ([ADR 0055](../adr/0055-admin-writes-and-iwctl-against-a-server.md)).

- **Where backups go.** Only into `backup.dir`, by name: one path component of 1 to 128 ASCII letters, digits, `.`, `_` or `-`, not starting with `.`. A name under which anything exists (a file, a directory, a symlink) is refused with `conflict`: a backup never overwrites. A backup that fails is removed. Keep the directory writable only by the server's user.
- **Throttling.** `backup.max_bytes_per_second` (or the request's rate). Checkpoints wait for the whole copy: at 10 MiB/s, a 50 GiB store holds them back about 85 minutes, and the WAL grows by everything committed meanwhile. `iwdb_backup_running` shows it, and `rate(iwdb_backup_bytes_total[1m])` the copy's speed ([metrics.md](metrics.md)).
- **Under the memory limit** checkpoints, backups and pruning go on; verifying is refused with `resource_exhausted` while writes are refused (it replays each namespace into memory the limit doesn't count).
- **Restore is offline**: `iwctl restore` on the server's host into a new directory, then start a server on it.
- **The archive.** With `store.archive`, `iwctl --server <endpoint> archive prune --before <backup>` removes what no restore from that backup (or a later one) needs; keep the oldest backup you want to restore from.

## Managed jobs

An analytics job that may take longer than a request's timeout (`limits.max.timeout_ms`) runs as a managed job: `StartJob` (gRPC), `POST /v1/namespaces/{ns}/jobs` (REST), then watched, cancelled and fetched by id, with `iwctl --server <endpoint> jobs ...` or the console's status page ([ADR 0056](../adr/0056-managed-analytics-jobs.md)).

- **Who.** Starting needs `read` on the namespace; a job is seen, cancelled and fetched by its owner and server admins (others get `not_found`). Starting and cancelling are audited.
- **Where they run.** On `jobs.running` threads of their own, not the query workers. A queued or running job is also listed by `ListRequests` (as `StartJob`, with the job's id), and `CancelRequest` cancels it.
- **Memory.** A running job's projection and every stored result count in the memory limit's `working` part, so jobs add at most `jobs.running` projections plus `jobs.result_bytes`. Jobs still start while writes are refused, as reads do.
- **Shutdown.** When the server drains, every queued and running job is cancelled ("the server is shutting down") and new ones are refused. Nothing is kept across a restart: a job's id then answers `not_found`.

## Audit log

Every login (and failed login), logout, user, grant, token, namespace and catalog change, every cancelled request (`CancelRequest`, step 16c), every admin write (`Checkpoint`, `Backup`, `Verify`, `PruneArchive`, step 16e), every started or cancelled job (`StartJob`, `CancelJob`, step 16f), and every refused request (`unauthenticated`, `permission_denied`), leaves one audit entry ([ADR 0049](../adr/0049-audit-log.md)). Reads and data commits don't (the change stream is for those).

- **In the log.** Entries are log events of target `iwdb::audit` at `info`, in the log's format. They pass whatever `log.level` says unless the level names `iwdb::audit` itself (`warn,iwdb::audit=off` turns them off in the log).
- **In files** with `audit.dir`: the same entries as JSON lines, one file per UTC day, the files older than `audit.retention_days` deleted.
- **How long they are kept.** The audit files: `audit.retention_days` (30 by default). Entries in the log (stderr) are kept as long as whatever collects it keeps them: set its limits there (Docker: the log driver's `max-size` and `max-file`; journald: `MaxRetentionSec`, `SystemMaxUse`).
- **Fields** (absent ones left out): `operation` (the RPC: `Login`, `CreateUser`, `CommitCatalog`, ...), `outcome` (`success` or `failure`), `code` (the error code of a failure), `user`, `auth` (`session`, `api_token`, `certificate`, `off`), `client` (the client's IP address), `namespace`, `subject` (the user an account change is about), `token_name`, `role`, `admin`, `seq` (a catalog change's commit), `namespace_event` (a namespace's creation or drop), `request` (the id of the request a `CancelRequest` cancels, or of the job a `StartJob` started or a `CancelJob` cancels; a cancelled one's owner is the `subject`), `backup` (the backup a `Backup`, `Verify` or `PruneArchive` names, by its name in the backup directory). Never a password, a token or its hash, a certificate, an error message or a value of the data.
- **Best effort.** An entry is written after the outcome is known, without fsync: a crash can lose the last entries, never the changes, which are in the WAL.

## Traces

With a build that has the `otel` feature (the Docker image has it) and `tracing.enabled = true`, the server exports a trace of every request to an OpenTelemetry collector over OTLP ([ADR 0057](../adr/0057-traces.md)): the request, its wait for a worker, its execution, and for writes the commit, the WAL append and the fsync. guarantees.md's [Traces](../guarantees.md#traces-step-16g) lists the spans and what is never in them.

```
IWDB_TRACING_ENABLED=true IWDB_TRACING_ENDPOINT=http://otel-collector:4317 iwdb-server --config server.toml
```

- **Callers' traces.** A request with a W3C `traceparent` (and `tracestate`) header, over gRPC or REST, joins the caller's trace; a malformed one is ignored and the request starts a new trace. Nothing is echoed in the answer.
- **When the collector is down** spans are dropped, never waited for: requests don't slow down or fail. `iwdb_trace_spans_dropped_total` counts the drops ([metrics.md](metrics.md)), and the log says once when exports start failing and once when they work again.
- **Shutdown** exports what is queued, within what is left of `server.drain_timeout_secs` (at least a second).
- **`OTEL_*` variables are not read**: set the `IWDB_TRACING_*` ones instead. The server warns at start about `OTEL_*` variables it finds.

Instead of the standard variables:

- `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`: `IWDB_TRACING_ENDPOINT`
- `OTEL_EXPORTER_OTLP_PROTOCOL`: `IWDB_TRACING_PROTOCOL`
- `OTEL_EXPORTER_OTLP_HEADERS`: `IWDB_TRACING_HEADERS`
- `OTEL_SERVICE_NAME`: `IWDB_TRACING_SERVICE_NAME`
- `OTEL_TRACES_SAMPLER_ARG` (with `parentbased_traceidratio`): `IWDB_TRACING_SAMPLE_RATIO`
- `OTEL_SDK_DISABLED`, `OTEL_TRACES_EXPORTER=none`: `IWDB_TRACING_ENABLED=false`
