# Step 8: Concurrency, idempotency and read-your-writes

Status: todo
Milestone: M2 Service
Depends on: step 7

## Goal

Many readers run concurrently with the single writer and always see a consistent state. Clients can retry safely and read their own writes.

## Tasks

- [ ] `RwLock<Graph>` per namespace; commits hold the write lock only for apply + index flush (resolve and WAL append happen before). ADR for the choice and the upgrade path (left-right double buffer replaying the same ops).
- [ ] Analytics: build a `Projection` under a short read lock, run the algorithm without the lock.
- [ ] Requests run under a `cancel::Token` on blocking threads; a timer cancels them at their deadline.
- [ ] Idempotency keys: bounded table of recent keys → `CommitResult`, persisted through the WAL, survives restarts.
- [ ] `min_seq` on reads: wait (with timeout) until that `seq` is applied.
- [ ] Stress tests: readers never observe partial transactions; lock hold times measured.

## Notes from step 7

- **Locks in the store.** `Store` has two mutexes: the live namespace (commits and, for now, reads) and the checkpointer. A backup takes the checkpointer's, then the live one for its fsync only, and holds the checkpointer's during its copy. Nothing may take the checkpointer's mutex while holding the live namespace's: keep that order when reads move to an `RwLock<Graph>` in this step. ADR 0009 has the waits: commits wait for the fsync, checkpoints for the copy.
- **Idempotency keys through the WAL** need a record kind or a field: WAL format 2 (step 7) added a commit time to the frame, and the next frame or payload change is format 3 (version bump, a reader for format 2, a `wal-v3` fixture, `wal.md`). A key table rebuilt by replay must also come out of a restore: restore replays the WAL into a namespace and writes one checkpoint (ADR 0009), so whatever the key table needs must be in the checkpoint (the graph meta, which is a layout change: layout 3) or rebuilt from the records after it. `verify` should then check the table against the records too.
- **`CommitResult` and commit times.** The WAL writer gives every record a commit time (`CommitTime`, non-decreasing). Returning it in `CommitResult` (so a client can restore to just before its own commit) needs the writer to report it from `Wal::append`.
- **`min_seq` and backups.** A backup reaches the synced seq at its start; a client that wants its last commit in a backup either commits with `always` or calls `sync` first. The same holds for read-your-writes across a restore: a restored store has a new history, so a `min_seq` from the old one means nothing there (compare history ids).
- **Python.** The bindings release the GIL around every call that may wait, and serialize with a `RwLock<Option<Store>>` (calls share it, `close` takes it). Concurrent readers in step 8 need no change there; a read-your-writes `min_seq` argument on reads should be added to the Python API ([python-api.md](../python-api.md)) and to the step 14 client at the same time.
- **The harness** now runs `verify` before every checked recovery, archives most data directories, takes backups in the child's script and restores to random seqs in a child (ADR 0007, addendum). A stress test for concurrent readers can reuse `iwdb_crash::Model::state_at` to check that every read sees the state at some seq.

## Acceptance criteria

- A retried commit with the same idempotency key returns the original result and applies once, also across restarts.
- Read latency stays bounded during long analytics jobs (benchmark).
