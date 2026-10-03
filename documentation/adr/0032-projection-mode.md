# ADR 0032: Projection mode keeps its high-water mark in the commit

Status: accepted
Date: 2026-10-03

## Context

Step 13, part 2: Ironweaver DB as a durable projection of an external, ordered event log. A projector reads events from a source (first: a Postgres table with an increasing id), maps them to mutations, and commits them. The acceptance criterion: projection mode survives crashes without applying an event twice.

Exactly once needs the position in the source (the **high-water mark**) to be stored atomically with the effect of the events up to it. If the mark is stored before the commit, a crash in between loses events; if after, a crash in between applies them twice. So the mark must be part of the commit: in its WAL record, in the namespace's state, and in checkpoints.

What we have:

- A commit record carries its change and, since WAL format 3, an optional idempotency key with its result (ADR 0015). The namespace keeps a key table, changed only by applying records and saved in checkpoints (`iwdb.keys`, layout 3). The same pattern fits a mark.
- Idempotency keys can't carry the mark: a key names one request, and the table forgets a key after 10 000 keyed commits, so it can't tell a projector where to resume.
- The core has no op for graph-level data, and the graph's nodes are user data: a reserved "position node" would show up in every query.
- Rule 4: changing the WAL payload or a checkpoint's meta keys bumps the format's version, with a reader for the old one and a fixture.
- Postgres ids from a sequence are assigned when a transaction inserts, not when it commits: a reader can see id 7 before id 5 commits, and a mark at 7 would skip 5 for good.

## Decision

**1. Marks.** A namespace keeps named **marks**, `name -> position` (`u64`, at most `i64::MAX`), with the seq of the commit that set each. A commit may carry a `MarkUpdate { name, expected, position }`: it applies only if the mark is at `expected` (`None`: no mark yet) and `position > expected`, otherwise it fails with `MarkConflict` (`conflict`) and changes nothing. Applying the record sets the mark. So two projectors with the same name can't both commit the same events, and replaying the log rebuilds the marks exactly. A commit with a mark may have no mutations: it records events that were skipped. Names are 1 to 255 bytes of UTF-8, a namespace holds at most 1 024 marks, and marks are never removed (a later step can add that). Marks are exposed as `Ns::mark` / `marks` and in `NamespaceStatus.marks`, not in the change stream's events.

**2. Formats.** WAL format 4: the payload is `(keyed, mark, body)`, with `mark: Option<Mark { name, position }>` (the record needs no `expected`: replay doesn't check). Format 3 is still read (its records have no mark), and a writer starts a new segment in format 4. Data-dir layout 5: a checkpoint's graph meta holds `iwdb.marks` too (a JSON string, like `iwdb.keys`); a checkpoint without it (layout 4 and older) has no marks. A layout 4 directory is upgraded on open by rewriting its marker, its files unchanged; layouts 1 to 3 are upgraded straight to 5 as before. Fixtures `wal-v4` and `data-dir-v5`.

**3. The projector** lives in `iwdb::projection`, below the adapters and without async:

- `Source`: `read(after, limit)` returns events with strictly increasing positions above `after`. An event is a position and a map of fields (`Attrs`).
- `Mapping`: an event to a list of mutations. `Rules` is the declarative one (point 4); Rust callers can implement the trait.
- The runner reads up to `batch` events after the namespace's mark, maps them, and commits all their mutations with `MarkUpdate { expected: mark, position: last }`. If the commit fails with an error of the data (a constraint, a missing node), it retries the batch one event at a time, and an event that fails alone stops the projection (`on_error = "stop"`, the default) or is committed as its mark alone (`"skip"`). A mapping error is treated the same. A `MarkConflict` means another projector moved the mark: the runner reads the mark again and goes on from there. Source errors are retried with a backoff (1 s, doubling, at most 30 s) and reported in the status. Positions that don't increase are an error of the source and stop the projection.
- `Projection::step(target)` runs one round in the calling thread; the target is a `Target` (read a mark, commit with a mark): `Ns`, or the store's own handle for its threads. `Store::project(namespace, projection)` runs a projection on a thread of its own and returns a handle with `status()` (mark, events applied and skipped, the last error; running, caught up, retrying, stopped or failed) and `stop()`. Closing (or dropping) the store stops its projections first. `Ns::commit_marked` is the commit underneath, public for projectors of one's own.

Because the mark and the effect are one commit, and the runner always resumes from the committed mark, every event is applied at most once and, unless skipped, exactly once, whatever crashes: a commit either reached the log (and is recovered with its mark) or didn't (and its events are read again). The durability of a projection's progress is the store's fsync policy.

**4. Rules.** A projection's mapping in the server's config:

```toml
[[projection.rule]]
when = { kind = "customer_created" }     # every field equal; no `when`: every event
mutations = [
  { upsert_node = { id = "customer:${customer_id}", labels = ["Customer"], attr = { name = "${name}" } } },
]
```

Every rule whose `when` matches applies, in order, and their mutations form the event's transaction. In a template, a string that is exactly `${path}` becomes the field's value (any type; `path` goes into dicts with dots), a string with `${path}` inside it is text with the field's text in it, and `$$` is a literal `$`. A missing field is a mapping error. An event no rule matches maps to nothing: it is committed as its mark alone. No expressions: rules are data, like `Expr` filters (rule 5).

**5. The Postgres source** (feature `postgres` of `iwdb`, the sync `postgres` crate; enabled by `iwdb-server`): `SELECT <columns> FROM <table> WHERE <position> > $1 ORDER BY <position> LIMIT $2`. Columns become fields (integers, floats, text, booleans, `bytea`, `json`/`jsonb` as nested values, timestamps as datetimes; other types, `numeric`, `date` and `uuid` among them, are an error that names the column, so that no value is silently approximated: cast them in a view). No more crates than `postgres` itself: `numeric` or `uuid` would each need one. Positions are taken to be **dense**: when the rows read skip a position, the source returns the rows before the hole and waits for it, up to `gap_timeout` (5 s by default), because a transaction may hold the missing id and commit later. After the timeout it treats the hole as a rollback and goes on. Positions from a source that are sparse by design set `gap_timeout = 0`. The first read of a projection (no mark) starts at the lowest row. A writer that serializes its inserts (one writer, or a lock taken before the id) never makes holes that fill later, and needs no waiting.

**6. Server.** `[[projection]]` sections start projections when the server has opened the store; they are stopped when serving has ended, before the store closes (so their last commits are in its final checkpoint). A namespace a projection names must exist. The Postgres URL can come from an environment variable (`url_env`), so a password needn't be in the file.

## Consequences

- Exactly once without a second store and without two-phase commit: the mark is data of the namespace, committed and replayed like the rest. The same mechanism serves any projector written against `iwdb::projection`.
- A WAL and a layout version more. Older versions refuse directories this version has written (as with layout 4).
- The projector holds a thread per projection and a connection per Postgres source. That is fine for a handful of projections, which is the use case.
- The gap wait trades latency for completeness: a rolled back id delays the projection by `gap_timeout` once. A source that loses rows (deleted, or committed later than the timeout) is not detected; the timeout should be longer than the longest transaction that writes the table.
- PGlite runs one Postgres session, so the Postgres tests can't produce truly concurrent transactions; the gap logic is tested with explicit ids, which is what the reader sees either way.
- Not in this step: marks over the `Database` trait's commit (remote projectors), removing marks, other sources (Kafka, files), and projections from Python.
