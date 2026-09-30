# Step 5: Checkpoints, recovery and `Store::open`

Status: todo
Milestone: M1 Embedded durable
Depends on: step 4

## Goal

`Store::open(dir)` recovers to the last acknowledged commit after any crash. Checkpoints bound recovery time and WAL size without stalling writers.

## Tasks

- [ ] Data-directory layout (`documentation/formats/data-dir.md`): lock file, `checkpoints/`, `wal/`, catalog.
- [ ] Exclusive lock file (like SQLite); a second open fails with a clear error.
- [ ] Background checkpointer: keeps its own graph, loads the latest checkpoint, replays WAL segments up to a target `seq`, saves with `write_atomic` (binary format, `iwdb.seq` in meta), then deletes WAL segments fully below the checkpoint. No lock on the live graph.
- [ ] Triggers: WAL size, time interval, manual, graceful close.
- [ ] Recovery: newest valid checkpoint (fall back to older ones on checksum failure), rebuild indexes from the catalog, replay WAL from `seq + 1`, truncate a torn tail.
- [ ] Create `crates/iwdb` (embedded facade): `Store::open`, `transaction`, `commit`, basic reads, `checkpoint`, `close`.

## Acceptance criteria

- Recovery tests: clean shutdown, crash after append, crash mid-checkpoint, corrupt newest checkpoint.
- Writes keep flowing while a checkpoint runs (measured).

## Non-goals

- Systematic fault injection (step 6), backup/PITR (step 7).
