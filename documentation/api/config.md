# Server configuration

`iwdb-server` reads its settings from a TOML file (`--config <file>`), from `IWDB_*` environment variables, or both; a variable wins over the file ([ADR 0039](../adr/0039-configuration-and-environment-overrides.md)). Only `data_dir` is required, so in a container `IWDB_DATA_DIR` alone is enough and no file needs mounting.

```
iwdb-server --config server.toml                      # file, plus any IWDB_* variables
IWDB_DATA_DIR=/var/lib/iwdb IWDB_AUTH_BOOTSTRAP_PASSWORD=... iwdb-server   # first start: creates the admin
iwdb-server --check-config --config server.toml       # validate; print the effective settings and their sources
```

## Rules

- **Names.** A setting's variable is `IWDB_` and its path in the file in upper case, with `.` as `_`: `store.fsync` is `IWDB_STORE_FSYNC`, `limits.max.timeout_ms` is `IWDB_LIMITS_MAX_TIMEOUT_MS`.
- **Values.** Numbers as digits, booleans as `true`/`false` (or `1`/`0`), choices by name (`group`), paths and addresses as they are.
- **Typos are errors.** A variable that starts like a section (`IWDB_STORE_`, `IWDB_SERVER_`, `IWDB_LIMITS_`, `IWDB_LOG_`, `IWDB_CONSOLE_`, `IWDB_AUTH_`) but names no setting stops the server (apart from the two bootstrap variables below). Other `IWDB_*` variables (test harnesses use some) are ignored.
- **Every problem at once.** Startup checks the file and the variables before it opens the store, and lists every problem with where it came from (the file, a variable), then exits with code 2. A TOML syntax or type error in the file is reported with its line; the file's other checks wait until it parses.
- **Relative `data_dir`.** From the file: relative to the file's directory. From `IWDB_DATA_DIR`: relative to the working directory.
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
| `server.drain_timeout_secs` | `IWDB_SERVER_DRAIN_TIMEOUT_SECS` | `30` | How long running calls may finish on shutdown. |
| `server.max_message_bytes` | `IWDB_SERVER_MAX_MESSAGE_BYTES` | `67108864` | Largest request or answer message (REST: request body); at least 1024. |
| `server.workers` | `IWDB_SERVER_WORKERS` | `0` | Threads running requests (0: one per CPU). |
| `server.queue` | `IWDB_SERVER_QUEUE` | `1024` | Requests waiting for a worker; more fail with `unavailable`. |
| `server.unready_delay_ms` | `IWDB_SERVER_UNREADY_DELAY_MS` | `0` | On shutdown, serve this long with readiness off before draining, so load balancers stop sending first ([ADR 0040](../adr/0040-health-and-readiness.md)). |
| `server.plaintext_public` | `IWDB_SERVER_PLAINTEXT_PUBLIC` | `false` | Allow a non-loopback `listen` address. The server has no TLS until step 15b, so passwords, tokens and data cross the network in clear (and with authentication off, anyone who reaches the port can change everything); without this flag such an address is refused ([ADR 0047](../adr/0047-auth-bootstrap-and-configuration.md)). |
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
| `console.enabled` | `IWDB_CONSOLE_ENABLED` | `false` | Serve the operator console at `/console/` (needs a build with the `console` feature; [ADR 0041](../adr/0041-console-served-by-the-server.md)). |
| `auth.enabled` | `IWDB_AUTH_ENABLED` | `true` | Every call but login, health, the console's pages and the OpenAPI document needs a token; a store without users refuses to start ([ADR 0047](../adr/0047-auth-bootstrap-and-configuration.md)). Off, every caller is a server-wide admin. |
| `auth.session_lifetime_secs` | `IWDB_AUTH_SESSION_LIFETIME_SECS` | `43200` | How long a login's session lasts (sessions also end at logout, a password change, the user's deletion and a restart). |
| `auth.login_max_failures` | `IWDB_AUTH_LOGIN_MAX_FAILURES` | `5` | Failed logins per user, and per client address, within the window; more are refused until it has passed. |
| `auth.login_window_secs` | `IWDB_AUTH_LOGIN_WINDOW_SECS` | `60` | The window of failed logins. |
| `auth.login_table_size` | `IWDB_AUTH_LOGIN_TABLE_SIZE` | `10000` | Users and addresses the slowdown remembers; the least recently seen is forgotten first. |

A test keeps this table equal to the code's list (`iwdb_server::config::KEYS`).

`[console] public` (`IWDB_CONSOLE_PUBLIC`) of step 16b was replaced by `server.plaintext_public` in step 15a: the console is behind authentication now, and what remains to opt into is the lack of TLS. Either name is refused with a message that says so.

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

`iwdb-server --probe [<host:port>]` asks the readiness route (default `127.0.0.1:7600`) and exits 0 when ready, 1 otherwise: the Docker image's `HEALTHCHECK`, with no curl needed. A Kubernetes pod can use the gRPC probe or `httpGet` on `/v1/health/ready` for readiness and `/v1/health/live` for liveness.
