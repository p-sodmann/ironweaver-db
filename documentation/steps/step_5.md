# Step 5: Checkpoints, recovery and `Store::open`

Status: todo
Milestone: M1 Embedded durable
Depends on: step 4

## Goal

`Store::open(dir)` recovers to the last acknowledged commit after any crash. Checkpoints bound recovery time and WAL size without stalling writers.

## Tasks

- [ ] Data-directory layout (`documentation/formats/data-dir.md`): lock file, `checkpoints/`, `wal/`. The namespace's catalog lives in each checkpoint's graph meta ([ADR 0003](../adr/0003-catalog-storage.md)).
- [ ] Exclusive lock file (like SQLite); a second open fails with a clear error.
- [ ] Background checkpointer: keeps its own graph, loads the latest checkpoint, replays WAL segments up to a target `seq`, saves with `write_atomic` (binary format, `iwdb.seq` in meta), then deletes WAL segments fully below the checkpoint. No lock on the live graph.
- [ ] Triggers: WAL size, time interval, manual, graceful close.
- [ ] Recovery: newest valid checkpoint (fall back to older ones on checksum failure), rebuild indexes from the catalog (`iwdb_engine::codec::from_binary_reader` applies them), replay WAL from `seq + 1`, truncate a torn tail.
- [ ] Create `crates/iwdb` (embedded facade): `Store::open`, `transaction`, `commit`, basic reads, `checkpoint`, `close`.

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

- Recovery tests: clean shutdown, crash after append, crash mid-checkpoint, corrupt newest checkpoint.
- Writes keep flowing while a checkpoint runs (measured).

## Non-goals

- Systematic fault injection (step 6), backup/PITR (step 7).
