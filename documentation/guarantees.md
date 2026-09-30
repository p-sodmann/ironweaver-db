# Guarantees

What Ironweaver DB promises, and under which conditions. This document grows with each step. Step 17 turns it into the user-facing guarantees page. Sections are marked with the step that provides them. Anything not listed here is not guaranteed.

## Durability of commits (step 4)

Every commit is appended to the write-ahead log ([format](formats/wal.md)) before it is applied in memory and acknowledged. When it becomes durable depends on the fsync policy ([ADR 0005](adr/0005-wal-fsync-and-failures.md)):

| Policy | Acknowledged commits lost on a process crash | Acknowledged commits lost on an OS crash or power loss |
|---|---|---|
| `always` (default) | none | none |
| `group { max_delay, max_batch }` | none | only the commits after the last completed fsync: a suffix of the log, fewer than `max_batch` commits, written within `max_delay` of each other. With `sync_due` called every `P`, no commit older than `max_delay + P` |
| `off` (tests only) | none | anything since the log was created. Recovery may also refuse to open the log |

Conditions:

- The storage honours flushes. On macOS, `fsync` alone doesn't flush the drive's cache; we use `File::sync_all`, which issues `F_FULLFSYNC` there. On Linux it is `fsync`. A drive or virtual disk that acknowledges flushes without persisting them voids every durability guarantee.
- New segment files and renames are made durable with a directory fsync (Unix only).
- Commits are applied in `seq` order, and any lost commits are a suffix: if commit `n` survives, so do all commits before it.
- Until step 5 (`Store::open` with recovery), the guarantee is that the records are readable after reopening the files. Replaying them rebuilds the same state. Recovery that does this automatically comes in step 5, and the kill -9 test suite in step 6.

## Failed commits (step 4)

- A commit that fails validation (a conflict, constraint violation, reserved name, a record above 64 MiB, ...) changes nothing, isn't logged, and the namespace stays writable.
- A commit that fails because the log can't be written or fsynced is not applied and not acknowledged. **Its outcome is unknown**: the record may still reach the log and be recovered after a restart. Treat it like a timeout, and retry only if the retry is idempotent (idempotency keys come in step 8).
- After such a failure, the namespace is **read-only until reopened**. Reads keep working and see every applied commit, including group-committed ones whose durability the failure may have cost. Writes fail with a read-only error. A failed fsync is never retried.
- The same holds if applying a logged commit fails. That is a bug, and it poisons the namespace.

## Integrity of the log (step 4)

- Every record and segment header is checksummed (CRC32C over all of its fields and payload).
- A torn or damaged record at the end of the log (from a crash during a write) marks the end of the log. The records before it are intact, and its position is reported for recovery to truncate.
- Damage anywhere else is never skipped silently: in an earlier segment, before a record that proves the damaged one was synced, a gap or a repeat in `seq`, or an unreadable record with a valid checksum. The reader reports it as an error.
- Reading never panics on corrupt input and never allocates more than the file's size for a corrupt length.

## Limits (step 4)

- A commit's WAL record is at most 64 MiB, otherwise the commit is rejected with `RecordTooLarge`.
- Attribute values are nested at most 100 levels deep (`MAX_VALUE_DEPTH`).
- Segment size is 1 KiB to 1 GiB (64 MiB by default). A reader holds one segment in memory at a time.
