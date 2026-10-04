# Step 16b: Configuration, health and logs

Status: done
Milestone: M4 Production 1.0
Depends on: step 13a (the server features and the Docker image)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

An operator can configure the server from a file, from the environment or both, and learns of every mistake before the store opens; an orchestrator can tell a live process from a ready one (ready only once recovery finished); and the server's output is structured logs a collector can parse.

## Tasks

### Configuration ([ADR 0039](../adr/0039-configuration-and-environment-overrides.md))
- [x] Environment overrides of every scalar config key, `IWDB_<SECTION>_<KEY>` (`IWDB_DATA_DIR`, `IWDB_LISTEN`, `IWDB_STORE_FSYNC`, `IWDB_LIMITS_MAX_TIMEOUT_MS`, …), applied over the file. The variables come from one table in `config.rs` (`KEYS`), so the docs and a test can list them. A variable under a section prefix that names no setting is an error; other `IWDB_*` variables are ignored (see "Plan change"). `[[projection]]`s stay file-only (they keep `url_env`).
- [x] The file is optional when `IWDB_DATA_DIR` is set: `iwdb-server` with no arguments runs from the environment, so the Docker image can be configured without mounting a file.
- [x] Validation at startup, before the store opens: every error at once, each naming its key and where the value came from (the variable; a TOML error in the file with its line), with exit code 2. The checks that were in `Config::parse` (limits, group commit, message size, projections) report together now, plus the log filter and the console.
- [x] `iwdb-server --check-config [--config <file>]`: validate, print the effective config as TOML with each value's source, exit 0 or 2.

### Health and readiness ([ADR 0040](../adr/0040-health-and-readiness.md))
- [x] The server binds its port before it opens the store, and serves health while recovery runs. Every database request during recovery answers `unavailable` ("the server is recovering"), over gRPC and REST.
- [x] Liveness and readiness, decided once in `iwdb-server` (`health::Health`, a phase the lifecycle drives: `recovering`, `ready`, `draining`), not in the `Database` trait. Ready means: recovery finished and shutdown hasn't started.
  - gRPC: the standard health protocol `grpc.health.v1.Health` (Check and Watch; service `""` and `ironweaver_db.v1.DatabaseService`).
  - REST: `GET /v1/health/live` and `GET /v1/health/ready`, 200 or 503 with the `Health` message (`proto/ironweaver_db/v1/health.proto`), in the route table and the OpenAPI document. Served in every build, also without the `rest` feature.
- [x] Draining turns readiness off first; `[server] unready_delay_ms` keeps serving that long before the drain, so a load balancer stops sending before connections get GOAWAY. Health watches end after `NOT_SERVING`, so they don't hold the drain.
- [x] The lifecycle moves from `main.rs` into the library (`iwdb_server::launch`), so tests drive it in-process.
- [x] The Docker image gets a `HEALTHCHECK` through `iwdb-server --probe <addr>` (readiness over HTTP/1.1, exit 0 or 1), since the image has no curl.

### Logs ([ADR 0042](../adr/0042-structured-logs.md))
- [x] Structured logs with `tracing` and `tracing-subscriber`: `[log] format = "auto" | "json" | "text"` (auto: text on a terminal, JSON otherwise) and `level`, an `EnvFilter` directive (`IWDB_LOG_LEVEL`). JSON lines carry timestamp, level, target, message and fields at the top level.
- [x] The library crates keep `log`; the subscriber bridges it (`tracing-log`), so pure-Rust crates gain no dependency (design rule 1). The `eprintln!` lines of `main.rs` became events with fields. Listening, recovery (per namespace: checkpoint, records replayed, seq, torn tail; then the duration), ready (`serving`), shutdown and close each log one event.
- [x] No request logging; errors of type `internal`, `corrupt` and `io` are logged at `error` with their code, inside the request's span (its path).

### The operator console, served by the server ([ADR 0041](../adr/0041-console-served-by-the-server.md); added, see "Plan change")
- [x] Feature `console` (implies `rest`, off by default): the pages compiled in, served at `/console/` when `[console] enabled = true`; refused on a non-loopback address unless `[console] public = true` (no authentication until step 15).
- [x] `rest.js` reads `console-config.json` relative to the page (proxy and server both work) and shows the server's readiness.
- [x] The Docker image builds with it (off); `compose.yaml` turns it on (port published on localhost).

### Tests and docs
- [x] Config: every override, every validation error at once with its source, unknown variables, `--check-config` reading back, the console rules; `documentation/api/config.md` tested against `KEYS` (keys, variables, defaults).
- [x] Readiness with a slow recovery (design rule 3): `tests/health.rs` holds the store's open at a gate (live 200, ready 503, gRPC `NOT_SERVING`, database calls `unavailable` over gRPC and REST, then ready with every committed record readable at the first ready answer); a failed open never becomes ready; a stop during recovery closes the store cleanly. `tests/binary.rs` recovers a 100 000-node WAL and polls readiness until it flips, seeing it unready first.
- [x] Readiness turns off at the start of a drain (gRPC Watch and REST), and the server serves during the unready delay.
- [x] Logs: every line of the binary is JSON with timestamp, level and target; the `serving` event carries `address`.
- [x] ADRs 0039 to 0042. `guarantees.md`: what "ready" promises. `grpc.md`, `rest.md` (now tested against the route table), `errors.md`, `config.md`, `openapi.json`.

## Plan change

2026-10-04, while implementing:

- **The console moved in here** from step 16c's decision: the owner asked for `iwdb-server` to serve it now. Step 15 isn't done, so it is opt-in twice (a cargo feature and a config switch) and refused on a non-loopback address without `public = true` (ADR 0041).
- **`IWDB_LOG` became `IWDB_LOG_LEVEL`**, the variable of `[log] level`, like every other setting; one naming rule instead of an exception.
- **Unknown `IWDB_*` variables are errors only under a section prefix.** The test harnesses and the console proxy use `IWDB_SERVER`, `IWDB_URL` and others; refusing every unknown `IWDB_*` would break them.
- **`[server] unready_delay_ms` was added**: without a delay "readiness off first" would last microseconds.
- **The `serving` log event now means ready.** It came first before; now `listening` does. The binary's tests and the Python fixture wait for `serving` in the JSON lines.

## Notes

- New dependencies: `tonic-health` (the `grpc.health.v1` messages and service trait, from the tonic project; the service is ours, over the phase channel), `tracing` (already in the tree through tokio) and `tracing-subscriber` with `json`, `env-filter`, `ansi` and `tracing-log` (structured lines, filters, the `log` bridge); `toml` gains its `display` feature for `--check-config`. In `iwdb-server` only. None is behind a feature: every deployment needs health and logs (ADR 0034 asks for a feature only where a deployment can do without).

## Acceptance criteria

- A bad config (file or environment) fails with every problem listed, before the store opens. (`a_bad_configuration_lists_every_problem`, `every_problem_is_reported_at_once`)
- The server runs from the environment alone. (`runs_from_the_environment_alone`)
- Readiness is false until recovery finished, proven with a held and a large recovery, over gRPC and REST. (`not_ready_until_recovery_has_finished`, `ready_only_after_a_large_recovery`)
- Logs are JSON lines by default when not on a terminal. (`serves_its_data_directory_and_shuts_down_on_sigterm`)

## Non-goals

- Metrics, status views, cancel (step 16c); traces (step 16g).
- Reloading the config without a restart.
- TLS and authentication for the health endpoints or the console (step 15; probes stay unauthenticated by design).
