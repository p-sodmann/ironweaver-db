# ADR 0034: The Docker image, and REST and Postgres as server features

Status: accepted
Date: 2026-10-04

## Context

`iwdb-server` came in one shape: gRPC, REST/JSON (ADR 0030) and the Postgres source of projections (ADR 0032), the last turned on in `iwdb` unconditionally. A deployment that only speaks gRPC, or never projects from Postgres, still compiled and shipped the code: axum, pbjson and the OpenAPI generator for REST; `postgres` and `tokio-postgres` for projections. There was no Docker image; step 17 planned one with the release pipeline.

## Decision

**Features.** `iwdb-server` gets two cargo features, both in `default`:

- `rest`: the `rest` module (routes, JSON serde, OpenAPI), the messages' pbjson serde from `build.rs`, the descriptor set, `client::RestRemote`, and the crates only they use (axum, pbjson, pbjson-build, prost-types, serde_json, http-body-util). gRPC isn't a feature: it is the server.
- `postgres`: turns on `iwdb/postgres`.

Defaults stay on, so nothing changes for existing builds, users or CI's `--all-features` jobs. Leaving a feature out is a deliberate choice for a smaller build.

**Behaviour without a feature.**
- Without `rest`, the dispatcher (`serve.rs`) still tells gRPC requests apart by content type, and answers every other one 404 with an empty body, on HTTP/1.1 and HTTP/2. The connection stays usable.
- Without `postgres`, the config file's schema doesn't change: `kind = "postgres"` still parses, so the same file means the same thing to every build, and the error is about the build, not the syntax. `Config::parse` refuses it ("this iwdb-server was built without the postgres feature"), so the server exits 2 before it opens the store.
- `--version` lists the features (`(grpc, rest, postgres)`), so an operator can tell which build is running.

**Image.** A multi-stage `Dockerfile` at the root:
- The build stage is `rust:1.99-trixie`, the MSRV (ADR 0029), with `--locked`. `rust-toolchain.toml` is left out of the build context, because its floating `stable` channel would make rustup download a toolchain inside every build. Protos compile with protox, so no `protoc` is needed.
- The runtime stage is `debian:trixie-slim`: glibc, the same Debian as the build stage, and a shell for `docker exec`. Distroless or static musl would be smaller, but would need a musl build of the whole tree for a gain that matters little next to the data.
- The user is `iwdb` (uid 10001), the data directory is the volume `/var/lib/iwdb`, the port is 7600, and the config is `/etc/iwdb/iwdb.toml`. The image's config sets `drain_timeout_secs = 8`, below `docker stop`'s 10 s, so a default stop ends with the final checkpoint instead of a SIGKILL during the drain. A SIGKILL loses nothing logged, but the next start replays the WAL.
- No `HEALTHCHECK`: there is no health endpoint before step 16, and a TCP probe would report healthy during recovery. (Step 16b added one: `iwdb-server --probe`, ready once recovery has finished, [ADR 0040](0040-health-and-readiness.md). It also added the `console` feature, [ADR 0041](0041-console-served-by-the-server.md), built into the image but off by default.)

## Consequences

- Leaving out `rest` saves the REST crates and code generation. Leaving out `postgres` saves the Postgres client and its own tokio runtime thread per source.
- Tests that need a feature are gated on it. CI runs `iwdb-server` without features and with each feature alone (job `features`), and builds and smoke-tests both images (job `docker`). The test crate's dev-dependency on itself sets `default-features = false`; otherwise it would turn the defaults back on and `--no-default-features` would test nothing.
- Each new optional dependency of the server should be a feature too if a deployment can do without it.
- Step 17 publishes this image (multi-arch, registry, tags) instead of writing one. Step 16's environment overrides will let the image be configured without mounting a file.
