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

## Acceptance criteria

- A retried commit with the same idempotency key returns the original result and applies once, also across restarts.
- Read latency stays bounded during long analytics jobs (benchmark).
