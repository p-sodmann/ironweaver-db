# gRPC API

The `iwdb-server` binary serves a data directory over gRPC (step 11) and, on the same port, over REST/JSON (step 12, [rest.md](rest.md)). The contract is `proto/ironweaver_db/v1/*.proto`, package `ironweaver_db.v1`, service `DatabaseService`. Every RPC is one operation of the `Database` trait (`crates/iwdb-query/src/service.rs`) with the same semantics, limits and error codes as the embedded store: the server only translates (design rule 8).

Decisions: [ADR 0023](../adr/0023-wire-encoding-of-values-filters-and-patterns.md) (encoding), [0024](../adr/0024-the-rust-grpc-client.md) (Rust client), [0025](../adr/0025-server-streaming.md) (streaming), [0026](../adr/0026-deadlines-over-grpc.md) (deadlines), [0027](../adr/0027-graceful-shutdown.md) (shutdown), [0028](../adr/0028-internal-apply-errors-abort.md) (aborts).

## Running the server

```
iwdb-server --config server.toml
IWDB_DATA_DIR=/var/lib/iwdb iwdb-server          # no file: the environment alone
```

Only `data_dir` is required (in the file, relative to it, or as `IWDB_DATA_DIR`); everything else has a default. Every setting can be overridden by an `IWDB_*` variable; [config.md](config.md) lists them all, with the rules, the logs and the health endpoints. `iwdb-server --check-config` validates a configuration and prints the effective settings.

```toml
data_dir = "/var/lib/iwdb"
listen = "127.0.0.1:7600"           # gRPC and REST

[store]
fsync = "always"                  # always | group | off (guarantees.md)
group_max_delay_ms = 10
group_max_batch = 64
checkpoint_on_shutdown = true
retain_records = 0                # WAL kept for the change stream: the last N commits ...
retain_age_secs = 0               # ... and commits younger than this (changes.md)

[server]
drain_timeout_secs = 30
max_message_bytes = 67108864      # largest request or answer message (64 MiB); REST: request body
workers = 0                       # threads running requests (0: one per CPU)
queue = 1024                      # requests waiting for a worker; more fail with `unavailable`
unready_delay_ms = 0              # on shutdown: serve unready this long before draining

[log]
format = "auto"                   # json unless stderr is a terminal | json | text
level = "info"                    # a filter: "warn,iwdb_storage=debug"

[console]                         # the operator console at /console/ (feature `console`)
enabled = false
public = false                    # allow it on a non-loopback address (no authentication until step 15)

[limits.default]                  # what a read gets if it asks for nothing
max_results = 1000
max_visited = 100000
max_edges = 1000000
timeout_ms = 30000

[limits.max]                      # what no read can exceed
max_results = 100000
max_visited = 10000000
max_edges = 100000000
timeout_ms = 300000
```

`[[projection]]` sections run projections from Postgres tables into namespaces while the server runs ([projections.md](projections.md)). Their marks are in `NamespaceStatus.marks`.

- **Startup.** The port opens first and answers health while the store recovers; database calls fail with `unavailable` until recovery has finished, then the server is ready (`grpc.health.v1.Health` reports `SERVING`, `GET /v1/health/ready` answers 200; [ADR 0040](../adr/0040-health-and-readiness.md)). A bad configuration is refused before that, with every problem listed (exit 2).
- **SIGINT / SIGTERM** shut down gracefully (below): readiness turns off first (and with `unready_delay_ms` the server keeps serving that long), then the drain; a second signal cancels the calls still running. Exit codes: 0 after a clean shutdown, 1 if serving or closing the store failed, 2 for a bad command line or configuration.
- **Logs** are JSON lines on stderr unless it is a terminal ([config.md](config.md#logs)).
- **Run it under a supervisor** (systemd, Kubernetes). A bug in a commit's apply path (a panic, or `GraphError::Internal` from the core) aborts the whole process, as a crash, so that no reader ever sees part of a transaction (ADR 0008, ADR 0028). The next start recovers every logged commit.
- **No TLS and no authentication yet** (step 15): bind to localhost or a private network.
- Metrics and status views come with step 16c.

### Features and Docker

`iwdb-server` has three cargo features (ADR 0034); the first two are on by default:

- `rest`: the REST/JSON API on the same port ([rest.md](rest.md)). Without it the server speaks gRPC only, and answers every other request 404.
- `postgres`: the Postgres source of `[[projection]]`s. Without it, a config file with a Postgres source is refused at startup (exit 2).
- `console` (off by default; implies `rest`): the operator console's pages, compiled in and served at `/console/` when `[console] enabled = true` ([ADR 0041](../adr/0041-console-served-by-the-server.md)).

`cargo build -p iwdb-server --no-default-features` builds a gRPC-only server. `iwdb-server --version` lists what a binary has.

The `Dockerfile` at the root builds an image with `iwdb-server` and `iwctl`, with the same features chosen by the `FEATURES` build argument:

```
docker build -t iwdb .                                # gRPC, REST, Postgres projections, console
docker build --build-arg FEATURES="" -t iwdb:grpc .   # gRPC only
docker run -p 127.0.0.1:7600:7600 -v iwdb-data:/var/lib/iwdb iwdb
```

The image runs as the user `iwdb` (uid 10001), keeps its data in the volume `/var/lib/iwdb`, and reads `/etc/iwdb/iwdb.toml` ([docker/iwdb.toml](../../docker/iwdb.toml): listens on `0.0.0.0:7600`, drains for 8 s so that `docker stop` ends with a checkpoint). Mount your own config there, or set `IWDB_*` variables (`docker run -e IWDB_STORE_FSYNC=group ...`); if you raise `drain_timeout_secs`, raise `docker stop -t` above it. `compose.yaml` runs it, and with `--profile postgres` a Postgres with an example projection. Run `iwctl` against the volume only while the server is stopped: one process opens a data directory at a time. The image's `HEALTHCHECK` runs `iwdb-server --probe` (ready once recovery has finished). The console is compiled in but off: `-e IWDB_CONSOLE_ENABLED=true -e IWDB_CONSOLE_PUBLIC=true -p 127.0.0.1:7600:7600` serves it at `http://127.0.0.1:7600/console/` (public, because the container listens on `0.0.0.0`; publish the port on localhost only).

## RPCs

| RPC | Trait method | Answer |
|---|---|---|
| `Commit` | `commit` | unary |
| `CommitCatalog` | `commit_catalog` | unary |
| `WaitForSeq` | `wait_for_seq` | unary |
| `GetNodes`, `GetEdges` | `get_nodes`, `get_edges` | stream |
| `Find` | `find` (paginated) | stream |
| `Explain` | `explain` | unary |
| `Neighbourhood` | `neighbourhood` (paginated) | stream |
| `Traverse` | `traverse` | stream |
| `ShortestPath` | `shortest_path` | unary |
| `RandomWalks` | `random_walks` | stream |
| `Subgraph` | `subgraph` | stream |
| `MatchPattern` | `match_pattern` (paginated) | stream |
| `Analyze` | `analyze` (ADR 0022) | stream |
| `GetCatalog` | `catalog` | unary |
| `GetNamespaceStatus` | `namespace_status` | unary |
| `ListNamespaces` | `namespaces` | unary |
| `CreateNamespace`, `DropNamespace` | `create_namespace`, `drop_namespace` | unary |
| `GetChanges` | `changes` (ADR 0031) | unary |
| `Watch` | `changes` with `wait`, in a loop | stream, until cancelled, an error or shutdown ([changes.md](changes.md)) |

Every operation names a namespace; seqs, cursors, idempotency keys and catalogs are per namespace.

## Versioning

`v1` changes only compatibly: new fields, messages, RPCs, enum values and `oneof` members. `buf breaking` (the `FILE` rules) guards it in CI against the base branch. A change that breaks it needs `v2`. The bytes inside `Value`, `Expr` and `Pattern` (below) are part of the contract too; a test (`crates/iwdb-server/tests/wire.rs`) pins them.

## Values, filters and patterns

Everything structural is a plain proto message. Three of the core's types travel in the core's serde form, encoded with [postcard](https://postcard.jamesmunns.com/wire-format) (ADR 0023):

```proto
message Value   { oneof form { bytes postcard = 1; } }
message Expr    { oneof form { bytes postcard = 1; } }
message Pattern { oneof form { string text = 1; bytes postcard = 2; } }
```

Attributes and meta are `map<string, Value>`. The depth limits are the core's: values and filters nested up to 100 levels are valid, deeper ones are `invalid_argument` with the core's message (`attribute values nested more than 100 levels deep`, `expression nested more than 100 levels deep`).

**Postcard, for clients in other languages.** Integers are varints (LEB128); signed integers are zigzag-encoded first; a string or byte string is its length as a varint, then its bytes; a sequence or map is its length, then its items; an `Option` is `00` (none) or `01` and the value; an enum is its variant index as a varint, then its fields in order. Floats are 8 bytes, little-endian.

| `Value` variant | Index | Then |
|---|---|---|
| `String` | 0 | a string |
| `Int` | 1 | zigzag varint (`-30` → `3b`) |
| `Float` | 2 | 8 bytes LE (`1.5` → `000000000000f83f`) |
| `Half` | 3 | the f16 bits as a varint (`1.5` = `0x3e00` → `807c`) |
| `Bool` | 4 | `00` or `01` |
| `None` | 5 | nothing |
| `List` | 6 | a sequence of `Value`s |
| `Dict` | 7 | a map of string → `Value` (the server writes keys sorted) |
| `Bytes` | 8 | a byte string |
| `Date` | 9 | zigzag varint: days since 1970-01-01 |
| `DateTime` | 10 | zigzag varint: microseconds since 1970-01-01T00:00 (local to the offset), then `Option` of a zigzag varint: the offset in seconds east of UTC (none: a wall-clock time) |

| `Expr` variant | Index | Then |
|---|---|---|
| `Const` | 0 | a bool |
| `Compare` | 1 | path (sequence of strings), op (`Eq` 0, `Ne` 1, `Lt` 2, `Le` 3, `Gt` 4, `Ge` 5), a `Value` |
| `In` | 2 | path, a sequence of `Value`s |
| `Exists` | 3 | path |
| `Label` | 4 | a string |
| `Type` | 5 | a string |
| `And`, `Or` | 6, 7 | a sequence of `Expr`s |
| `Not` | 8 | an `Expr` |

Examples: `{"name": "ann"}`'s value is `00 03 61 6e 6e`; `age >= 18` is `01 01 03 61 67 65 05 01 24`; `Label("Person")` is `04 06 50 65 72 73 6f 6e`. The semantics are the core's (`Expr` in `ironweaver-core`): missing attributes make every comparison false, numbers compare across ints and floats, the paths `labels` and `type` are the graph's.

**Patterns** are best sent as the core's text (`(a:Person {age: 30})-[:knows*1..3]->(b)`); `postcard` carries the patterns the text can't express (filters other than property equality, bound ids). The Rust client sends the text whenever `Pattern::to_text` accepts the pattern.

REST ([rest.md](rest.md)) shows these three messages in the core's JSON form instead (`{"Int": 30}`, `{"Label": "Person"}`, a pattern as its text), with the same depth limits.

## Options and limits

Reads take `QueryOptions`: `min_seq` (read-your-writes) with `history` (32 hex digits; a seq of another history is `invalid_argument`), `timeout_ms`, `limits` (`max_results`, `max_visited`, `max_edges`), `partial` and `cursor`. A missing limit takes the server's default, one above the server's cap is lowered to the cap, 0 is `invalid_argument`. What each operation counts is documented on the trait (`iwdb-query`) and in [ADR 0021](../adr/0021-bounded-reads-and-cursors.md).

## Answers, streaming and pagination

Unary answers carry an `AnswerMeta` next to their value: the `seq` the read saw, `next` (the cursor of the next page, empty if there is none), `truncated` and `work`.

A streaming RPC answers **one** trait call, in chunks (ADR 0025): every message has the same item fields, each holds about 1 MiB at most (one item may be bigger), and only the **last** one has `meta`. An empty answer is one message with only `meta`. A stream that ends without `meta` was cut off: treat it as `unavailable`.

The server never follows a cursor. To read a large result, pass `meta.next` as `options.cursor` with the same request; a page is served only at the seq of the first one, and fails with `cursor_expired` (`FAILED_PRECONDITION`) if the namespace changed in between. Start again then.

## Deadlines and cancellation

- **Reads** run until the smaller of `grpc-timeout` and `QueryOptions.timeout_ms`, capped by the server's maximum, counted from when the server starts the call, including any wait for a worker or for `min_seq`. Running out of time is `timeout` (`DEADLINE_EXCEEDED`). The Rust client sends `timeout_ms` and no `grpc-timeout`.
- **A client that cancels a call or disconnects** stops its read: the server drops the call, and the read stops at the core's next check.
- **Commits** (`Commit`, `CommitCatalog`, `CreateNamespace`, `DropNamespace`) have no deadline on the server. Once accepted, a commit runs to the end even if the client gives up, so after a client-side deadline, a lost connection or `unavailable`, its outcome is unknown. **Send an idempotency key with every commit you may retry** (`CommitOptions.idempotency_key`, 1 to 255 bytes, for example a UUID): a retry with the same key and request returns the original result with `deduplicated` set, and applies the commit if it wasn't applied ([ADR 0015](../adr/0015-idempotency-keys.md)).

## Errors

A failed call ends with the gRPC status of its error code, and the code itself, as a string, in the trailing metadata key **`iwdb-code`**. Branch on `iwdb-code`, never on the message or the status alone: `constraint_violation` and `cursor_expired` are both `FAILED_PRECONDITION`, and `read_only`, `unavailable` and `io` are all `UNAVAILABLE`. The codes, their meaning, whether to retry, and the status of each are in [errors.md](errors.md). A status without `iwdb-code` comes from the transport (a refused connection, a message over `max_message_bytes`, a client-side deadline).

## Shutdown

On SIGINT or SIGTERM the server stops accepting connections and sends every HTTP/2 connection GOAWAY (new calls fail with `UNAVAILABLE`; HTTP/1.1 connections of REST clients close after their current request), lets running calls finish for up to `drain_timeout_secs`, and then closes the connections still open, which cancels their reads. Commits that were accepted are applied. `Watch` streams end with `unavailable` when shutdown starts ([changes.md](changes.md)). It then flushes every namespace's WAL, writes a checkpoint (if `checkpoint_on_shutdown`) and releases the data directory: every commit acknowledged before shutdown is durable, whatever the fsync policy ([ADR 0027](../adr/0027-graceful-shutdown.md)).

## The Rust client

`iwdb_server::client::Remote` (feature `client`) implements the `Database` trait over gRPC, so code written against the trait runs embedded or remote:

```rust
use iwdb_query::{exec::block_on, Database, FindRequest, QueryOptions};
use iwdb_server::client::Remote;

let db = Remote::connect("http://127.0.0.1:7600")?;
let answer = block_on(db.find("default", FindRequest { filter: ironweaver_core::Expr::Label("Person".into()) }, QueryOptions::default()))?;
```

It runs its calls on a small tokio runtime of its own (`Remote::connect_on` takes another one), so any executor can await them; dropping a call's future cancels it. It connects on the first call and reconnects after a lost connection.
