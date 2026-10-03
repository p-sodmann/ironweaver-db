# ADR 0031: The change stream reads durable commits from the WAL

Status: accepted
Date: 2026-10-03

## Context

Step 13 asks for a change stream from any retained `seq`, like etcd's Watch or CouchDB's `_changes`, over gRPC (server streaming) and SSE, backed by the WAL, with a clear error when the `seq` is no longer retained and a configurable retention. Its acceptance criterion: a consumer resumes after a restart without gaps or duplicates.

What we have:

- Every namespace has its own WAL and seq space, without gaps (`documentation/formats/wal.md`). A record is the resolved ops of one commit (with explicit edge ids and the version ops of ADR 0004) or a catalog change, with its commit time and idempotency key.
- Under `group` (and `off`) a commit is acknowledged before its fsync. After an OS crash the log ends at its last fsync, and the next commit reuses the seqs that were lost, with other content.
- The checkpointer deletes WAL segments that the oldest kept checkpoint covers (the last segment is never deleted).
- The `Database` trait is request/response with bounded answers (rule 5); streaming RPCs stream one bounded answer (ADR 0025). A watch, by nature, never ends.
- `WalReader` reads a segment into memory whole and checks every record before the start seq. Starting it at an arbitrary seq costs up to a segment (64 MiB by default) of reading and decoding.

## Decision

**1. One bounded, long-polling read on the trait.** `Database::changes(namespace, ChangesRequest { from_seq, wait }, QueryOptions) -> Answer<Changes>` returns the commits with `seq >= from_seq` in seq order: at most `max_results` commits and about 4 MiB of WAL payload (always at least one commit if there is one). If there is none yet and `wait` is set, it waits for one until the timeout, and then answers with an empty batch, not `timeout`. `Changes` holds the events, `next_seq` (where the next call starts) and `first_seq` (the oldest retained seq). `Answer::seq` is the namespace's streamable seq (point 2) when the batch was read.

The streams are this read in a loop, written once in `iwdb-server` (`ops::follow`) for both transports:

- gRPC `Watch` (server streaming): one message per batch, and an empty message with `next_seq` after each long poll that found nothing (a heartbeat).
- REST `GET /v1/namespaces/{ns}/changes/stream`: Server-Sent Events, one `change` event per commit with `id: <seq>`, and a comment line as heartbeat. A reconnecting `EventSource` sends `Last-Event-ID`, and the stream resumes after it.
- Unary `GetChanges` and `GET /v1/namespaces/{ns}/changes` for polling, and `Store`/`Ns::changes` and Python's `changes` for embedded use.

A stream ends with an error when the read fails: `not_retained`, `not_found` (the namespace was dropped), `invalid_argument` (another history), and `unavailable` when the server shuts down. Shutdown ends the streams at the start of the drain (ADR 0027), so they don't hold it up.

**2. Only durable commits.** The stream returns commits up to the **streamable seq**: the lower of the applied seq and the synced seq, or the applied seq under `off`. A commit that a crash can still lose is never streamed, because its seq would come back with other content and the consumer would apply both. Under `always` that is every acknowledged commit. Under `group` the stream lags by up to the group delay. Under `off` nothing is promised (as for durability). `LoggedNamespace` publishes the streamable seq after every apply and fsync, wakes blocking waiters on the condvar `wait_for_seq` already uses, and wakes registered futures. The embedded long poll is such a future (`StreamableWait`, with the store's timer for its deadline): a waiting call holds none of the request workers, which a few watchers would otherwise use up.

**3. Resuming.** A consumer stores the seq of the last commit it processed together with its effect, and resumes from that seq + 1. Delivery is then exactly once: the seqs are gap free, and a durable seq never changes its content. A consumer that stores its position separately sees a commit at least once. A restore starts a new history ([`HistoryId`]), whose seqs mean something else: a consumer passes the history it read with (`QueryOptions::history`), and a call with another history fails with `invalid_argument`, so it can't silently continue in the wrong history.

**4. Retention.** The stream serves what is in `wal/`. A `from_seq` below the oldest record there fails with the new code **`not_retained`** (gRPC `OUT_OF_RANGE`, HTTP 410), whose message names the oldest retained seq; the consumer has to start again from a snapshot. A new store option `WalRetention { records, age }` keeps segments that hold one of the last `records` commits, or a commit younger than `age`, even after a checkpoint covers them. A segment is deleted only when the checkpoint and the retention both allow it. The default keeps nothing beyond what the checkpoints need, which is the behaviour before this step. The server config gets `[store] retain_records` and `retain_age_secs`. The WAL archive isn't read (see Consequences).

**5. Reading the WAL at a seq.** A per-namespace **offset index** (in memory, in `iwdb-storage`) maps every 64th seq of each segment to its frame's offset. It is filled lazily as reads scan segments, so a tailing consumer reads a few frames from a known offset instead of the whole segment, and catching up reads each segment once. The range read is always at or below the streamable seq, so every frame read is complete and synced: any damage is corruption (`corrupt`), never a torn tail. A segment that the checkpointer deletes between listing and opening is `not_retained`. Entries of deleted segments are dropped. The offset comes from the server, never from the client, so a client can't point the reader at bytes inside a record.

**6. Event shape.** An event is the WAL record: `seq`, the commit `time`, the idempotency key if the commit had one, and either the data ops exactly as logged (`Vec<Op<DbRecord, DbRecord>>`) or the catalog change. On the wire the ops are a `ChangeOp` oneof that mirrors the core's `Op`, with records as `attr`, `meta` and `version`; the version op of ADR 0004 (`SetNodeAttr` on `iwdb.version`) becomes `SetNodeVersion` / `SetEdgeVersion`, and back. Values are in the same encoding as everywhere else (ADR 0023: postcard in protobuf, the core's JSON over REST).

## Consequences

- One implementation of reading changes (the trait), one of following them (`ops::follow`), and every access method gets the same events, limits and errors. The conformance suite checks the events against the commits.
- Consumers see changes at the granularity of the log: resolved ops, including version bumps, in the core's vocabulary. That is precise and replayable (applying the ops to an empty graph from seq 1 gives the namespace), but it is not "the node as it is now": a consumer that wants the full node reads it.
- Under `group` the stream lags by up to `2 * max_delay` behind acknowledged commits.
- Retention is per store, not per consumer: a consumer that stops for longer than the retention loses its place (`not_retained`). Consumer positions kept by the server (like Postgres replication slots) are not planned before replication (step 18). Reading older changes from the WAL archive would be possible (restore already reads it as a log), but isn't part of this step.
- `not_retained` is a new error code: a minor change of the contract (`documentation/api/errors.md`).
- The offset index costs about 16 bytes per 64 commits of retained WAL.
