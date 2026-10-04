# ADR 0040: Health and readiness

Status: accepted
Date: 2026-10-04

## Context

Orchestrators need to tell a live process from one that can serve. Until step 16b the server opened the store (recovery: replaying the WAL, which takes a while on a large store) before it bound its port, so during recovery nothing answered and a TCP probe couldn't tell "starting" from "dead"; ADR 0034 left the Docker image without a `HEALTHCHECK` for that reason. On shutdown the server sent GOAWAY at once, so a load balancer learned of it only from failed calls. Points to decide: where health is decided, how it is served over gRPC and REST, what "ready" promises, and the order of events at startup and shutdown.

## Decision

- **Health is the server's, not the `Database` trait's.** While recovery runs there is no database to ask. `iwdb_server::health::Health` holds the phase (`Recovering`, `Ready`, `Draining`) in a `watch` channel; the lifecycle moves it, and every adapter reads it. Design rule 8 holds: health is implemented once, and gRPC, REST and `--probe` only translate.
- **Bind first, then recover.** `iwdb_server::launch` binds the listener, serves at once, and opens the store on a blocking thread. A gate in front of both APIs answers health at any time and database calls only once the store is open; before that they fail with `unavailable` ("the server is recovering"), over gRPC with the `iwdb-code` trailer and over REST with an `Error` body and 503. The lifecycle moved from `main.rs` into the library so tests drive it in-process.
- **Ready means recovery has finished and shutdown hasn't begun.** The phase turns `Ready` only after the store's open has returned, so every committed record is visible at the first ready answer (tested: a held open, and the binary on a 100 000-node WAL).
- **gRPC: the standard protocol.** `grpc.health.v1.Health`, with service `""` and `ironweaver_db.v1.DatabaseService`: `SERVING` when ready, `NOT_SERVING` otherwise; other services are `NOT_FOUND` (Check) or `SERVICE_UNKNOWN` (Watch). So Kubernetes' gRPC probes and `grpc_health_probe` work. The messages come from `tonic-health` (from the tonic project); the service is ours, over the phase channel, because `tonic-health`'s reporter is async and keeps its own state.
- **REST: two routes.** `GET /v1/health/live` (200 whenever the server answers) and `GET /v1/health/ready` (200 or 503), with the `Health` message of the contract (`proto/ironweaver_db/v1/health.proto`) as body. They are in the route table and the OpenAPI document, but the gate serves them, so they work during recovery and in a build without the `rest` feature (written by hand there; a test compares it with pbjson's).
- **Shutdown turns readiness off first.** On the first signal the phase turns `Draining`; the server keeps serving for `[server] unready_delay_ms` (default 0, so nothing changes unless asked: set it to a few seconds behind a load balancer), then drains as before (ADR 0027). gRPC health watches send `NOT_SERVING` and end, so they don't hold the drain. A signal during recovery waits for the open to finish (recovery can't stop halfway), then closes the store without becoming ready.
- **`iwdb-server --probe [<host:port>]`.** The readiness route over plain HTTP/1.1 with the standard library; exit 0 when ready. It is the Docker image's `HEALTHCHECK` (no curl in the image).

## Consequences

- During recovery clients get a fast `unavailable` instead of a refused connection; both are retryable, and `unavailable` says why.
- A health watcher sees the whole life: `NOT_SERVING`, `SERVING`, `NOT_SERVING`, end.
- The gate adds one path comparison per request.
- The `serving <dir> on <address>` log event now means "ready"; a `listening` event comes first. Tools that waited for the old first line (the tests, the Python fixture) wait for the `serving` event instead.
