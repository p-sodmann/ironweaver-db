# syntax=docker/dockerfile:1
#
# The iwdb-server image (step 13a, ADR 0034): iwdb-server and iwctl on Debian
# slim, as a non-root user, with the data directory as a volume.
#
#   docker build -t iwdb .                                # gRPC, REST, Postgres projections, console
#   docker build --build-arg FEATURES="" -t iwdb:grpc .   # gRPC only
#   docker run -p 7600:7600 -v iwdb-data:/var/lib/iwdb iwdb
#
# FEATURES are iwdb-server's cargo features (rest, postgres, console), space
# separated. The console is compiled in but off: IWDB_CONSOLE_ENABLED=true and
# IWDB_CONSOLE_PUBLIC=true turn it on (the container listens on 0.0.0.0).
# No TLS and no authentication until step 15: publish the port on localhost
# or a private network only.

ARG RUST_VERSION=1.99
ARG DEBIAN=trixie

FROM rust:${RUST_VERSION}-${DEBIAN} AS build
ARG FEATURES="rest postgres console"
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
      org.opencontainers.image.licenses="MIT"
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
# /v1/health/ready over HTTP/1.1, so the image needs no curl. A long
# recovery stays within the start period
HEALTHCHECK --interval=10s --timeout=5s --start-period=60s --retries=3 \
    CMD ["iwdb-server", "--probe", "127.0.0.1:7600"]
ENTRYPOINT ["iwdb-server"]
CMD ["--config", "/etc/iwdb/iwdb.toml"]
