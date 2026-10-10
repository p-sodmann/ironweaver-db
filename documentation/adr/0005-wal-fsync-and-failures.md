# ADR 0005: WAL fsync policies and failure behaviour

Status: accepted
Date: 2026-09-30

## Context

Step 4 adds the write-ahead log. A commit is resolved and validated, appended to the log, fsynced according to a policy, applied in memory and then acknowledged. Four questions need an answer that the guarantees ([guarantees.md](../guarantees.md)) and the format ([formats/wal.md](../formats/wal.md)) can rely on:

1. What does each fsync policy promise, and when does group commit fsync, given a single synchronous writer and no background thread yet?
2. What happens when a write or fsync fails?
3. How does the reader tell a torn write at the end of the log from corruption, when unsynced data can reach the disk out of order?
4. Does `File::sync_all` actually flush to stable storage on macOS?

Relevant facts:

- **fsyncgate** (PostgreSQL, 2018): after a failed `fsync`, Linux may drop the dirty pages and mark them clean. A retried `fsync` then succeeds without writing them, so a retry "succeeds" and the data is lost silently. PostgreSQL now PANICs on fsync failure and recovers from its WAL.
- **Write-back order**: pages written but not synced reach the disk in any order. After an OS crash or power loss, a later page of the file can survive while an earlier one is lost or zeroed (ext4 with delayed allocation, XFS). Within one fsync batch the survivors are arbitrary.
- **macOS**: `fsync` doesn't flush the drive's cache; `fcntl(F_FULLFSYNC)` does. The standard library's `File::sync_all` and `sync_data` call `fcntl(fd, F_FULLFSYNC)` on Apple targets. We checked this in the std source of our toolchain (1.97.1, `library/std/src/sys/fs/unix.rs`) and of our MSRV (1.85.0, `library/std/src/sys/pal/unix/fs.rs`). On Linux they call `fsync` and `fdatasync`.

## Decision

### Fsync policies

- **`always`**: every append is fsynced before `append` returns, so before the commit is applied and acknowledged. An acknowledged commit survives any crash (process, OS, power), given hardware that honours flushes.
- **`group { max_delay, max_batch }`**: like Redis `appendfsync everysec`, batched by **both time and count**, without a background thread. After writing its record, an append fsyncs the log before it returns if, counting that record, at least `max_batch` records are unsynced, or the oldest unsynced record was written at least `max_delay` ago. Otherwise it returns at once. The commit that closes a batch pays for the fsync and makes its whole batch durable.
  - Without later appends, the last records of a burst would stay unsynced indefinitely. So `Wal::sync_due()` syncs records older than `max_delay`. Its owner calls it on a timer: in step 4 the tests do, and from step 5 the store runs a timer thread. `Wal::sync()` syncs at once, and `close()` syncs.
  - What an OS crash can lose: acknowledged commits that were written after the last completed fsync. They form a suffix of the log, fewer than `max_batch` commits, all written within `max_delay` of each other. With `sync_due` called every `P`, every commit older than `max_delay + P` is durable. A process crash alone loses nothing, because written data is in the OS page cache.
- **`off`** (tests only): no fsync at all, not even when a segment is created or rotated. A process crash loses nothing. After an OS crash, anything since the files were created may be lost, and the log may be damaged in ways recovery refuses (damage in an earlier segment). Only an explicit `Wal::sync` fsyncs, and then everything: every segment that may hold unsynced records, and the directory. A writer under `off` starts with `synced_seq` 0, because it can't know whether an earlier writer's records were synced (step 6 fixed both; before, a sync covered only the current segment, and a new writer claimed the whole log synced).
- Every policy fsyncs **around segment creation** (except `off`): the new segment's header is written to a temporary file, fsynced, renamed into place and the directory fsynced. Before rotating, the old segment is fsynced. So a segment file exists only with a valid header, and only the last segment can have a torn tail. When a writer starts, it fsyncs the existing last segment, which makes true the claim in its first frame that everything before it is synced.
- **Directory fsync** is `File::open(dir)?.sync_all()` on Unix (on macOS, `F_FULLFSYNC` on the directory, which works). It is a no-op on non-Unix targets, which aren't supported yet. *Update, step 16h ([ADR 0058](0058-windows.md)): on Windows it is `FlushFileBuffers` on the directory, opened with `FILE_FLAG_BACKUP_SEMANTICS` and write access (no unsafe code), on NTFS or ReFS; its error fails the operation as on Unix. The core's `write_atomic` didn't sync the directory on Windows (upstream [#71](https://github.com/p-sodmann/Ironweaver/issues/71)), so `StdFs::write_atomic` did it there until the core did, in `73d8fab`.*

### Failures

- If a write, fsync, rename, directory fsync or segment creation fails, the `Wal` becomes **failed**, and the commit is not applied. Every later append, `sync`, `sync_due` and `close` returns `Error::ReadOnly` without touching the files. A failed fsync is **never retried** (fsyncgate). `LoggedNamespace` (and later the store) is then **read-only until reopened**: reads keep working and see every applied commit, and writes fail. Reopening means recovery from checkpoint and log (step 5).
- **The failed commit's outcome is unknown.** It was not applied in memory and not acknowledged. Its record may nevertheless be complete in the log: if the fsync failed, the page cache may still write it back, and recovery will then find it. Clients see an I/O error and must treat it like a timeout. Idempotency keys (step 8) make a retry safe.
- Under `group`, a failed fsync also means that the unsynced, already acknowledged commits of the batch may be lost. That is within the policy's documented loss window.
- If applying a logged record fails (`iwdb_engine::Error::ApplyFailed`, which poisons the namespace), the namespace is read-only in the same way. The record is in the log, so recovery replays it. If it fails again there, recovery reports it (step 5).
- Errors that happen before anything is written leave the log usable, and the commit is rejected cleanly: a record above `MAX_RECORD_LEN` (64 MiB) returns `Error::RecordTooLarge`, as does an encoding error or an out-of-order seq.

### Telling a torn tail from corruption: `synced_seq`

Each frame carries `synced_seq`: the highest seq whose fsync had completed when the frame was written. Damage in the last segment is a torn tail unless a later valid frame has `synced_seq` at or above the damaged record's seq. Such a frame proves the damaged record had been synced, so the damage is corruption and an error. Frames written before that sync can be lost or survive in any order, and are discarded with the torn tail. This adds 8 bytes per record. Without it, the reader would have to choose between two bad options:

- *any valid data after damage is corruption*: a group-mode log could then be unrecoverable after a power loss, although nothing outside the documented loss window was lost;
- *the first damage ends the log* (PostgreSQL's approach): corruption in the middle of a synced log would silently drop every later commit.

## Consequences

- The guarantee per policy is precise and testable. `always` is the default. Step 6's kill -9 harness checks it, and step 14 benchmarks each policy's throughput.
- Group commit needs a timer to bound the loss window in time. The store (step 5) owns it. Until then, a `LoggedNamespace` user must call `sync_due` or `sync`.
- A failed fsync or write takes the namespace out of service until it is reopened. That is deliberate: memory and log may disagree, and continuing would acknowledge commits whose predecessors may be lost.
- The file-system calls go through a small trait (`iwdb_storage::io::LogFs` / `LogFile`). Tests inject failures through it today, and step 6 places failpoints on the same calls.
- Records carry 25 bytes of framing. A record over 64 MiB can't be committed; bulk import (step 13) must split large loads.
