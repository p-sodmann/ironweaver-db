# Step 13: Change stream, projection mode and bulk import/export

Status: in progress
Milestone: M3 Network access
Depends on: step 12

## Goal

Other systems can follow every change, and Ironweaver DB can act as a durable projection of an external event log.

## Parts

The step has three parts that build on each other, done in this order, each in its own commits (and PR if it gets large):

1. **Change stream** (refined below, [ADR 0031](../adr/0031-change-stream.md)).
2. **Projection mode** (uses the commit pipeline and idempotent high-water marks). Refined when part 1 is done.
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

- [ ] Pluggable source for an external ordered log (first: a Postgres table with a monotonically increasing id), mapping config from events to mutations, high-water mark committed in the same transaction as the changes.

### Part 3: bulk import/export

- [ ] Bulk import/export: ironweaver JSON/binary files, LGF, CSV/Parquet edge lists, GraphML; streaming with progress; import produces one consistent checkpoint instead of millions of WAL records. *(Since core `d15a7ec` (upstream #54), ironweaver 0.1 binary files can't be imported: the core refuses them with a message saying how to convert them. 0.1 JSON files still load and are migrated by the core.)*

## Notes for parts 2 and 3

- **Postgres.** Neither Postgres nor Docker is assumed on a developer machine. The source is a trait with an in-process test source for the crash tests. The Postgres tests run when `IWDB_TEST_POSTGRES_URL` is set, which CI does with a service container. The Postgres client (`tokio-postgres`) is a new dependency of the crate that holds the source, behind a feature.
- **Formats.** The core reads and writes only its own JSON and binary files. LGF, GraphML and CSV are ours to parse (in `iwdb`, not upstream: they are import formats, not the core's file format). Parquet needs the `parquet`/`arrow` crates, a large dependency tree: decide in part 3 whether it goes behind a feature or is deferred.

## Acceptance criteria

- [x] A consumer resumes the change stream after a restart without gaps or duplicates (`a_consumer_resumes_after_restarts_without_gaps_or_duplicates` and `an_os_crash_under_group_commit_takes_back_no_streamed_commit` in `crates/iwdb/tests/changes.rs`; the conformance cases over embedded, gRPC and REST).
- [ ] Projection mode survives crashes without applying an event twice.

## Non-goals

- Server-side consumer positions (like replication slots) and reading the WAL archive in the stream (ADR 0031).
- Filtering the stream on the server (by label, type or key): consumers filter the ops themselves.
- Authentication of streams: step 15. Limits on concurrent watches: step 16.
