# Step 13: Change stream, projection mode and bulk import/export

Status: in progress
Milestone: M3 Network access
Depends on: step 12

## Goal

Other systems can follow every change, and Ironweaver DB can act as a durable projection of an external event log.

## Parts

The step has three parts that build on each other, done in this order, each in its own commits (and PR if it gets large):

1. **Change stream** (refined below, [ADR 0031](../adr/0031-change-stream.md)).
2. **Projection mode** (uses the commit pipeline and idempotent high-water marks). Refined below, [ADR 0032](../adr/0032-projection-mode.md).
3. **Bulk import/export** (uses checkpoints). Refined when part 2 is done.

## Decisions (part 1)

[ADR 0031](../adr/0031-change-stream.md):

- The trait gets one bounded, long-polling read, `changes(namespace, {from_seq, wait}, options)`: commits with `seq >= from_seq`, at most `max_results` and about 4 MiB per batch. gRPC `Watch`, the SSE route and the polling routes are that read in a loop, written once (`ops::follow`).
- Only durable commits are streamed (up to the lower of the applied and the synced seq), so a seq never changes its content. Resuming from the last processed seq + 1 is gap free and, when the consumer stores its position with its effect, exactly once. A consumer passes its history, so a restore can't be mistaken for the same log.
- Retention: `WalRetention { records, age }` keeps WAL segments beyond what checkpoints need. Older seqs fail with the new code `not_retained` (gRPC `OUT_OF_RANGE`, HTTP 410).
- A per-namespace offset index (every 64th seq) lets a batch start near its seq instead of reading the whole segment.
- Events are the WAL records: seq, time, idempotency key, and the data ops as logged (version ops as `SetNodeVersion` / `SetEdgeVersion` on the wire) or the catalog change.

Refined while writing the code:

- **The long poll holds no thread.** A waiting `changes` call would otherwise hold one of the embedded store's request workers for its whole timeout, so a few watchers could starve every other read. The namespace wakes registered futures when its streamable seq advances, and the store's timer wakes them at their deadline (`StreamableWait`). The read itself then runs on a worker. The wait ends a little before the deadline (a tenth of the timeout, at most 1 s), so the read has time to answer.
- **Retention lives in `[store]`** (`retain_records`, `retain_age_secs`): the server config has no `[wal]` section.
- **The first batch of a stream doesn't wait**, so an error (`not_found`, `not_retained`) comes before any event: over REST it is the HTTP status, and a new stream reports `first_seq` at once.
- **SSE resumes with `Last-Event-ID`**, which `EventSource` sends when it reconnects.

## Tasks

- [x] First: run the [upstream check](upstream-check.md). Issues this step depended on: #26, #31 (fixed at `3b15149`) and #46 (JSON export of NaN and infinities, fixed at `cd09ea0`). *(Checked 2026-10-03 at `7e7b7fa`: core `main` is the pinned revision and no issue of ours is open. Nothing to bump or adopt.)*

### Part 1: change stream

- [x] Storage: the streamable seq in `LoggedNamespace` (published after apply and fsync, with a blocking wait and wakers for futures); `changes::OffsetIndex`, a range reader of the WAL; `WalRetention` in the checkpointer.
- [x] `Store` / `Ns::changes` and `StoreOptions::retention`; `Database::changes` and its embedded implementation (`StreamableWait`); the `not_retained` code.
- [x] Protos (`changes.proto`): `ChangeEvent`, `ChangeOp`, `GetChanges` and `Watch`; conversion both ways (`convert/changes.rs`).
- [x] gRPC `GetChanges` and `Watch`; REST `GET .../changes` and SSE `GET .../changes/stream` (with `Last-Event-ID`); OpenAPI; streams end at shutdown (`Server::stopping`, `ops::follow`).
- [x] The Rust gRPC and REST clients implement `changes`; four conformance cases for changes over every access method.
- [x] Python: `Store.changes` / `Namespace.changes`, events as dicts; `Store.open(retain_records=, retain_age=)`.
- [x] Server config: `[store] retain_records` and `retain_age_secs`.
- [x] Tests: resume after restarts without gaps or duplicates, rebuilding the namespace from the events; after an OS crash under `group` no streamed commit is taken back (checked to fail when the stream returns unsynced commits); retention by count and age, and `not_retained` without it; every range read like the full reader, cold and warm; damage in the read range is `corrupt`; SSE events, resume with `Last-Event-ID`, errors as statuses; `Watch` heartbeats; both streams end at shutdown. Not tested: a segment deleted between listing and reading (a race the checkpointer would have to hit in a window of microseconds; the code maps it to `not_retained`).
- [x] Docs: [api/changes.md](../api/changes.md) (new), errors.md (`not_retained`), grpc.md, rest.md, guarantees.md, data-dir.md (retention), the design doc.

### Part 2: projection mode

The original task, "pluggable source for an external ordered log (first: a Postgres table with a monotonically increasing id), mapping config from events to mutations, high-water mark committed in the same transaction as the changes", refined ([ADR 0032](../adr/0032-projection-mode.md)):

- [x] **Marks** (the high-water marks): a namespace keeps named marks (`name -> position`), set by the commit that applies the events up to the position, compare-and-set (`MarkUpdate { name, expected, position }`, a mismatch is `conflict`). A commit with a mark may have no mutations (skipped events). WAL format 4 (the payload gets the mark), data-dir layout 5 (checkpoints keep the marks, `iwdb.marks`; layout 4 is upgraded by rewriting the marker); readers for WAL 3 and layout 4, fixtures for both new versions. Crash tests of marks through recovery and checkpoints.
- [x] `CommitOptions::mark`, `Ns::mark` / `marks`; `NamespaceStatus.marks` over gRPC, REST and `iwctl`.
- [x] **Projector** (`iwdb::projection`): the `Source` trait (events with strictly increasing positions after a given one), the `Mapping` trait (an event to mutations), and a runner that reads a batch after the mark, maps it and commits it with the mark in one commit. A failing batch is retried an event at a time; a failing event stops the projection or, with `on_error = "skip"`, is committed as a mark alone. `Store::project` runs one on a thread of its own, with a handle to stop it and read its status.
- [x] **Rules**, the declarative mapping (TOML in the server's config, serde): rules match events by field values and produce mutation templates (`upsert_node`, `delete_node`, `set_attr`, `remove_attr`, `append_attr`, `upsert_edge`) whose strings take `${field.path}` from the event.
- [x] **Postgres source** (`iwdb` feature `postgres`, sync `postgres` crate): a table with a `bigint` position column; rows become events with their columns as fields. Positions are assumed dense: a hole in them is waited for up to `gap_timeout` (a transaction that took a lower id and hasn't committed yet), then skipped (a rollback). Tests run against PGlite (below) when `IWDB_TEST_POSTGRES_URL` is set.
- [x] Server: `[[projection]]` in the config (name, namespace, source, rules, batch, on_error); projections start with the server and stop at the drain.
- [x] Tests: the acceptance criterion (crashes at every failpoint of the commit path while a projection runs, then the projection finishes: every event applied once, checked with a non-idempotent mapping); a mark conflict between two projectors; skip and stop on a bad event; the Postgres source against PGlite (types, resume, gaps); a server with a projection.
- [x] Docs: ADR 0032, `api/projections.md`, wal.md (v4), data-dir.md (layout 5), guarantees.md, errors.md, grpc.md (the server config), the design doc.

Refined while writing the code:

- **The layout 4 upgrade** only rewrites the marker, after recovery has read every namespace and before the WAL writers start (they write format 4): a failed open leaves the old marker, like the layout 1 to 3 upgrade (`a_failed_marker_upgrade_fails_the_open_and_the_next_one_finishes_it` now covers layout 4).
- **Postgres types**: `numeric`, `date` and `uuid` are refused with an error naming the column, not read as floats: each would need a crate of its own, and an approximated value would be silently wrong. Cast them in a view.
- **`Projection::step(&impl Target)`** runs a round in the caller's thread (the tests drive the crash cases with it); `Store::project` runs it on a thread, and `Store::projections` lists them. `Ns::commit_marked` is public for projectors of one's own.
- **The server stops its projections when serving has ended**, before the store's close, rather than at the start of the drain: they don't hold up the drain, and their last commits land in the final checkpoint.
- **Rules** got `add_label` and `remove_label` too.
- **Tests**: the acceptance test fails at each point of the commit path (WAL write before, halfway, after; fsync before, after), after 0, 2 and 7 writes, with batches of 1 and 4, and was checked to fail when the mark is committed apart from the events. The Postgres tests and a server binary test with a projection run against PGlite (`scripts/pglite.sh`, also in CI).

### Part 3: bulk import/export

- [ ] Bulk import/export: ironweaver JSON/binary files, LGF, CSV edge lists, GraphML; streaming with progress; import produces one consistent checkpoint instead of millions of WAL records. *(Since core `d15a7ec` (upstream #54), ironweaver 0.1 binary files can't be imported: the core refuses them with a message saying how to convert them. 0.1 JSON files still load and are migrated by the core.)* *(Parquet was dropped (decision of 2026-10-03): it would bring the `arrow`/`parquet` dependency tree for one import format.)*

## Notes for parts 2 and 3

- **Postgres tests run against [PGlite](https://pglite.dev)** (decision of 2026-10-03): Postgres compiled to WebAssembly, served over the wire protocol by `@electric-sql/pglite-socket`, so a developer machine and CI need only Node, no Postgres and no Docker. `scripts/pglite.sh` starts it and prints the URL to put in `IWDB_TEST_POSTGRES_URL`; without the variable the Postgres tests are skipped. PGlite is a single Postgres session: concurrent transactions (a lower id committed after a higher one) can't be produced there, so the gap handling is tested with explicit ids, which is the same situation as seen by the reader.
- **Formats.** The core reads and writes only its own JSON and binary files. LGF, GraphML and CSV are ours to parse (in `iwdb`, not upstream: they are import formats, not the core's file format).

## Acceptance criteria

- [x] A consumer resumes the change stream after a restart without gaps or duplicates (`a_consumer_resumes_after_restarts_without_gaps_or_duplicates` and `an_os_crash_under_group_commit_takes_back_no_streamed_commit` in `crates/iwdb/tests/changes.rs`; the conformance cases over embedded, gRPC and REST).
- [x] Projection mode survives crashes without applying an event twice (`a_projection_survives_crashes_without_applying_an_event_twice` and `an_os_crash_under_group_commit_loses_events_and_their_marks_together` in `crates/iwdb/tests/projection.rs`).

## Non-goals

- Server-side consumer positions (like replication slots) and reading the WAL archive in the stream (ADR 0031).
- Filtering the stream on the server (by label, type or key): consumers filter the ops themselves.
- Authentication of streams: step 15. Limits on concurrent watches: step 16.
