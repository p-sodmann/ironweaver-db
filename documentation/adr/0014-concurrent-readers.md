# ADR 0014: Concurrent readers: a writer mutex and a reader/writer lock per namespace

Status: accepted
Date: 2026-10-01

## Context

Up to step 7, `Store` kept the live `LoggedNamespace` (namespace and WAL) behind one `Mutex`. Every read waited for a whole commit, including its WAL fsync (milliseconds under `always`), and a long read blocked every commit. Step 8 asks for many readers running concurrently with the single writer, without ever seeing part of a transaction, and with analytics that don't hold a lock while they run.

Constraints:

- Design rule 2: one commit pipeline, one writer. The order prepare (resolve and validate) → WAL append and fsync → apply stays.
- ADR 0008: a panic while the store changes its namespace or WAL aborts the process.
- ADR 0009: the lock order is checkpointer → live; a backup holds the checkpointer's lock across its copy.
- The core's graph is a plain `Graph<N, E>` with no internal synchronization. `Projection::collect` reads it, `RawProjection::finish` and every algorithm work on the projection alone (the core split them for this purpose).

Options:

1. **`RwLock<Namespace>` plus a writer mutex.** A commit takes the writer mutex (the WAL) for its whole duration, prepares under the namespace's read lock, appends and fsyncs with no namespace lock, and takes the write lock only to apply the record and flush the indexes.
2. **Left-right (double buffer).** Two copies of the namespace; readers read one while the writer applies to the other, then the roles swap and the writer replays the same ops on the old copy. Readers never wait. The cost is twice the memory and a second apply of every record.
3. **Copy-on-write snapshots (`Arc<Namespace>` swapped per commit).** Needs a persistent graph structure; the core's isn't one, and cloning the graph per commit is O(graph).
4. **MVCC in the core.** Out of scope for this database (design rule 9: don't reimplement the core).

## Decision

Option 1, with std's `RwLock` and `Mutex` (no new dependency; `parking_lot` would add little).

- `LoggedNamespace` (iwdb-storage) owns the locks and is shared by `&self`. `commit` holds the WAL mutex from start to end, so commits are serialized and none can prepare against a state that another commit is about to change. Readers (`read`, `namespace`) take the read side. The write side is held for `Namespace::apply` only: the core's `apply_all` and the index flush. So a reader waits at most for one apply, never for validation or an fsync, and sees the state after some commit (the write lock covers all of a record's ops).
- The applied seq is published in an atomic and a condition variable after each apply, for `seq()` without a lock and for `min_seq` waits (ADR 0016).
- Lock order: checkpointer → WAL mutex → namespace lock. Nothing takes the WAL mutex while holding the namespace lock (`target()` reads the atomic seq, then the WAL). The checkpointer replays the WAL into its own namespace (ADR 0006) and takes neither lock except to read the target. A backup takes the checkpointer's lock, then the WAL mutex for its fsync (ADR 0009, unchanged).
- ADR 0008 holds: every change runs in `or_abort`. A panic in apply would poison the write lock, but the process aborts first. A panic in a reader's closure doesn't poison an `RwLock` (std poisons on panics under the write lock only), so it changes nothing.
- **Read-only state** is mirrored outside the WAL mutex, so asking whether the store is read-only never waits for an fsync.
- **Analytics** (`Store::analyze`) call `Projection::collect` under the read lock (O(n + m), no sorting), release it, then `finish` and run the algorithm without a lock, under a cancel token (ADR 0016). Commits and reads continue during the job; the job sees the snapshot at the seq it reports.
- **Measurement**: `LoggedNamespace::lock_stats` counts write-lock acquisitions and their total and longest hold time (`Store::lock_stats`). Step 16 turns it into a metric.

`Store::read(f)` still runs `f` under the read lock. A long `f` delays commits (they wait to apply), and with a writer-preferring lock, readers that arrive while a commit waits wait too. The docs say so; long work belongs in `analyze`, and step 10's query layer bounds every read.

## Upgrade path: left-right

If apply latency or long reads become a problem (measured with `lock_stats` and the step 14 benchmarks), the next step is a left-right double buffer, which fits this design without changing the WAL or the commit order:

- two `Namespace` copies; an epoch counter tells readers which one to read; the writer applies a record to the other copy, flips the epoch, waits for readers of the old copy to leave, and **replays the same record** onto it. Records are already exact replays (resolved ops with explicit ids, ADR 0004; deterministic, design rule 7), so both copies stay identical, and verify's `invariants::compare` can check that in tests;
- readers never wait for a writer; the writer waits for readers of the old copy, so a long read delays the next commit rather than other reads;
- the cost is a second copy of the graph (the checkpointer already keeps one, ADR 0006; the two could be merged) and a second apply per commit.

`LoggedNamespace::read` and `namespace` are the only read entry points, so the change stays inside iwdb-storage.

## Consequences

- Reads wait only for applies: measured in `crates/iwdb/tests/concurrency.rs` (lock hold times, read latency during commits and during a long analytics job).
- Readers never see a partial transaction: the stress test checks every read against the reference state at its seq.
- Commit throughput is unchanged (one writer); a commit with many concurrent readers may wait for them to leave before it applies.
- Step 9 gives each namespace its own pair of locks.
