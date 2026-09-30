# Guarantees

What Ironweaver DB promises, and under which conditions. This document grows with each step. Step 17 turns it into the user-facing guarantees page. Sections are marked with the step that provides them. Anything not listed here is not guaranteed.

## Durability of commits (step 4)

Every commit is appended to the write-ahead log ([format](formats/wal.md)) before it is applied in memory and acknowledged. When it becomes durable depends on the fsync policy ([ADR 0005](adr/0005-wal-fsync-and-failures.md)):

| Policy | Acknowledged commits lost on a process crash | Acknowledged commits lost on an OS crash or power loss |
|---|---|---|
| `always` (default) | none | none |
| `group { max_delay, max_batch }` | none | only the commits after the last completed fsync: a suffix of the log, fewer than `max_batch` commits, written within `max_delay` of each other. With `sync_due` called every `P`, no commit older than `max_delay + P`; the store's timer uses `P = max_delay`, so no commit older than `2 × max_delay` |
| `off` (tests only) | none | anything since the log was created. Recovery may also refuse to open the log |

Conditions:

- The storage honours flushes. On macOS, `fsync` alone doesn't flush the drive's cache; we use `File::sync_all`, which issues `F_FULLFSYNC` there. On Linux it is `fsync`. A drive or virtual disk that acknowledges flushes without persisting them voids every durability guarantee.
- New segment files and renames are made durable with a directory fsync (Unix only).
- Commits are applied in `seq` order, and any lost commits are a suffix: if commit `n` survives, so do all commits before it.
- `Store::open` recovers every commit in the log (step 5, below). The kill -9 test suite that checks this across thousands of crashes comes in step 6.
- **Group commit timer (step 5).** The store calls `sync_due` every `max_delay` in a background thread, so `P = max_delay`: after an OS crash, no acknowledged commit older than about `2 × max_delay` is lost (plus the time the fsync itself takes, and scheduling delays). `Store::close` syncs everything; dropping a store without `close` doesn't.

## Failed commits (steps 4 and 5)

- A commit that fails validation (a conflict, constraint violation, reserved name, a record above 64 MiB, ...) changes nothing, isn't logged, and the namespace stays writable.
- A commit that fails because the log can't be written or fsynced is not applied and not acknowledged. **Its outcome is unknown**: the record may still reach the log and be recovered after a restart. Treat it like a timeout, and retry only if the retry is idempotent (idempotency keys come in step 8).
- After such a failure, the namespace is **read-only until reopened**. Reads keep working and see every applied commit, including group-committed ones whose durability the failure may have cost. Writes fail with a read-only error. A failed fsync is never retried.
- The same holds if applying a logged commit fails. That is a bug, and it poisons the namespace.
- The store exposes this state (`Store::read_only`, step 5). Reopening the store runs recovery and makes it writable again.
- **A panic while the store changes its namespace or WAL** (a commit, an fsync, the group commit timer) aborts the process (step 6, [ADR 0008](adr/0008-panics-in-the-commit-path-abort.md)). It is a crash: no reader sees a partially applied transaction, and the next open recovers every logged commit (the one in flight like a failed fsync: its outcome is unknown). A panic in a `Store::read` closure changes nothing and leaves the store writable.

## Recovery (step 5)

`Store::open` takes the data directory's exclusive lock, then rebuilds the namespace from the newest checkpoint that loads and every WAL record after it ([data-dir.md](formats/data-dir.md)).

- **What it recovers to**: the state after the last complete commit in the log, with the catalog as of that commit. That includes every acknowledged commit the fsync policy made durable (table above): with `always`, every acknowledged commit after any crash. No partial transaction is ever visible: a commit is one log record, applied whole or not at all.
- **A crash during an append** leaves a torn tail, which recovery cuts off. The commit it belonged to was never acknowledged.
- **A crash during a checkpoint** leaves either the previous checkpoints and a temporary file (removed on open), or the new checkpoint without the WAL cut yet. Either way nothing is lost.
- **A damaged checkpoint** (checksum, truncation, format) is skipped, and recovery falls back to the next older one and replays more WAL. The WAL is kept from the oldest kept checkpoint on (2 by default), so this works as long as one kept checkpoint loads.
- **It refuses rather than repairs.** Corruption in the WAL, WAL records missing behind the newest usable checkpoint, a WAL that ends before a checkpoint, or a record that fails to replay: open fails with a typed error and changes no data file.
- **The lock**: a second `Store::open` of the same directory fails with `Locked`, in the same process or another one. The lock is released by `close`, by dropping the store, and when the process exits, also on `kill -9`.

## Checkpoints (step 5)

- A checkpoint holds exactly the commits up to its seq, catalog included, and only commits that were synced to the WAL, so the WAL always reaches it.
- Checkpoints don't block commits: the checkpointer replays the WAL into its own copy of the namespace. Measured commit latency is the same during a checkpoint as outside one (step_5.md). The cost is a second copy of the graph in memory.
- A failed checkpoint never deletes a WAL segment or changes a previous checkpoint. If the failure happens after the new file is in place (a directory fsync or a removal), checkpoints stay off until the store is reopened, and commits continue. `Store::checkpoint_failure` reports it.

## Integrity of the log (step 4)

- Every record and segment header is checksummed (CRC32C over all of its fields and payload).
- A torn or damaged record at the end of the log (from a crash during a write) marks the end of the log. The records before it are intact, and its position is reported for recovery to truncate.
- Damage anywhere else is never skipped silently: in an earlier segment, before a record that proves the damaged one was synced, a gap or a repeat in `seq`, or an unreadable record with a valid checksum. The reader reports it as an error.
- Reading never panics on corrupt input and never allocates more than the file's size for a corrupt length.

## Limits (step 4)

- A commit's WAL record is at most 64 MiB, otherwise the commit is rejected with `RecordTooLarge`.
- Attribute values are nested at most 100 levels deep (`MAX_VALUE_DEPTH`).
- Segment size is 1 KiB to 1 GiB (64 MiB by default). A reader holds one segment in memory at a time.
