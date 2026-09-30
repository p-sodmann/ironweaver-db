# Step 5: Checkpoints, recovery and `Store::open`

Status: done
Milestone: M1 Embedded durable
Depends on: step 4

## Goal

`Store::open(dir)` recovers to the last acknowledged commit after any crash. Checkpoints bound recovery time and WAL size without stalling writers.

## Tasks

- [x] Data-directory layout (`documentation/formats/data-dir.md`): marker file (layout version), lock file, `checkpoints/`, `wal/`. The namespace's catalog lives in each checkpoint's graph meta ([ADR 0003](../adr/0003-catalog-storage.md)).
- [x] Exclusive lock file (like SQLite); a second open fails with a clear error.
- [x] Background checkpointer: keeps its own graph, loads the latest checkpoint, replays WAL segments up to a synced target `seq`, saves with `write_atomic` (binary format, `iwdb.seq` in meta), then deletes WAL segments fully covered by the oldest kept checkpoint. No lock on the live graph.
- [x] Triggers: WAL size, time interval, manual, graceful close.
- [x] Recovery: newest valid checkpoint (fall back to older ones on checksum failure), rebuild indexes from the catalog (`iwdb_engine::codec::from_binary_reader` applies them), replay WAL from `seq + 1`, truncate a torn tail.
- [x] Create `crates/iwdb` (embedded facade): `Store::open`, `commit` (a transaction is a slice of `Mutation`s) and `commit_catalog`, basic reads, `checkpoint`, `close`.
- [x] Group commit timer: the store calls `sync_due` every `max_delay`; `close` syncs.

## Notes from step 4

- Recovery reads the log with `iwdb_storage::WalReader::open(wal_dir, checkpoint_seq + 1)` and feeds each record to `Namespace::replay`. When the iterator ends, `end()` gives the `LogEnd`: the next `seq`, and the last segment's `valid_len` and torn tail, if any.
- **Truncate a torn tail** before writing: `set_len(valid_len)` on the last segment, then fsync it. If `valid_len` is 0 (a damaged header and no records), delete the file and fsync the directory. `Wal::create` refuses a log with a torn tail (`Error::TornTail`), a gap (`LogEndsBefore`) or records at or after its first `seq` (`LogAhead`), and it fsyncs the last segment itself. Then start the writer at `LogEnd::next_seq`, where it creates a new segment.
- `TornTail::discarded_frames > 0` means that a group- or off-mode crash lost records out of order. That is expected within the policy's loss window. Log it.
- Delete stale `*.tmp` files in the WAL directory on open. They are segments whose creation was interrupted, and the reader ignores them.
- **Group commit needs a timer.** The store calls `LoggedNamespace::sync_due` (or `Wal::sync_due`) about every `max_delay`, and `close` syncs. Without a timer, the last commits before an idle period stay unsynced.
- **The checkpointer must only checkpoint synced records** (`seq <= Wal::synced_seq()`). Otherwise an OS crash could leave a checkpoint that is newer than the log, and the writer would reuse seqs, or reading from `checkpoint_seq + 1` would fail with `LogEndsBefore`.
- **Deleting segments.** A segment's last `seq` is the next segment's `first_seq - 1`. Delete segments from the oldest up to the checkpoint, never the segment being written, and fsync the directory afterwards. `WalReader::open(dir, s)` only needs the segments from the one containing `s`, and fails with `MissingRecords` if they are gone.
- **Read-only state.** After a log failure or a poisoned namespace, `LoggedNamespace::read_only()` returns the cause, and every write fails with `Error::ReadOnly`. The store should expose this state, and reopening means running recovery. If a record fails again during replay (`ApplyFailed`), don't truncate the log: report it.
- The reader holds one segment in memory at a time, 64 MiB by default and at most `MAX_SEGMENT_FILE_LEN`.

## Acceptance criteria

- [x] Recovery tests: clean shutdown, crash after append, crash mid-checkpoint, corrupt newest checkpoint (`crates/iwdb/tests/recovery.rs`, plus a random workload with checkpoints, reopens and crashes in `model.rs`).
- [x] Writes keep flowing while a checkpoint runs (measured, below; `checkpoints.rs` also blocks a checkpoint's write and commits meanwhile).

## Changes to the plan

- **"`transaction`"** in the facade became `commit(&[Mutation])`: a transaction is the slice of mutations, as in the engine. A builder can come with the `Database` trait (step 10), which defines the shared API shape.
- **A layout marker** (`IWDB`, with a version) was added to the data directory. Without it, a newer layout or a foreign directory can't be refused before anything is written. It also carries the version of the checkpoint content, because the core's load errors can't tell a newer file from a damaged one ([ADR 0006](../adr/0006-checkpoints-and-recovery.md)).
- **Engine**: `GraphMeta` gained `seq` (`iwdb.seq`, required on load), and `Namespace::from_loaded` builds a namespace from a loaded checkpoint (there was only `Namespace::new`).
- **Storage**: `WalReader::open_until` (read up to a seq, for the checkpointer on the active segment), `Wal::appended_bytes` (size trigger), and `LogFs` gained `write_atomic`, `remove_file` and `truncate` as fault injection seams.
- **Upstream**: `write_atomic` ignores a failed directory fsync; filed as [#32](https://github.com/p-sodmann/Ironweaver/issues/32), worked around.

## Results

Tests (`cargo test --workspace`): `crates/iwdb/tests/recovery.rs` (acceptance cases and every refusal case), `data_dir.rs` (layout, lock across threads and processes, `kill -9`), `checkpoints.rs` (failures through `LogFs`, the WAL cut, triggers, the group timer, commits during a blocked checkpoint), `model.rs` (proptest: random commits, checkpoints, clean reopens, crashes and torn frames, compared with the reference after every reopen), `data_dir_fixture.rs` (layout 1 fixture), and `crates/iwdb-storage/tests/wal_until.rs` (the bounded reader, also while a writer appends).

**Commit latency during a checkpoint** (`cargo test --release -p iwdb --test latency -- --ignored --nocapture`; Apple silicon laptop, macOS, internal SSD, 2026-09-30). The graph has 500,000 nodes (3 attributes each) and 500,000 edges; the checkpoint file is 69 MiB. The checkpoint is the checkpointer's first run: streaming load of the last checkpoint, WAL replay, save. Commits are single-node upserts from one thread.

| Policy | Checkpoint time | Commits | p50 | p99 | p99.9 | max |
|---|---|---|---|---|---|---|
| `always`, outside a checkpoint | | 2,000 | 3,988 µs | 4,249 µs | 10,647 µs | 12,372 µs |
| `always`, during the checkpoint | 15.7 s | 4,402 | 3,881 µs | 4,334 µs | 10,823 µs | 17,721 µs |
| `group` 10 ms, outside a checkpoint | | 2,000 | 3 µs | 8 µs | 89 µs | 4,137 µs |
| `group` 10 ms, during the checkpoint | 1.6 s | 189,091 | 4 µs | 10 µs | 2,456 µs | 7,597 µs |

Commit latency is the same inside and outside the checkpoint; only the tail grows slightly (disk contention). Under `always`, the commits' `F_FULLFSYNC` calls (about 250 per second) slow the checkpoint's own write and fsync down tenfold, not the other way round.

## Notes for step 6

See [step_6.md](step_6.md#notes-from-step-5).

## Non-goals

- Systematic fault injection (step 6), backup/PITR (step 7).
