# Step 16b: Configuration, health and logs

Status: todo
Milestone: M4 Production 1.0
Depends on: step 13a (the server features and the Docker image)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

An operator can configure the server from a file, from the environment or both, and learns of every mistake before the store opens; an orchestrator can tell a live process from a ready one (ready only once recovery finished); and the server's output is structured logs a collector can parse.

## Tasks

### Configuration
- [ ] Environment overrides of every scalar config key, `IWDB_<SECTION>_<KEY>` (`IWDB_DATA_DIR`, `IWDB_LISTEN`, `IWDB_STORE_FSYNC`, `IWDB_LIMITS_MAX_TIMEOUT_MS`, …), applied over the file. The variables come from one table in `config.rs`, so the docs and a test can list them; an unknown `IWDB_*` variable is an error, not ignored. `[[projection]]`s stay file-only (they keep `url_env`).
- [ ] The file is optional when `IWDB_DATA_DIR` is set: `iwdb-server` with no arguments runs from the environment, so the Docker image can be configured without mounting a file.
- [ ] Validation at startup, before the store opens: every error at once, each naming its key and where the value came from (file and line, or the variable), with exit code 2. Ranges that are checked today only by their users move here (limits: default ≤ max, workers, queue, message size, group commit).
- [ ] `iwdb-server --check-config [--config <file>]`: validate, print the effective config as TOML with each value's source, exit 0 or 2.

### Health and readiness
- [ ] The server binds its port before it opens the store, and serves health while recovery runs. Every database request during recovery answers `unavailable` ("recovering"), over gRPC and REST.
- [ ] Liveness and readiness, decided once in `iwdb-server` (a `Health` state the lifecycle drives: `starting`, `recovering`, `ready`, `draining`), not in the `Database` trait: before recovery ends there is no database to ask. Ready means: recovery finished, the listener accepts, and shutdown hasn't started.
  - gRPC: the standard health protocol `grpc.health.v1.Health` (Check and Watch; service `""` and `ironweaver_db.v1.DatabaseService`), so Kubernetes' gRPC probes and `grpc_health_probe` work.
  - REST: `GET /v1/health/live` and `GET /v1/health/ready`, 200 or 503 with a small JSON body (`{"status": "recovering"}`), in the route table and the OpenAPI document. Without the `rest` feature these paths are still served, since probes need them; only the JSON API is left out.
- [ ] Draining (ADR 0027) turns readiness off first, so a load balancer stops sending before connections get GOAWAY.
- [ ] The lifecycle moves from `main.rs` into the library (`iwdb_server::run` or a `Launch` builder), so tests drive it in-process.
- [ ] The Docker image gets a `HEALTHCHECK` (ADR 0034 left it out for want of an endpoint), through `iwdb-server --probe <addr>` (readiness over HTTP/1.1, exit 0 or 1), since the image has no curl.

### Logs
- [ ] Structured logs with `tracing` and `tracing-subscriber`: `[log] format = "json" | "text"` (text on a terminal, JSON otherwise, by default), `level`, and `IWDB_LOG` as an `EnvFilter`. JSON lines carry time, level, target, message and fields.
- [ ] The library crates keep `log`; the binary bridges it (`tracing-log`), so pure-Rust crates gain no dependency (design rule 1). The `eprintln!` lines of `main.rs` become events with fields (`data_dir`, `listen`, `drain`, …). Startup, recovery (duration, records replayed, per namespace), readiness, shutdown and drain each log one event.
- [ ] No request logging by default (volume); errors of type `internal`, `corrupt` and `io` are logged with their request's operation and namespace.

### Tests and docs
- [ ] Config: every override, every validation error (all at once, with source), an unknown `IWDB_*` variable, `--check-config`; a test that the variable table, `grpc.md`'s config section and the docs agree.
- [ ] Readiness with a slow recovery (design rule 3): a test whose store open is held at a gate shows live = 200, ready = 503 and database calls `unavailable` until the gate opens, then ready = 200 and every committed record readable at the first ready answer; and a test against the binary that recovers a WAL large enough to take measurable time and polls readiness until it flips (no `ready` before recovery's last record is applied).
- [ ] Readiness turns off at the start of a drain (gRPC Watch and REST).
- [ ] Logs: JSON lines parse and carry the fields; `IWDB_LOG` filters.
- [ ] ADRs: configuration and environment overrides; health and readiness semantics; logging (`tracing`, JSON, the `log` bridge). `guarantees.md`: what "ready" promises. `grpc.md`/`rest.md`/`errors.md`/`openapi.json` for the new routes and `unavailable` during recovery.

## Notes

- New dependencies (justified in the PR): `tracing`, `tracing-subscriber` (`json`, `env-filter`), `tracing-log` in the binary; `tonic-health` for the standard gRPC health service (or the service written against its proto, if that is smaller; decide in the ADR). None goes into a pure-Rust crate below the server.
- Not behind a feature: every deployment needs health and logs (ADR 0034 asks for a feature only where a deployment can do without).

## Acceptance criteria

- A bad config (file or environment) fails with every problem listed, before the store opens.
- The server runs from the environment alone.
- Readiness is false until recovery finished, proven with a held and a large recovery, over gRPC and REST.
- Logs are JSON lines by default when not on a terminal.

## Non-goals

- Metrics, status views, cancel (step 16c); traces (step 16g).
- Reloading the config without a restart.
- TLS and authentication for the health endpoints (step 15; probes stay unauthenticated by design).
