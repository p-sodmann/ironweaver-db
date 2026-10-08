# syntax=docker/dockerfile:1
#
# The iwdb-server image (step 13a, ADR 0034): iwdb-server and iwctl on Debian
# slim, as a non-root user, with the data directory as a volume.
#
#   docker build -t iwdb .                                # gRPC, REST, Postgres projections, console, traces
#   docker build --build-arg FEATURES="" -t iwdb:grpc .   # gRPC only
#   docker run -p 127.0.0.1:7600:7600 -v iwdb-data:/var/lib/iwdb \
#     -v "$PWD/docker/tls:/etc/iwdb/tls:ro" -e IWDB_AUTH_BOOTSTRAP_PASSWORD=... iwdb
#
# FEATURES are iwdb-server's cargo features (rest, postgres, console, otel),
# space separated. The console is compiled in but off: IWDB_CONSOLE_ENABLED=true
# turns it on. So are traces (ADR 0057): IWDB_TRACING_ENABLED=true and
# IWDB_TRACING_ENDPOINT=http://<collector>:4317 send them over OTLP. The server speaks TLS only (step 15b): mount the certificate
# and key at /etc/iwdb/tls/server.pem and server.key (docker/dev-cert.sh makes
# a pair for development). The image holds no certificate or key. Plaintext
# needs IWDB_TLS_ENABLED=false and IWDB_SERVER_PLAINTEXT_PUBLIC=true.
# Authentication is on: the first start needs IWDB_AUTH_BOOTSTRAP_PASSWORD
# (the user admin; no default password).

ARG RUST_VERSION=1.99
ARG DEBIAN=trixie

FROM rust:${RUST_VERSION}-${DEBIAN} AS build
ARG FEATURES="rest postgres console otel"
WORKDIR /src
COPY . .
# rust-toolchain.toml is left out (.dockerignore): the image's toolchain
# builds, no rustup download. The protos compile with protox: no protoc.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p iwctl \
    && cargo build --release --locked -p iwdb-server --no-default-features --features "${FEATURES}" \
    && mkdir -p /out \
    && cp target/release/iwdb-server target/release/iwctl /out/

FROM debian:${DEBIAN}-slim
LABEL org.opencontainers.image.title="iwdb-server" \
      org.opencontainers.image.description="Ironweaver DB: a durable graph database over gRPC and REST" \
      org.opencontainers.image.source="https://github.com/p-sodmann/ironweaver-db" \
      org.opencontainers.image.licenses="AGPL-3.0-only"
RUN useradd --system --uid 10001 --home-dir /var/lib/iwdb --shell /usr/sbin/nologin iwdb \
    && mkdir -p /var/lib/iwdb /etc/iwdb \
    && chown iwdb:iwdb /var/lib/iwdb
COPY --from=build /out/iwdb-server /out/iwctl /usr/local/bin/
COPY docker/iwdb.toml /etc/iwdb/iwdb.toml
USER iwdb
VOLUME ["/var/lib/iwdb"]
EXPOSE 7600
# iwdb-server drains running calls on SIGTERM, flushes the WAL and
# checkpoints (ADR 0027); docker stop waits 10 s by default, more than the
# drain of the image's config (8 s)
STOPSIGNAL SIGTERM
# Ready once recovery has finished (step 16b, ADR 0040); the probe asks
# /v1/health/ready over HTTP/1.1, so the image needs no curl. It reads the
# config (and the container's IWDB_* variables) for the port and whether to
# speak TLS (step 15b); it doesn't verify the certificate and sends no
# credentials. A long recovery stays within the start period
HEALTHCHECK --interval=10s --timeout=5s --start-period=60s --retries=3 \
    CMD ["iwdb-server", "--probe", "--config", "/etc/iwdb/iwdb.toml"]
ENTRYPOINT ["iwdb-server"]
CMD ["--config", "/etc/iwdb/iwdb.toml"]
