# ADR 0010: Commit times in the WAL (format 2) and restore to a time

Status: accepted
Date: 2026-10-01

## Context

Step 7 asks for point-in-time restore "to a given `seq` or timestamp". WAL format 1 (step 4) records carry a seq but no time, so nothing on disk says when a commit happened. Restoring to a time therefore needs a time in the record, which is a WAL format change (design rule 4: version bump, a reader for the previous version, a fixture), or an explicit deferral.

Questions a time raises:

- Whose clock? The database has one writer per namespace, in one process. Clients have their own clocks, which the database can't trust.
- What does "restore to T" mean when clocks go backwards (NTP steps, a VM migration, a restore onto another machine)? Commit times can then be out of seq order.
- Where does the time go: the frame (written by the WAL writer) or the payload (the engine's `CommitRecord`)?

## Decision

**Add the time now: WAL format 2.** Each frame gets an 8-byte `time` field (microseconds since 1970-01-01 UTC, `i64`) after `synced_seq`, covered by the frame's CRC. The frame header grows from 25 to 33 bytes. The segment header is unchanged except for its version (`2`). The reader reads format 1 segments as before; their records have no time. A writer always starts a new segment, so a log written by step 4 to 6 continues in format 2 without rewriting anything. Fixtures: `wal-v1/` (kept) and `wal-v2/`, written with fixed times through `Wal::append_at`.

- **The writer's clock.** The time is the system clock of the process that appends the record (the store's), at append time. It is the database's time, not the client's: a client can't set it.
- **Non-decreasing.** The writer gives each record the later of the clock and the previous record's time; a new writer starts from the last record of the log's last segment. A clock that goes backwards makes commit times stand still until it catches up, rather than run backwards. That keeps "restore to T" a prefix of the history in practice.
- **Restore to T** picks the **last record in seq order whose time is at or before T**, and restores everything up to it. Seq order decides: if times are ever out of order (a log copied from elsewhere, a writer that started on a header-only last segment and so didn't see the previous time), an earlier record with a later time is still included. Records without a time (format 1) are never picked, but are included when they come before the picked one. If no record in the available WAL is at or before T, restore fails (`NoCommitAtOrBefore`) rather than guess: a checkpoint carries no time.
- **In the frame, not the payload.** The time belongs to the log (when the record was appended), like `synced_seq`; `CommitRecord` and the engine stay unaware of clocks. The reader exposes it next to the record (`WalReader::time`). `Wal::append_at` writes a given time, for fixtures and for copying records with their original time later (replication, step 18).
- **Type.** `iwdb_storage::CommitTime(i64)`, printed and parsed as RFC 3339 with a UTC offset using the core's date-time parser (no new dependency). A time without an offset is refused: it names no instant.

## Consequences

- Every record is 8 bytes larger. Segment rotation points move, which no contract depends on.
- PITR by time works for every commit written from step 7 on. Commits written by earlier versions can be restored to by seq only.
- The guarantee is only as good as the clock: commit times are the writer's wall clock, not a synchronized one. Two stores' times are comparable only as far as their clocks are.
- `CommitResult` doesn't return the commit time yet; step 8 (idempotency) or step 13 (change stream) can add it from the WAL writer.
