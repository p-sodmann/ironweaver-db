# Projection mode

A projection makes a namespace a durable copy of an external, ordered event log: it reads events from a **source** (first: a Postgres table), turns each into mutations with a **mapping**, and commits them. The projection's position in the source, its **mark**, moves in the same commit as the events' effect. A projection resumes from its mark, so whatever crashes, it applies every event exactly once. Design: [ADR 0032](../adr/0032-projection-mode.md).

## Guarantees

- **Exactly once.** A commit moves the mark from the position the projection read to the last event of its batch, compare-and-set. If a crash takes the commit (or the store took it but the projection never heard back), the mark goes with it and the events are read again. If two projections with the same name run at once, the one whose mark is stale commits nothing (`conflict`) and reads again. Tested by crashing every point of the commit path while a projection runs (`a_projection_survives_crashes_without_applying_an_event_twice` in `crates/iwdb/tests/projection.rs`), and by an OS crash under group commit.
- **Durable like any commit.** Under `group`, an OS crash can lose the last commits and their marks together; the projection reads those events again.
- **In order.** Events are applied in the order of their positions. A batch is one transaction.
- **A failing event** (its mapping fails, or its mutations do: a missing node, a constraint) stops the projection with its mark before the event (`on_error = "stop"`, the default), or is committed as its mark alone and counted as skipped (`"skip"`). A source that fails to read is retried with a backoff (1 s, doubling, up to 30 s).

Marks live in the namespace, in its WAL and checkpoints (WAL format 4, data-dir layout 5): they survive restarts, checkpoints, backups and restores (a restore to an older seq restores the marks of that seq, so the projection reads the events after it again). They show in the namespace status (`marks`: name, position, the seq that set it) in Rust, Python, gRPC, REST and `iwctl status`.

## In the server

Each `[[projection]]` in the config file runs while the server runs:

```toml
[[projection]]
name = "orders"                   # the mark's name: unique in its namespace
namespace = "default"             # must exist
batch = 100                       # events per commit
poll_ms = 500                     # how often to look for new events when caught up
on_error = "stop"                 # stop | skip

[projection.source]
kind = "postgres"
url_env = "ORDERS_DB_URL"         # or url = "postgresql://user:pass@host/db"
table = "public.order_events"
position = "id"
gap_timeout_ms = 5000
# columns = ["id", "kind", "payload"]   # default: all

[[projection.rule]]
when = { kind = "order_placed" }
mutations = [
  { upsert_node = { id = "order:${id}", labels = ["Order"], attr = { total = "${payload.total}" } } },
  { upsert_edge = { from = "customer:${payload.customer}", to = "order:${id}", type = "PLACED" } },
]

[[projection.rule]]
when = { kind = "order_cancelled" }
mutations = [{ delete_node = { id = "order:${payload.order}" } }]
```

The server starts its projections after it opened the store and stops them when it shuts down, before the store's last checkpoint.

## Sources

**Postgres** (`iwdb` feature `postgres`; the server has it). The table (or view) has a position column, a `bigint` or `integer` above 0 that increases with every event, typically a `bigserial` primary key. Each row is an event, its columns the fields:

| Postgres | value |
|---|---|
| `smallint`, `integer`, `bigint` | integer |
| `real`, `double precision` | float |
| `text`, `varchar`, `char`, `name` | string |
| `boolean` | bool |
| `json`, `jsonb` | nested values (objects as dicts) |
| `bytea` | bytes |
| `timestamp`, `timestamptz` | datetime (UTC for `timestamptz`) |
| `NULL` | none |

Other types (`numeric`, `date`, `uuid`, arrays) fail the read with an error naming the column; cast them in a view (`amount::float8`, `id::text`).

**Holes in the positions.** A sequence hands out ids when transactions insert, not when they commit: id 7 can be visible before id 5 commits. The source therefore treats positions as dense: when the rows skip one, it returns the rows before the hole and waits for it, up to `gap_timeout_ms` (5 s by default), before it takes the hole for a rolled-back transaction and goes on. Make the timeout longer than your longest transaction that writes the table. A table whose positions are sparse by design sets it to 0; a writer that serializes its inserts (a single writer, or a lock taken before `nextval`) never makes holes that fill later.

No TLS yet (step 15): connect over a private network, with `sslmode=disable` or `prefer`.

**Your own source** (Rust): implement `iwdb::projection::Source` (`read(after, limit)`: events with increasing positions above `after`).

## Rules

A rule's `when` maps field paths (dots go into JSON objects) to values; the rule applies to an event if every one is equal (a missing field doesn't match). Without `when`, it applies to every event. Every matching rule applies, in order, and all their mutations form the event's transaction. An event no rule matches changes nothing; its mark is still committed.

Mutations: `upsert_node` (`id`, `labels`, `attr`, `meta`), `delete_node` (`id`), `set_attr` / `append_attr` (`node`, `key`, `value`), `remove_attr` (`node`, `key`), `add_label` / `remove_label` (`node`, `label`), `upsert_edge` (`from`, `to`, `type`, `attr`, `meta`: the one edge between the two nodes with this type, added if missing).

Templates: a string that is exactly `${path}` is the field's value, of its type (a number stays a number); `${path}` inside a string puts the field's text there (strings, numbers, booleans); `$$` is a `$`. Ids may come from integer fields. A missing field fails the event's mapping. Rules have no expressions: map in SQL (a view) if you need more.

## Embedded (Rust)

```rust
use iwdb::projection::{Projection, ProjectionOptions, Rules};
use iwdb::projection::postgres::{PostgresConfig, PostgresSource};

let source = PostgresSource::new(PostgresConfig::new(url, "order_events", "id"))?;
let projection = Projection::new(MarkName::new("orders")?, source, rules, ProjectionOptions::default());
let handle = store.project("default", projection)?;     // a thread of its own
handle.status();                                         // mark, applied, skipped, state, error
handle.stop();                                           // or store.close()
```

A mapping is any `Mapping`: `Rules`, or a closure `Fn(&SourceEvent) -> Result<Vec<Mutation>, MappingError>`. `Projection::step(&ns)` runs one round in the calling thread. `Ns::commit_marked(mutations, &MarkUpdate { name, expected, position }, options)` is the commit underneath, for projectors of your own.

## Testing against Postgres

The Postgres tests run against [PGlite](https://pglite.dev) (Postgres in WebAssembly; needs only Node):

```sh
scripts/pglite.sh &
export IWDB_TEST_POSTGRES_URL='postgresql://postgres:postgres@127.0.0.1:55432/postgres?sslmode=disable'
cargo test -p iwdb --features postgres --test postgres
cargo test -p iwdb-server --test binary
```

Without the variable they are skipped. CI starts PGlite the same way.

## Not (yet) there

- Projections from Python, and other sources (Kafka, files).
- Marks through the `Database` trait's commit (remote projectors), and removing a mark.
- TLS to Postgres (step 15).
