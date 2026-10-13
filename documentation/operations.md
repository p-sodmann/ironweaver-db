# Operations guide

How to install, configure, run, watch, back up and upgrade an `iwdb-server`, without reading the code. Each section is a short procedure: what to run, what you should see, what to do when it fails. The reference pages hold the full tables, and this guide links to them instead of copying them:

- [api/config.md](api/config.md): every setting, its variable and its default
- [api/metrics.md](api/metrics.md): every metric
- [api/errors.md](api/errors.md): every error code
- [iwctl.md](iwctl.md): every `iwctl` command
- [guarantees.md](guarantees.md): what is durable when, and what each failure does

**About the examples.** Shell examples are POSIX `sh`, run from a working directory that holds the config file `iwdb.toml` of [Configure](#configure) and the server's certificate, key and CA in `tls/` (`server.pem`, `server.key`, `ca.pem`). `$ADMIN_PASSWORD` is the first admin's password. A test keeps the guide honest:

- `crates/iwdb-server/tests/operations.rs` checks the settings, `IWDB_*` variables, TOML blocks and metrics;
- a unit test in `iwctl` (`crates/iwctl/src/guide.rs`) parses every `iwctl` command line;
- on Linux and macOS, the blocks fenced as `sh run` run in order against a real server, and must exit 0. A `sh run=server` block starts the server it shows and waits until it is ready. A `sh run=stopped` block stops it first (SIGTERM). The test replaces port 7600 with a free one. Docker, Kubernetes and Windows examples aren't run there; CI's docker job runs the image.

## Contents

1. [Install](#install)
2. [Configure](#configure)
3. [First start and authentication](#first-start-and-authentication)
4. [TLS](#tls)
5. [Probes](#probes)
6. [Shutdown](#shutdown)
7. [Logs and the audit log](#logs-and-the-audit-log)
8. [Metrics and alerts](#metrics-and-alerts)
9. [The status views and iwctl](#the-status-views-and-iwctl)
10. [Backups and restore](#backups-and-restore)
11. [The memory limit](#the-memory-limit)
12. [Resource limits per user and namespace](#resource-limits-per-user-and-namespace)
13. [Managed jobs](#managed-jobs)
14. [Traces](#traces)
15. [Upgrades](#upgrades)
16. [Windows](#windows)
17. [Troubleshooting](#troubleshooting)

## Install

Releases are published on GitHub only: a tag and a [GitHub Release](https://github.com/p-sodmann/ironweaver-db/releases) with the files below. There is no PyPI package, crate or image registry. What a version number promises is [ADR 0060](adr/0060-versioning-policy.md); what a release holds, in detail, is [releasing.md](releasing.md#the-files-of-a-release).

### From a release

Download the files of the release (`gh release download vX.Y.Z -R p-sodmann/ironweaver-db`, or from the release's page) and check them: `sha256sum -c --ignore-missing SHA256SUMS` (macOS: `shasum -a 256 -c --ignore-missing SHA256SUMS`).

- **The server and `iwctl`**: `ironweaver-db-X.Y.Z-<target>.tar.gz` (`.zip` on Windows) holds both binaries, built with every feature (gRPC, REST, Postgres projections, console, traces). Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (glibc 2.28 or later), `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`. Unpack it and put both on the `PATH`. They aren't signed: on macOS, a file downloaded with a browser needs `xattr -d com.apple.quarantine iwdb-server iwctl`.
- **The image**: `docker load -i ironweaver-db-X.Y.Z-docker-linux-amd64.tar.gz` (or `-arm64`) gives the image `iwdb-server:X.Y.Z`; run it as below, with that name for `iwdb`.
- **The Python package**: see [The Python package](#the-python-package).

```sh
tar -xzf ironweaver-db-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz
ironweaver-db-X.Y.Z-x86_64-unknown-linux-gnu/iwdb-server --version   # iwdb-server X.Y.Z (grpc, rest, postgres, console, otel)
```

### The Docker image (Linux)

The image ([ADR 0034](adr/0034-docker-image-and-server-features.md)) has `iwdb-server` and `iwctl`, runs as a non-root user (uid 10001), keeps the data directory in the volume `/var/lib/iwdb`, and reads `/etc/iwdb/iwdb.toml` ([docker/iwdb.toml](../docker/iwdb.toml): every interface, port 7600, `fsync = "always"`, an 8 s drain). It has every feature: gRPC, REST, Postgres projections, the console and traces. Load it from a release ([above](#from-a-release)) and tag it `iwdb` (`docker tag iwdb-server:X.Y.Z iwdb`), or build it:

```sh
docker build -t iwdb .                                # or: docker build --build-arg FEATURES="" -t iwdb:grpc .
sh docker/dev-cert.sh                                 # a development CA and certificate in docker/tls
docker run -d --name iwdb -p 127.0.0.1:7600:7600 -v iwdb-data:/var/lib/iwdb \
  -v "$PWD/docker/tls:/etc/iwdb/tls:ro" -e IWDB_AUTH_BOOTSTRAP_PASSWORD="$ADMIN_PASSWORD" iwdb
docker run --rm iwdb --version                        # iwdb-server 0.1.0 (grpc, rest, postgres, console, otel)
```

- **TLS is the default.** The image holds no certificate or key. Mount yours at `/etc/iwdb/tls/server.pem` and `server.key`. Without them the container exits with code 2 and the log says `can't read the TLS certificate /etc/iwdb/tls/server.pem`.
- **The development certificate.** `docker/dev-cert.sh` makes a CA and a certificate for `localhost`, `127.0.0.1`, `::1` and `iwdb`. It is only for development: the CA's key lies next to it, and nothing renews it. Clients trust `docker/tls/ca.pem` (`iwctl --tls-ca`, `curl --cacert`, `iwdb.connect(ca=...)`).
- **Plaintext needs both flags**: `-e IWDB_TLS_ENABLED=false -e IWDB_SERVER_PLAINTEXT_PUBLIC=true`. With only the first, the server refuses a non-loopback address (exit 2, the message names `IWDB_SERVER_PLAINTEXT_PUBLIC`). Use it only behind a TLS-terminating proxy on a private network: passwords, tokens and data cross the network in clear.
- **Compose.** `IWDB_ADMIN_PASSWORD=... docker compose up --build` runs the image with the development certificate and the console on ([compose.yaml](../compose.yaml)).

### From source

Rust 1.99 or later ([ADR 0029](adr/0029-rust-1.99-and-edition-2024.md)), on Linux, macOS or Windows:

```sh
cargo build --release -p iwdb-server                  # gRPC, REST and Postgres projections (the defaults)
cargo build --release -p iwdb-server --features "console otel"            # also the console and traces
cargo build --release -p iwdb-server --no-default-features                # gRPC only
cargo build --release -p iwctl
```

The binaries are `target/release/iwdb-server` and `target/release/iwctl`. `--version` lists the build's features; a feature the config needs but the build lacks (`[console] enabled`, `[tracing] enabled`) stops the server at start.

```sh run
iwdb-server --version
iwctl --version
```

### The Python package

One package, `ironweaver-db`, is both the embedded store (a data directory in your process) and the remote client of a server (`iwdb.connect`); it doesn't include the server ([python-api.md](python-api.md)). It isn't on PyPI: install a wheel of a release (abi3, CPython 3.9 and later, for Linux x86_64 and aarch64, macOS x86_64 and arm64, Windows x86_64), by its URL or from the downloaded files, where pip picks the one for your platform:

```sh
pip install https://github.com/p-sodmann/ironweaver-db/releases/download/vX.Y.Z/<the wheel's file name>
pip install --no-index --find-links <download directory> ironweaver-db
```

From source: `maturin develop -m crates/iwdb-python/Cargo.toml` in a virtualenv, or `pip install` the release's sdist (`ironweaver_db-X.Y.Z.tar.gz`), which needs Rust 1.99 or later.

### Windows

Build from source as above. The server runs as a console process: there is no Windows service, installer or Docker image ([ADR 0058](adr/0058-windows.md)). Start it from a console and keep the console open:

```powershell
.\iwdb-server.exe --config C:\iwdb\iwdb.toml
```

Stop it with Ctrl-C or Ctrl-Break ([Shutdown](#shutdown)). See [Windows](#windows) for what else differs.

## Configure

The server reads a TOML file (`--config <file>`), `IWDB_*` environment variables, or both ([ADR 0039](adr/0039-configuration-and-environment-overrides.md)). **A variable wins over the file.** A setting's variable is `IWDB_` and its path in upper case, with `.` as `_`: `store.fsync` is `IWDB_STORE_FSYNC`. Only `data_dir` is required, plus the TLS certificate and key unless TLS is off. Every setting is in [api/config.md](api/config.md).

This guide's config file:

```toml file=iwdb.toml
data_dir = "data"
listen = "127.0.0.1:7600"

[store]
fsync = "always"
archive = "archive"

[server]
drain_timeout_secs = 20

[tls]
cert = "tls/server.pem"
key = "tls/server.key"

[backup]
dir = "backups"

[audit]
dir = "audit"
```

- **Relative paths** (`data_dir`, `store.archive`, `tls.cert`, `tls.key`, `tls.client_ca`, `audit.dir`, `backup.dir`) are relative to the file's directory when they come from the file, and to the working directory when they come from a variable. In production, prefer absolute paths.
- **The backup directory must exist** and not be inside the data directory; the data directory, the archive and the audit directory are created if missing.
- **Typos are errors.** A variable that starts like a section (`IWDB_STORE_`, `IWDB_TLS_`, ...) but names no setting stops the server.

Check a configuration before you start a server on it. `--check-config` prints the effective settings, each with where it came from (`# file`, `# default`, or the variable), and exits 0; a bad one exits 2 and lists every problem:

```sh run
mkdir -p backups
iwdb-server --check-config --config iwdb.toml
IWDB_STORE_FSYNC=group iwdb-server --check-config --config iwdb.toml | grep 'IWDB_STORE_FSYNC'
```

```
iwdb-server: invalid configuration (iwdb.toml):
  - IWDB_STORE_FSYNCX: no such setting (the settings: documentation/api/config.md)
```

**Paths on Windows.** In TOML a backslash starts an escape inside `"..."`. Write a literal string `'C:\iwdb\data'`, or use forward slashes:

```toml
data_dir = 'C:\iwdb\data'

[store]
archive = "C:/iwdb/archive"

[tls]
cert = 'C:\iwdb\tls\server.pem'
key = 'C:\iwdb\tls\server.key'
```

A data directory on a network share (a UNC path to another machine) or on FAT/exFAT isn't supported.

## First start and authentication

Authentication is on by default, and there is no default password ([ADR 0047](adr/0047-auth-bootstrap-and-configuration.md)). A store without users refuses to start, and the message names the two ways to make the first admin:

- **On the first start**: set `IWDB_AUTH_BOOTSTRAP_PASSWORD` (and `IWDB_AUTH_BOOTSTRAP_USER`, default `admin`). A start that finds no users creates that admin. These are variables only, never config keys, and `--check-config` doesn't print them. Unset them after the first start; a start that finds users ignores them with a warning.
- **Offline**, with the server stopped: `iwctl user create data admin --admin` (it prompts for the password, at least 8 bytes).

```sh run=server
IWDB_AUTH_BOOTSTRAP_PASSWORD="$ADMIN_PASSWORD" iwdb-server --config iwdb.toml
```

You should see, on stderr, `listening; opening the store (recovery)`, then `recovery finished`, `created the first admin from IWDB_AUTH_BOOTSTRAP_PASSWORD`, and `serving data on 127.0.0.1:7600`: the server is ready.

**Tokens.** `iwctl` (and any client) logs in with a password (`--user`) or sends an API token (`--token`, or `IWDB_TOKEN`). Make a token once and keep it like a password: it is printed once, on the last line. On a terminal `--user` prompts for the password; in a script it is read from stdin.

```sh run
export S="--server https://127.0.0.1:7600 --tls-ca tls/ca.pem"
IWDB_TOKEN=$(echo "$ADMIN_PASSWORD" | iwctl token create $S --user admin admin ops | tail -n 1)
export IWDB_TOKEN
iwctl token list $S admin
```

**Users and roles.** Roles are per namespace: `read`, `write` (and commits), `admin` (and catalog changes, dropping the namespace). A server-wide admin manages users and runs the admin commands. Every change is audited ([Logs](#logs-and-the-audit-log)).

```sh run
echo 'a-password-for-ann' | iwctl user create $S ann
iwctl create-namespace $S social
iwctl user grant $S ann social write
iwctl user list $S
```

Others: `iwctl user passwd $S ann` (ends ann's sessions), `iwctl user admin $S ann on`, `iwctl user revoke $S ann social`, `iwctl token revoke $S ann cli`, `iwctl user delete $S ann`. The same commands work offline on the data directory, without credentials, while the server is stopped: whoever can open the directory owns the store ([iwctl.md](iwctl.md#users-and-tokens)).

**Client certificates (mTLS).** With `tls.client_ca`, a client certificate signed by that CA authenticates as the user its common name (CN) names; the user must exist. With `tls.client_auth = "required"`, every request but health and the console's pages needs one ([ADR 0048](adr/0048-tls-and-mtls.md)).

```toml
data_dir = "data"

[tls]
cert = "tls/server.pem"
key = "tls/server.key"
client_ca = "tls/client-ca.pem"
client_auth = "required"
```

```sh
sh docker/dev-cert.sh --client admin                  # docker/tls/client-admin.pem and .key
iwctl user list --server https://127.0.0.1:7600 --tls-ca docker/tls/ca.pem \
  --tls-cert docker/tls/client-admin.pem --tls-key docker/tls/client-admin.key
```

**Authentication off** (`[auth] enabled = false`) makes every caller a server-wide admin. Use it only on loopback, for development.

## TLS

The server speaks TLS on its one port (gRPC and REST; ALPN `h2` and `http/1.1`). `tls.cert` is the chain in PEM, the server's certificate first; `tls.key` its key (PKCS#8, PKCS#1 or SEC1). The modes (TLS, plaintext on loopback, plaintext behind a proxy, optional and required client certificates) are in [api/config.md](api/config.md#tls-and-mtls).

The server doesn't manage certificates: it doesn't request, renew or watch them. Get them from your CA or an ACME client, and renew them before they expire.

**Replacing a certificate.**

1. Write the new certificate and key over the old files (or point `tls.cert` and `tls.key` at new ones and restart).
2. On Linux and macOS, send SIGHUP: `kill -HUP <pid>`, or `docker kill --signal HUP iwdb`. The log says `reloaded the TLS certificate`. New connections get the new certificate; open ones keep theirs.
3. If the log says the reload failed (at `error`: a missing file, a key that doesn't match), the old certificate stays in use. Fix the files and send SIGHUP again.
4. **On Windows there is no SIGHUP:** restart the server. The graceful drain keeps the gap short.

Private keys are never logged, printed by `--check-config` or quoted in an error.

## Probes

| | Answers | Use it for |
|---|---|---|
| `GET /v1/health/live` | 200 whenever the server answers | liveness |
| `GET /v1/health/ready` | 200 when ready, 503 while it recovers or drains | readiness, load balancers |
| `grpc.health.v1.Health/Check` | `SERVING` / `NOT_SERVING` | gRPC probes, `grpc_health_probe` |
| `iwdb-server --probe` | exit 0 ready, 1 not, 2 bad config | the Docker `HEALTHCHECK`, scripts |

**Ready means recovery has finished** and shutdown hasn't begun ([ADR 0040](adr/0040-health-and-readiness.md)). The server binds its port first, answers health while it replays the WAL, and turns ready only once every committed record is visible. Until then database calls fail with `unavailable` ("the server is recovering"). Health needs no credentials and no client certificate.

```sh run
iwdb-server --probe --config iwdb.toml
curl -fsS --cacert tls/ca.pem https://127.0.0.1:7600/v1/health/ready
curl -fsS --cacert tls/ca.pem https://127.0.0.1:7600/v1/health/live
```

`--probe --config` reads the config (file and variables) for the port and whether to speak TLS; `--probe https://host:port` asks that address. The probe doesn't verify the certificate and sends no credentials.

**Docker.** The image's `HEALTHCHECK` runs `iwdb-server --probe --config /etc/iwdb/iwdb.toml` every 10 s, with a 60 s start period for a long recovery. `docker inspect -f '{{.State.Health.Status}}' iwdb` shows `healthy`.

**Kubernetes.** A minimal probe setup (the only orchestrator recipe here). The `httpGet` probes speak HTTPS without verifying the certificate. The startup probe gives recovery up to 10 minutes. Set the grace period above `server.unready_delay_ms` + `server.drain_timeout_secs` + the time of the final checkpoint:

```yaml
spec:
  terminationGracePeriodSeconds: 60
  containers:
    - name: iwdb
      image: iwdb:latest
      ports: [{ containerPort: 7600 }]
      env:
        - { name: IWDB_SERVER_UNREADY_DELAY_MS, value: "5000" }
        - { name: IWDB_SERVER_DRAIN_TIMEOUT_SECS, value: "30" }
      startupProbe:
        httpGet: { path: /v1/health/ready, port: 7600, scheme: HTTPS }
        periodSeconds: 5
        failureThreshold: 120
      readinessProbe:
        httpGet: { path: /v1/health/ready, port: 7600, scheme: HTTPS }
        periodSeconds: 5
      livenessProbe:
        httpGet: { path: /v1/health/live, port: 7600, scheme: HTTPS }
        periodSeconds: 10
        failureThreshold: 6
```

Run one replica per data directory: the data directory is locked by one process ([Troubleshooting](#troubleshooting)).

## Shutdown

SIGTERM or SIGINT (on Windows: Ctrl-C, Ctrl-Break, closing the console, a system shutdown) shuts down gracefully ([ADR 0027](adr/0027-graceful-shutdown.md)):

1. Readiness turns off (`shutting down: no longer ready`). With `server.unready_delay_ms`, the server keeps serving that long, so load balancers stop sending first. Set it to a few seconds behind a load balancer.
2. New connections are refused, and running calls may finish for up to `server.drain_timeout_secs` (default 30). Queued and running jobs are cancelled.
3. Calls still running at the timeout are cut off. A commit already accepted still completes, but its client doesn't learn the outcome and retries with its idempotency key.
4. The WAL of every namespace is flushed, whatever the fsync policy.
5. With `store.checkpoint_on_shutdown` (the default), every namespace is checkpointed, so the next start replays nothing.
6. The log says `closed data`, and the process exits 0. It exits 1 if the flush or the checkpoint failed; nothing acknowledged is lost by a failed checkpoint.

A second signal during the drain cuts off the calls still running at once.

**Allow enough time.** Your supervisor must wait longer than `unready_delay_ms` + `drain_timeout_secs` + the final checkpoint, or it kills the server first. A kill is a crash: recovery replays the WAL, and nothing acknowledged is lost (under `group`, see [guarantees.md](guarantees.md#durability-of-commits-step-4)). The image's config drains for 8 s, under `docker stop`'s default 10 s; with a longer drain, use `docker stop -t <seconds>`.

**On Windows**, closing the console window or a system shutdown gives the process only about 5 s before Windows ends it. Stop the server with Ctrl-C or Ctrl-Break instead.

## Logs and the audit log

**Logs** go to stderr, one event per line ([ADR 0042](adr/0042-structured-logs.md)). `log.format = "auto"` (the default) writes JSON lines unless stderr is a terminal: each line has `timestamp` (UTC), `level`, `target`, `message` and the event's fields. `log.level` is a filter: a level (`info`, the default), or directives such as `warn,iwdb_storage=debug`. Collect stderr with your container runtime or service manager, and set retention there (Docker's log driver `max-size` and `max-file`, journald's `SystemMaxUse`).

Events worth knowing ([api/config.md](api/config.md#logs) lists them):

| Event | Means |
|---|---|
| `listening; opening the store (recovery)` | started; health answers, the store is recovering |
| `recovered` (per namespace), `recovery finished` (`took_ms`) | recovery's result: `replayed`, `seq`, `torn_tail` |
| `serving <dir> on <address>` | ready |
| `shutting down; running calls may finish`, `closed <dir>` | a graceful stop and its end |
| `iwdb-server failed` (`error`) | the server stopped on an error (exit 1) |
| level `error` with `code` (`internal`, `corrupt`, `io`) | a request failed on the server's side |
| memory state changes (`warn`, `error`) | [The memory limit](#the-memory-limit) |

Requests aren't logged, except those that fail on the server's side. **The log tail** keeps the last `log.tail_events` events (1000) in memory for server admins and the console: `GET /v1/log` (`GetLog`).

```sh run
curl -fsS --cacert tls/ca.pem "https://127.0.0.1:7600/v1/log?limit=5" -H "authorization: Bearer $IWDB_TOKEN"
```

**The audit log** ([ADR 0049](adr/0049-audit-log.md)) has one entry per login (and failed login), logout, user, grant, token, namespace and catalog change, cancelled request, admin write (checkpoint, backup, verify, prune), started or cancelled job, and refused request. Entries never hold a password, a token, an error message or a value of the data.

- **In the log**: events of target `iwdb::audit` at `info`. `info,iwdb::audit=off` keeps them out of stderr.
- **In files**, with `audit.dir`: `audit-YYYY-MM-DD.jsonl`, one file per UTC day. `audit.retention_days` (30) keeps that many days, today included; older files are deleted at start and at each new day. `0` keeps them all.
- **Permissions.** On Unix the files are made with mode 0600 and the directory with 0700. On Windows they get the directory's inherited ACL: put `audit.dir` where only the server's account and administrators can read.
- **Best effort**: written after the outcome, without fsync. A crash can lose the last entries, never the changes.

```sh run
ls audit
```

## Metrics and alerts

The server exports Prometheus metrics at `GET /metrics` (also `GetMetrics` and `/v1/metrics`) on its one port. It pulls nothing and pushes nothing ([ADR 0050](adr/0050-metrics-without-a-library.md)). Every metric is in [api/metrics.md](api/metrics.md), with a scrape config.

**A monitoring user.** `/metrics` needs a token. A user without grants sees the server-wide series; grant it `read` on a namespace for that namespace's series:

```sh run
echo 'a-password-for-prometheus' | iwctl user create $S prometheus
iwctl user grant $S prometheus social read
iwctl token create $S prometheus scrape | tail -n 1 > prometheus.token
curl -fsS --cacert tls/ca.pem https://127.0.0.1:7600/metrics -H "authorization: Bearer $(cat prometheus.token)" | grep '^iwdb_ready'
```

Give Prometheus the token file (`authorization: { credentials_file: ... }`) and the CA (`tls_config: { ca_file: ... }`), as in metrics.md.

### Suggested alert rules

These rules use only documented metrics and Prometheus' own `up`. The thresholds are starting points: each rule says why it is set where it is, and what to do when it fires. The label `job="iwdb"` is your scrape job's name.

```yaml
groups:
  - name: iwdb
    rules:
      # Can't scrape: the server is down, or the token or TLS is wrong
      - alert: IwdbDown
        expr: up{job="iwdb"} == 0
        for: 2m
        labels: { severity: page }

      # A namespace refuses writes after a failed WAL write or fsync
      - alert: IwdbNamespaceReadOnly
        expr: iwdb_namespace_read_only == 1
        labels: { severity: page }

      # Writes are refused: memory above refuse_writes_at of the limit
      - alert: IwdbRefusingWrites
        expr: iwdb_memory_state == 2
        labels: { severity: page }

      - alert: IwdbMemoryHigh
        expr: iwdb_memory_state >= 1
        for: 10m
        labels: { severity: ticket }

      # Room for the next checkpoint (the series holds the 2 kept checkpoints)
      - alert: IwdbDiskLow
        expr: iwdb_disk_free_bytes < on(instance) max by (instance) (iwdb_checkpoint_bytes)
        for: 5m
        labels: { severity: page }

      - alert: IwdbDiskFillingUp
        expr: predict_linear(iwdb_disk_free_bytes[6h], 24 * 3600) < 0
        for: 30m
        labels: { severity: ticket }

      - alert: IwdbCheckpointFailed
        expr: iwdb_checkpoint_failed == 1
        for: 15m
        labels: { severity: ticket }

      # Checkpoints run every 5 minutes while there are commits
      - alert: IwdbCheckpointsBehind
        expr: (time() - iwdb_last_checkpoint_timestamp_seconds > 3600) and iwdb_checkpoint_lag_commits > 0 unless on(instance) iwdb_backup_running > 0
        for: 15m
        labels: { severity: ticket }

      - alert: IwdbWalLarge
        expr: iwdb_wal_bytes > 1073741824
        for: 30m
        labels: { severity: ticket }

      - alert: IwdbSlowFsync
        expr: histogram_quantile(0.99, sum by (instance, le) (rate(iwdb_wal_fsync_duration_seconds_bucket[5m]))) > 0.1
        for: 10m
        labels: { severity: ticket }

      - alert: IwdbRejectingRequests
        expr: sum by (instance) (rate(iwdb_requests_total{code="unavailable"}[5m])) / sum by (instance) (rate(iwdb_requests_total[5m])) > 0.01
        for: 10m
        labels: { severity: ticket }

      - alert: IwdbTimeouts
        expr: sum by (instance) (rate(iwdb_requests_total{code="timeout"}[5m])) / sum by (instance) (rate(iwdb_requests_total[5m])) > 0.05
        for: 10m
        labels: { severity: ticket }

      - alert: IwdbBackupFailed
        expr: increase(iwdb_backups_total{outcome="failed"}[1d]) > 0
        labels: { severity: ticket }

      - alert: IwdbNamespaceAtMemoryLimit
        expr: increase(iwdb_namespace_limit_refusals_total{limit="memory"}[10m]) > 0
        labels: { severity: ticket }

      - alert: IwdbUserThrottled
        expr: sum by (instance, user) (rate(iwdb_limit_refusals_total[10m])) > 1
        for: 30m
        labels: { severity: info }

      - alert: IwdbTraceExportFailing
        expr: rate(iwdb_trace_spans_dropped_total{reason="export_failed"}[10m]) > 0
        for: 30m
        labels: { severity: info }
```

| Rule | Why this threshold | What to do |
|---|---|---|
| `IwdbDown` | 2 minutes rides out a restart | Check the process and its log (`iwdb-server failed`); check the token and CA in the scrape config. |
| `IwdbNamespaceReadOnly` | never normal: writes fail until a restart | [Troubleshooting: read-only](#a-namespace-is-read-only). |
| `IwdbRefusingWrites` | clients see `resource_exhausted` now | [The memory limit](#the-memory-limit). |
| `IwdbMemoryHigh` | above `memory.warn_at` (80 %) for 10 minutes, not a passing analysis | Plan: raise the limit, or find what grows (`iwdb_memory_used_bytes` by `part`). |
| `IwdbDiskLow` | `iwdb_checkpoint_bytes` holds the 2 kept checkpoints; a new one is written before the oldest is removed, so less free space than that means the next checkpoint may fail, and a full disk makes WAL writes fail (read-only) | Free space or grow the volume now. Old backups on the same disk are the first candidates; never delete files in the data directory. |
| `IwdbDiskFillingUp` | the 6-hour trend reaches 0 within a day | Find the growth: the WAL (`iwdb_wal_bytes`), the archive, backups, logs. |
| `IwdbCheckpointFailed` | a failed checkpoint is retried at the next trigger (5 minutes); 15 minutes means it keeps failing | Read the error: `iwctl status $S` (`checkpoint failed: ...`) or the log. [Troubleshooting](#checkpoints-are-disabled). |
| `IwdbCheckpointsBehind` | with commits, a checkpoint runs at least every 5 minutes; an hour without one (and no backup holding it back) means something blocks it | As for `IwdbCheckpointFailed`. Recovery after a crash replays `iwdb_checkpoint_lag_commits` commits meanwhile. |
| `IwdbWalLarge` | the WAL is kept from the oldest kept checkpoint, and a checkpoint runs every 256 MiB of WAL, so it normally stays below about 600 MiB per namespace | Checkpoints aren't cutting it: they fail, a long backup holds them, the archive fails, or `store.retain_records`/`retain_age_secs` keep it on purpose (then raise the threshold). |
| `IwdbSlowFsync` | under `fsync = "always"` every commit waits for one fsync; a healthy SSD takes milliseconds | The disk is saturated or failing: check its metrics and the system log. Commit latency follows (`iwdb_commit_duration_seconds`). |
| `IwdbRejectingRequests` | more than 1 % refused: the queue (`server.queue`) is full, or the server drains | Add `server.workers` or capacity, or find slow requests (`iwctl requests $S`). |
| `IwdbTimeouts` | more than 5 % of requests reach their timeout | Find the slow reads (`iwctl requests $S`, `iwdb_request_duration_seconds` by operation); cancel runaway ones; check limits. |
| `IwdbBackupFailed` | any failed backup | Read the `iwctl backup` output or the log; a failed backup is removed. Also alert on `time() - iwdb_last_backup_timestamp_seconds` above your backup interval (no sample until the first backup since the server started). |
| `IwdbNamespaceAtMemoryLimit` | the namespace's writes are refused | [Resource limits](#resource-limits-per-user-and-namespace): delete or raise its `max_memory_bytes`. |
| `IwdbUserThrottled` | more than a refusal a second for half an hour is more than bursts | Ask the user's owner: raise its limits if its work needs them, or leave them if it misbehaves. |
| `IwdbTraceExportFailing` | spans are dropped, never waited for; requests are unaffected | Check the collector. `queue_full` drops instead mean too many spans: lower `tracing.sample_ratio`. |

## The status views and iwctl

`iwctl --server <endpoint>` runs the admin commands against a running server ([iwctl.md](iwctl.md#against-a-running-server), [ADR 0055](adr/0055-admin-writes-and-iwctl-against-a-server.md)). Each is one call of the server's API, so the REST and gRPC routes do the same. The admin writes need a server-wide admin. Exit codes: 0 done, 1 damage found, 2 usage, 3 the server is unavailable, 4 any other error.

```sh run
iwctl status $S
iwctl namespaces $S
iwctl create-index $S name -n social
iwctl add-constraint $S unique Person name -n social
iwctl indexes $S -n social
iwctl requests $S
```

`status` shows the version, start time, readiness, fsync policy, memory against its limit, disk use and free space, request counts, jobs, and every namespace you have a role on. A namespace line says `(read-only: <cause>)` and `checkpoint failed: <cause>` when that applies. `--json` gives the same as JSON.

| Task | Command |
|---|---|
| Namespaces | `iwctl namespaces $S`, `iwctl create-namespace $S <name>`, `iwctl drop-namespace $S <name>` |
| Indexes and constraints | `iwctl indexes $S -n <ns>`, `create-index`, `drop-index`, `add-constraint`, `drop-constraint` (an index builds online; a constraint validates the data first) |
| Running requests | `iwctl requests $S` (oldest first; `requests $S <user>` for one user) |
| Cancel a read or a job | `iwctl cancel $S <id>`. Commits and admin writes can't be cancelled |
| Checkpoint now | `iwctl checkpoint $S` (or `-n <ns>`); it waits for a running backup |

Change-stream readers and their lag (`GET /v1/consumers`), the metrics (`/metrics`) and the log tail (`/v1/log`) are read over REST or gRPC, or in the console (`[console] enabled = true`, a build with `console`, [ADR 0041](adr/0041-console-served-by-the-server.md)); `iwctl` has no command for them yet.

## Backups and restore

Backups, archiving, verify and restore are described in [guarantees.md](guarantees.md#backup-archiving-and-restore-step-7) and [ADR 0009](adr/0009-backup-archive-restore.md). In short:

- **A backup** holds every commit up to the moment it starts, and restores to any seq from its oldest checkpoint to that one. It is complete or refused.
- **The WAL archive** (`store.archive`) receives each WAL segment, durably, before a checkpoint removes it. A backup plus the archive restores to any later seq that reached the archive: segments arrive as checkpoints remove them, so the newest commits are in `wal/` only until then.
- **Never copy a live data directory file by file.** A copy taken while the server writes is not a backup: it can miss files or catch them half-written. On Windows, the `LOCK` file can't even be read. Use `iwctl backup`.

### Online backup

Set `backup.dir` (it must exist). Backups go there by name, never anywhere else, and never over an existing name:

```sh run
iwctl backup $S nightly-1
iwctl verify $S backup nightly-1
iwctl verify $S
```

You should see `backed up 3 namespaces into .../backups/nightly-1`, one line per namespace, then `verify ... (backup): ok`. Run it from cron or a timer with a dated name (`nightly-$(date +%F)`), and copy finished backups off the machine: a backup directory is a closed set of files, safe to copy once `iwctl backup` has returned.

- **Throttling.** `backup.max_bytes_per_second` (or `--max-bytes-per-second` per backup) limits the copy's speed. Checkpoints wait for the whole copy, so a slow backup holds them back and the WAL grows meanwhile: at 10 MiB/s, a 50 GiB store holds them back about 85 minutes. `iwdb_backup_running` is 1 meanwhile, and `rate(iwdb_backup_bytes_total[1m])` is the copy's speed.
- **Verify** checks every file and invariant and changes nothing: `iwctl verify $S` (the running store), `verify $S archive`, `verify $S backup <name>`. It is refused while the server refuses writes for memory. Exit 1 means damage.
- **A failed backup** is removed, and `iwdb_backups_total{outcome="failed"}` counts it.

### Pruning the archive

The archive grows until you prune it. Prune before the oldest backup you want to restore from:

```sh run
iwctl archive prune $S --before nightly-1 --dry-run
iwctl archive prune $S --before nightly-1
```

It removes what no restore from that backup (or a later one) needs. Delete the older backups themselves afterwards: they are directories in `backup.dir`.

### Restore and point-in-time recovery

Restore is offline: it writes a new data directory and never touches a running store. Run it on the server's host:

```sh run=stopped
iwctl restore restored --backup backups/nightly-1 --archive archive
iwctl verify restored
iwctl status restored
```

- **To the latest seq** the backup and the archive reach (above); **to a seq**: `--seq 42 -n social` (one namespace); **to a time**: `--time 2026-10-01T12:00:00Z`, the last commit at or before it. A time before the backup's oldest commit is refused (exit 4, "no commit at or before ...").
- **A restored store has a new history.** It needs a new, empty archive: the old one belongs to the old history and is refused.
- Without `--no-verify`, the restore verifies its result.

Then start a server on the restored directory, with a new archive. Variables win over the file, so the same config file works:

```sh run=server
IWDB_DATA_DIR=restored IWDB_STORE_ARCHIVE=archive-restored iwdb-server --config iwdb.toml
```

```sh run
iwdb-server --probe --config iwdb.toml
iwctl namespaces $S
```

To replace the original directory instead, stop the server, move the old directory away, move the restored one into its place, and point `store.archive` at a new directory.

## The memory limit

The server counts the memory of its graphs (with their indexes and attributes), the checkpointers' copies, and the analytics projections, index builds and stored job results ([ADR 0054](adr/0054-the-memory-limit.md), [guarantees.md](guarantees.md#the-memory-limit-step-16d)). The limit is `memory.limit_bytes`, or, unset, the cgroup's on Linux (a container's memory limit); elsewhere none.

| Memory used | State | What happens |
|---|---|---|
| below `memory.warn_at` (80 %) | normal (`iwdb_memory_state` 0) | – |
| from `warn_at` | warn (1) | a `warn` log event; `iwctl status` shows `(warn)` |
| from `memory.refuse_writes_at` (90 %) | refusing writes (2) | an `error` log event. Commits, index and constraint creation, namespace creation and imports fail with `resource_exhausted` before they are logged. Deletes, drops, logins, reads and jobs go on |

Each state is left 5 % of the limit below its line, so a store at the line doesn't flip. The 10 % above the refusal line is for what isn't counted (buffers, the allocator, the runtime).

**When writes are refused:**

1. See what is counted: `iwctl status $S` (the memory line), or `iwdb_memory_used_bytes` by `part`.
2. If `working` is large: cancel the analyses and jobs that hold projections (`iwctl requests $S`, `iwctl cancel $S <id>`, `iwctl jobs cancel $S <id>`). Stored job results count too; they expire after `jobs.retention_secs`, and `jobs.result_bytes` caps them.
3. If `checkpoint` is large, a checkpoint is running; it shrinks when it ends.
4. If `graph` is large, the data has outgrown the limit. Delete data or drop an unused index or namespace (all allowed while writes are refused), or raise the limit: `memory.limit_bytes`, or the container's limit, then restart.
5. Writes resume on their own once memory is 5 % below the line; there is nothing to reset. Clients retry `resource_exhausted` with backoff.

An embedded store has no limit unless its `StoreOptions` set one.

## Resource limits per user and namespace

Limits keep one user or namespace from starving the others ([ADR 0059](adr/0059-resource-limits-per-client-and-namespace.md), [guarantees.md](guarantees.md#resource-limits-step-15d)). Nothing is limited until you set something. A call over a limit is refused at once with `resource_exhausted`, before it takes a worker or a lock, and the message names the limit; clients retry with backoff.

| Limit | Per | What it bounds |
|---|---|---|
| `requests_per_second`, `burst` | user | calls admitted, as a token bucket (`burst`: how many at once after a quiet time) |
| `max_running` | user | the user's calls running at once |
| `max_results`, `max_visited`, `timeout_ms` | user | query budgets: what a read may ask for, below `[limits.max]` |
| `max_running` | namespace | calls on the namespace running at once, every user's together |
| `max_memory_bytes` | namespace | writes that add are refused while its graph memory (`iwdb_namespace_memory_bytes`) is at or above this; deletes, drops and reads go on |

Defaults for users who aren't server admins and namespaces without their own go in the configuration; a value set with `iwctl limits` replaces the default (0: no limit). Admins get only the limits set on them, so you can still act on a loaded server:

```toml
[limits.client]
requests_per_second = 100
burst = 200
max_running = 16
max_visited = 1000000

[limits.namespace]
max_running = 64
```

Set and read them per user and namespace (stored with the users, so they survive restarts and travel with backups):

```sh run
iwctl limits user $S prometheus requests_per_second=5 max_running=2
iwctl limits namespace $S social max_running=32 max_memory_bytes=1073741824
iwctl limits show $S
iwctl user list $S
```

A set replaces all of that user's or namespace's limits; `iwctl limits user $S <name>` with none clears them. Changes apply to the next request.

**Watching them.** `iwdb_limit_refusals_total{user, limit}` counts a user's refusals (`rate`, `running`; the first 64 users get their own series, later ones `(other)`), `iwdb_namespace_limit_refusals_total{namespace, limit}` a namespace's (`running`, `memory`), and `iwdb_namespace_memory_limit_bytes` shows each namespace's memory limit. `iwdb_requests_total{code="resource_exhausted"}` counts them all.

**When a user is refused:** a well-behaved client sees a few refusals under bursts; many mean its limits are too low for its work (raise them) or it misbehaves (leave them: the others are protected). Its own requests: `iwctl requests $S <user>`; cancelling is never limited. **When a namespace is at its memory limit:** delete data or drop an index in it, or raise `max_memory_bytes`; writes resume as soon as it is below, with nothing to reset.

## Managed jobs

An analysis that may take longer than a request's timeout runs as a managed job: started through the API, then followed, cancelled and fetched by id ([ADR 0056](adr/0056-managed-analytics-jobs.md)). `iwctl` doesn't start jobs; applications do (gRPC `StartJob`, REST `POST /v1/namespaces/{ns}/jobs`, the Rust clients).

```sh run
curl -fsS --cacert tls/ca.pem -X POST https://127.0.0.1:7600/v1/namespaces/social/jobs \
  -H "authorization: Bearer $IWDB_TOKEN" -H 'content-type: application/json' -d '{"job": {"pageRank": {}}}'
iwctl jobs list $S
```

| Task | Command |
|---|---|
| List (newest first) with progress | `iwctl jobs list $S` (`pagerank 37/100`: the algorithm's phase, units done, total) |
| One job | `iwctl jobs show $S <id>`: state, progress, error, when it expires |
| Cancel | `iwctl jobs cancel $S <id>` (or `iwctl cancel $S <id>`) |
| Results | `iwctl jobs result $S <id> 0 1000`: a page of rows; the last line names the next command while more are left |

- **Limits.** `jobs.running` jobs run at once, on threads of their own (never the query workers); `jobs.queued` may wait; `jobs.per_user` per user. More fail with `unavailable`. A job runs at most `jobs.timeout_secs` (`timeout`).
- **Retention.** An ended job and its result are kept for `jobs.retention_secs`, at most `jobs.max_finished` jobs and `jobs.result_bytes` of results; then `not_found` (or `expired`).
- **Shutdown ends them.** A drain cancels every queued and running job, and nothing survives a restart: restart when no long job runs, or start it again after.
- **Metrics:** `iwdb_jobs_queued`, `iwdb_jobs_running`, `iwdb_jobs_total{outcome}`, `iwdb_job_result_bytes`.

## Traces

With a build that has the `otel` feature (the Docker image has it) and `tracing.enabled = true`, the server exports a trace of every request over OTLP: the request, its wait for a worker, its execution, and for writes the commit, the WAL append and the fsync ([ADR 0057](adr/0057-traces.md), [guarantees.md](guarantees.md#traces-step-16g)). A build without `otel` refuses to start with tracing on.

```toml
data_dir = "data"

[tls]
cert = "tls/server.pem"
key = "tls/server.key"

[tracing]
enabled = true
endpoint = "http://otel-collector:4317"
protocol = "grpc"
sample_ratio = 0.1
```

- **Sampling.** `tracing.sample_ratio` is the share of new traces kept (1.0 by default). A request with a W3C `traceparent` header follows its caller's decision.
- **A collector that's down costs nothing but the spans.** Spans are dropped, never waited for, so requests don't slow down or fail. `iwdb_trace_spans_dropped_total{reason="export_failed"}` counts the drops, and the log says once when exports start failing and once when they work again.
- **`OTEL_*` variables are not read**; the server warns about them at start. Use `IWDB_TRACING_ENDPOINT`, `IWDB_TRACING_PROTOCOL`, `IWDB_TRACING_HEADERS` (a vendor's key: `authorization=Bearer ...`), `IWDB_TRACING_SERVICE_NAME` and `IWDB_TRACING_SAMPLE_RATIO` ([api/config.md](api/config.md#traces)).
- **Shutdown** exports what is queued, within what is left of the drain.

## Upgrades

**What the formats promise** (design rule 4 of [AGENTS.md](../AGENTS.md)): every on-disk format (the data directory's layout, the WAL, checkpoints, backups, the archive, the catalog) has a version and checksums. A new version reads the previous one (N-1). Compatibility fixtures of every older layout (`crates/iwdb/tests/fixtures/`) are opened by every build in CI. A data directory of an older layout is **upgraded when the new server opens it**: the recovery log says `upgraded_from`. After that, **older versions refuse it** (`UnsupportedLayout`), so going back means restoring a backup taken before the upgrade ([formats/data-dir.md](formats/data-dir.md), [ADR 0003](adr/0003-catalog-storage.md) for the catalog). A store with users can't be opened by a version from before step 15a.

The gRPC and REST API (`v1`) changes only compatibly. Building from source needs the Rust version of [ADR 0029](adr/0029-rust-1.99-and-edition-2024.md) (`rust-version` in `Cargo.toml`).

**The order of operations:**

1. **Back up** and verify: `iwctl backup $S pre-upgrade` (it verifies). Keep it until the new version has run for a while.
2. **Check the new binary's view of your config**: `iwdb-server --check-config --config iwdb.toml` with the new binary. A setting it doesn't know stops it, with the reason.
3. **Stop** the server gracefully ([Shutdown](#shutdown)) and wait for `closed <dir>` and exit 0. The final checkpoint means the new version replays nothing.
4. **Replace** the binary or the image tag.
5. **Start** it, and wait for ready. Read the recovery events (`upgraded_from` if the layout changed).
6. **Verify**: `iwctl status $S` (version, namespaces, seqs as before), `iwctl verify $S`.

If the new version doesn't start or verify fails, stop it, restore `pre-upgrade` into a new directory with `iwctl restore` (of the old version), and start the old version on it.

**Changing the fsync policy** (`store.fsync`) is a config change, not a format change: it takes effect at the next start, and you can change it back at any time. What each policy loses on an OS crash or power loss is in [guarantees.md](guarantees.md#durability-of-commits-step-4): `always` (the default) nothing acknowledged, `group` the commits of the last `store.group_max_delay_ms` or so, `off` anything not synced (for tests only). A process crash loses nothing acknowledged under any policy.

## Windows

Windows is supported since step 16h, with the guarantees of [guarantees.md](guarantees.md#platforms-step-7-step-16h) and these differences ([ADR 0058](adr/0058-windows.md)):

- **A console process.** No Windows service, installer or Docker image. Stop it with Ctrl-C or Ctrl-Break; closing the console window gives it about 5 s.
- **File systems.** NTFS or ReFS only. FAT32, exFAT and network shares (SMB, UNC paths) aren't supported: on a share the directory flush fails, so a store there refuses to open.
- **Antivirus and indexers.** Exclude the data directory, the archive and the backup directory from real-time scanning and the search indexer. They hold files open and slow down renames and deletes. That wasn't tested.
- **Certificates.** No SIGHUP: replace the files and restart the server.
- **Permissions.** Files and directories get the inherited ACL of where they are made, not Unix modes. Put the data directory, the archive, `backup.dir` and `audit.dir` where only the server's account and administrators can read.
- **Paths in TOML.** `'C:\iwdb\data'` (single quotes) or `"C:/iwdb/data"` ([Configure](#configure)).
- **Backups.** `iwctl backup`, never a file copy of the live directory: `LOCK` can't be read while the server runs.
- **Monitoring.** `iwdb_disk_free_bytes` is the space available to the server's account, quotas included.

## Troubleshooting

The codes clients see are in [api/errors.md](api/errors.md); what each failure does to the data is in [guarantees.md](guarantees.md).

### The server doesn't start

| You see | Means | Do |
|---|---|---|
| exit 2, `invalid configuration` with a list | a setting or variable is wrong or unknown | fix each listed problem; `iwdb-server --check-config` until it exits 0 |
| exit 2, `[tls] cert and [tls] key are not set` | TLS is on by default | give the certificate and key, or `tls.enabled = false` (and `server.plaintext_public` for a non-loopback address) |
| `iwdb-server failed`, `... has no users, so no one could log in` | authentication is on and the store has no user | [First start](#first-start-and-authentication) |
| `iwdb-server failed`, `the data directory is in use: '<dir>/LOCK' is locked by another store` | another server or an `iwctl` command has the directory open | see Locked, below |
| `iwdb-server failed`, `listening on ...: Address already in use` | another process has the port | stop it, or change `listen` |
| `iwdb-server failed` with a corruption error while opening | recovery refuses damaged data rather than repair it | `iwctl verify <dir>` (with the server stopped) names the damage; restore from a backup |

### Locked

One process at a time opens a data directory. A second server, or a local `iwctl` command (`status`, `checkpoint`, `backup <dir>`, `user create <dir>`, ...), is refused while it is open: the server fails to start (`the data directory is in use`), `iwctl` exits 3 (`status` still prints what the files say, marked `in use`), and the code over an API is `unavailable`. With a server running, use `iwctl --server` instead of the directory. A killed process releases the lock.

### A namespace is read-only

A failed WAL write or fsync (a full disk, an I/O error) makes the namespace read-only until the store is reopened: writes fail with `read_only`, reads go on ([guarantees.md](guarantees.md#failed-commits-steps-4-and-5)). The failed commit's outcome is unknown: clients retry it with the same idempotency key. You see it in `iwctl status $S` (`(read-only: <cause>)`), `iwdb_namespace_read_only`, and the log.

1. Fix the cause (free disk space; check the disk and the system log).
2. Restart the server. Recovery replays the WAL and the namespace is writable again.
3. `iwctl verify $S`.

### Checkpoints are disabled

A failure after a checkpoint's file is in place (a directory fsync, removing an old file, or the fsync of the archive's directory) disables checkpoints until the store is reopened, because a retried directory fsync can report success without persisting anything. Commits go on, but the WAL is no longer cut, and recovery after a crash takes longer. `iwctl status $S` shows `checkpoint failed: checkpoints are disabled until the store is reopened, after an earlier failure: <cause>`; `iwdb_checkpoint_failed` is 1. Other checkpoint failures (before the new file is in place) are retried at the next trigger.

1. Fix the cause from the message (disk, permissions, the archive's disk).
2. Restart the server: the final checkpoint and recovery run, and checkpoints are back on.

### `not_retained`

A change-stream reader asked for commits older than the WAL still holds ([ADR 0031](adr/0031-change-stream.md)); the message names the oldest retained seq. The reader starts again from a snapshot. To keep more WAL for slow readers, set `store.retain_records` or `store.retain_age_secs` (the WAL grows by that much). `GET /v1/consumers` shows the readers and their lag.

### `resource_exhausted`

The server refuses writes because its memory is above `memory.refuse_writes_at` ([The memory limit](#the-memory-limit)), or the call is over a limit of its user or namespace: a rate, a count of running calls, or a namespace's memory limit ([Resource limits](#resource-limits-per-user-and-namespace)). The message says which. (Over gRPC, `budget_exceeded` also uses `RESOURCE_EXHAUSTED`; the `iwdb-code` trailer tells them apart.)

### `unavailable`

The server is still recovering (wait for ready), shutting down, or its request queue (`server.queue`) is full. Clients retry with backoff. `iwctl --server` exits 3.

### `corrupt`

Damaged data in the WAL, a checkpoint or the namespace log. Nothing is repaired silently. Stop the server, run `iwctl verify data`, and restore from a backup if it finds damage. A damaged checkpoint alone is skipped by recovery (it falls back to an older one); damage in the WAL isn't.
