# ADR 0006: Checkpoints, recovery and the data directory lock

Status: accepted
Date: 2026-09-30

## Context

Step 5 makes the store durable across restarts: `Store::open(dir)` recovers from a checkpoint and the WAL (step 4), and checkpoints bound recovery time and WAL size without stalling writers. The format is in [formats/data-dir.md](../formats/data-dir.md). This ADR records the choices behind it.

Relevant facts:

- The core's `write_atomic` writes a temporary file, fsyncs it, and renames it over the target. It then syncs the directory on a best-effort basis and ignores errors (upstream [#32](https://github.com/p-sodmann/Ironweaver/issues/32)).
- The core's streaming loader reports every error as `GraphError::Format(String)`: a checksum mismatch and a newer format version look the same.
- A failed fsync must not be retried (ADR 0005, fsyncgate). The same holds for directories.
- `Wal::synced_seq` lags behind the last append under `group`. After an OS crash, unsynced records can be lost, so a checkpoint that includes them would be newer than the log.
- The WAL reader reads a whole segment and treats damage in the last segment as a torn tail. A frame that the writer is still writing looks like one.
- Our MSRV is 1.85 and the workspace forbids `unsafe`. `std::fs::File::try_lock` is stable from Rust 1.89.

## Decision

### Lock

The exclusive lock is `flock` on `LOCK` through **`fs4`** (MIT/Apache-2.0, MSRV 1.75, built on `rustix`, which is already in our tree through `tempfile`). We don't bump the MSRV to 1.89 for one call: that would drop support for about a year of toolchains, and the swap is local (one function in `layout.rs`) once the MSRV passes 1.89. `fs4`'s trait method names match std's inherent ones (`try_lock`), which take precedence on newer toolchains, so we call it as `fs4::FileExt::try_lock(&file)`. `fs2` is unmaintained, and `fd-lock` ties the lock to a guard's borrow of the file, which fits badly into a long-lived struct.

`flock` rather than POSIX `fcntl` locks: an `fcntl` lock belongs to the process, so a second open in the same process would succeed, and closing any descriptor of the file would release it.

### Directory marker

A 16-byte marker (`IWDBDIR\n`, layout version, CRC32C) is written last during initialization. It makes three cases distinguishable before anything is written: a directory that isn't ours, a newer layout, and an interrupted initialization. The layout version covers the checkpoint content too, because the core's load errors can't tell "newer" from "damaged" (below).

### Checkpoints

- **Content and naming**: the core's binary format with `iwdb.seq` and `iwdb.catalog` in graph meta (ADR 0003), named by seq. It holds exactly the commits up to its seq.
- **Only synced commits.** The target is `min(Wal::synced_seq, applied seq)`. `Store::checkpoint` and `close` sync first, so they cover everything. The background checkpointer doesn't sync, so it never adds an fsync to the commit path. Under `off` nothing is ever synced; there the target is the applied seq, and the risk is the one `off` already accepts.
- **Own namespace, no lock on the live one.** The checkpointer keeps its own `Namespace`, loaded once from the newest checkpoint that loads and brought forward by replaying WAL records, like a replica. It reads the WAL with a bounded reader (`WalReader::open_until`), which stops after the target record and never decodes the bytes after it. So the writer can append to the same segment meanwhile, and a frame in progress is never taken for damage. The live namespace's lock is held only to read the target seq. Cost: a second copy of the graph in memory, kept between runs. Reloading from disk each time would avoid that, at O(graph) load time per checkpoint; step 16 can make this an option if memory matters more.
- **Retention.** Keep the newest `keep` checkpoints (default 2) that aren't known to be damaged, and remove older ones. Keep every WAL segment with a record after the oldest kept checkpoint. So a damaged newest checkpoint can always fall back to the older one. Only when every kept checkpoint is damaged is recovery impossible (`NoUsableCheckpoint`), and then it refuses without changing anything. Damaged checkpoints are not counted and are removed once they are older than the oldest kept one; until then they stay for inspection.
- **Order of durability**: the checkpoint file (fsynced by `write_atomic`), then our own fsync of `checkpoints/` with its result checked (the workaround for #32), then removals of old checkpoints, fsync, removals of WAL segments, fsync. Nothing is removed before the new checkpoint's directory entry is durable.
- **Failures**: before the rename, a failure is retried at the next trigger, because nothing was published. After the rename (a directory fsync, a removal), checkpoints are disabled until the store is reopened. A retried directory fsync could succeed without persisting the entries, and deleting WAL segments on that basis could lose data. Commits are not affected. A replay failure in the checkpointer disables it too.
- **Triggers**: WAL growth (`Wal::appended_bytes`, checked after each commit, which wakes the background thread), an interval, `Store::checkpoint`, and `close`. After a failed run the size trigger still moves forward by `wal_size`, so a failing checkpoint isn't retried on every commit.

### Recovery

- **Fallback on any load failure.** Because the core reports damage and newer versions alike, recovery treats every checkpoint that fails to load as damaged, skips it, and reports it. Newer writers are kept out by the layout version in the marker instead, so a change to the checkpoint content must bump the layout version.
- **Refuse, never repair.** Corruption, missing WAL records, a WAL that ends before the checkpoint, and a record that fails to replay all fail the open with a typed error, and change nothing. The only repairs are those the formats define as the clean end of a crash: removing temporary files and cutting a torn tail.
- **Reporting.** `recover` returns a `RecoveryReport` (checkpoint used, skipped checkpoints, `IndexChanges`, records replayed, the torn tail cut with its `discarded_frames`, temporary files removed). The store logs the unusual items through the `log` facade (already in our tree), and exposes the report (`Store::recovery`). Step 16 adds metrics.

### Store

- One `Mutex` around the live `LoggedNamespace` serializes commits (the single writer) and, for now, reads (step 8 adds concurrent readers). The checkpointer has its own `Mutex`, so a manual and a background checkpoint never run at once.
- Two threads at most: the checkpointer (if a size or time trigger is set) and, under `group`, a timer that calls `sync_due` every `max_delay`. They stop on `close` and on drop.
- **Read-only state**: a WAL failure, a poisoned namespace or a panic during a commit (a poisoned mutex) makes the store read-only until it is reopened. Reads keep working. Reopening runs recovery.
- Dropping the store without `close` stops the threads and releases the lock, but doesn't sync or checkpoint. It behaves like a process crash, so tests can use a drop as a crash.

## Consequences

- Recovery reaches every acknowledged commit that the fsync policy made durable, with the checkpoint and WAL files checked end to end, and each refusal case leaves the files untouched. The tests in `crates/iwdb/tests` cover each case.
- Commit latency doesn't change while a checkpoint runs. Measured in step 5 (step_5.md): p50 and p99 are the same inside and outside a checkpoint of a 69 MiB graph. The checkpointer costs a second copy of the graph in memory.
- A checkpoint can lag behind the last commit by up to the group commit window. That doesn't matter: recovery replays the rest from the WAL.
- A directory fsync failure turns checkpoints off until the next open, so the WAL grows until then. `Store::checkpoint_failure` reports it; step 16 turns it into a health signal.
- Once upstream #32 is fixed, the extra directory sync after `write_atomic` can go. Once the MSRV reaches 1.89, `fs4` can be replaced by std.
