# Step 13a: Docker image and optional server features

Status: done
Milestone: M3 Network access
Depends on: step 13

## Goal

Run the server in Docker, and build it without the parts a deployment doesn't use: REST/JSON and the Postgres source of projections become cargo features of `iwdb-server`, on by default. Design: [ADR 0034](../adr/0034-docker-image-and-server-features.md).

This step pulls part of step 17's image task forward. Publishing the image (multi-arch, a registry, tags per release) stays in step 17.

## Tasks

### Features
- [x] `iwdb-server` features: `default = ["rest", "postgres"]`. `rest` gates the REST routes, the OpenAPI document, the messages' JSON serde (pbjson) and `client::RestRemote`, along with axum, pbjson, pbjson-build, prost-types, serde_json and http-body-util. `postgres` turns on `iwdb/postgres`. gRPC is always there.
- [x] Without `rest`, every request that isn't gRPC is answered 404 with an empty body.
- [x] Without `postgres`, the config schema is unchanged, but a `kind = "postgres"` source is an error when the config is read (exit 2, before the store opens) that names the projection and the missing feature.
- [x] `iwdb-server --version` lists the build's features: `iwdb-server 0.1.0 (grpc, rest, postgres)`.
- [x] Tests run in every feature set: the REST tests and the SSE parts of `changes.rs` need `rest`, the projection test in `binary.rs` needs `postgres`, and new tests cover the gRPC-only 404, the `--version` output and the missing-feature error. The test crate's dev-dependency on itself sets `default-features = false`, so that `--no-default-features` really tests a gRPC-only server.
- [x] CI job `features`: clippy and tests of `iwdb-server` with no features, `rest` alone and `postgres` alone, and a check that the gRPC-only dependency tree has no `postgres`, `pbjson`, `axum` or `prost-types`.

### Docker
- [x] `Dockerfile`: a multi-stage build on `rust:1.99-trixie` (BuildKit cache mounts, `--locked`, the `FEATURES` build argument) into `debian:trixie-slim`. It ships `iwdb-server` and `iwctl`, runs as the non-root user `iwdb` (uid 10001), has `/var/lib/iwdb` as a volume, exposes 7600 and stops with SIGTERM.
- [x] `docker/iwdb.toml`: the image's config, listening on `0.0.0.0:7600` with `drain_timeout_secs = 8`, so that a plain `docker stop` (10 s) still ends with the final checkpoint.
- [x] `.dockerignore`: everything that isn't a source of the build, and `rust-toolchain.toml` (it would make rustup download the current stable inside the build).
- [x] `compose.yaml`: the server on `127.0.0.1:7600` with a named volume, plus Postgres under the `postgres` profile, with `docker/projection.example.toml` and `docker/postgres-init.sql` as a working projection example.
- [x] CI job `docker`: build both images (all features and gRPC only), check `--version`, serve, answer `/v1/openapi.json` and `/v1/namespaces`, then `docker stop` with exit code 0 and a closed store.

### Docs
- [x] ADR 0034, [api/grpc.md](../api/grpc.md) ("Features and Docker"), [api/projections.md](../api/projections.md).

## Acceptance criteria

- `cargo clippy`/`cargo test` of `iwdb-server` pass with no features, each feature alone, and all features.
- A gRPC-only build has no Postgres or REST crates in its dependency tree.
- `docker build .` gives an image that serves gRPC and REST and shuts down cleanly on `docker stop`. CI checks this; the step was written on a machine without a Docker daemon.

## Non-goals

- Publishing the image, multi-arch builds, a Helm chart: step 17.
- Environment overrides of the config, health and readiness endpoints (and so a `HEALTHCHECK`): step 16.
- TLS and authentication: step 15. Until then the compose file publishes the port on localhost only.
- Optional features for the embedded crates: `iwdb` already has `postgres` (off by default), and nothing else in them pulls in a large dependency.
