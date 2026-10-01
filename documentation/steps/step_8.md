# Step 8: Concurrency, idempotency and read-your-writes

Status: done
Milestone: M2 Service
Depends on: step 7

## Goal

Many readers run concurrently with the single writer and always see a consistent state. Clients can retry safely and read their own writes.

## Tasks

- [x] `RwLock<Graph>` per namespace; commits hold the write lock only for apply + index flush (resolve and WAL append happen before). ADR for the choice and the upgrade path (left-right double buffer replaying the same ops). `LoggedNamespace` holds a writer mutex (the WAL) and an `RwLock<Namespace>` ([ADR 0014](../adr/0014-concurrent-readers.md)).
- [x] Analytics: build a `Projection` under a short read lock, run the algorithm without the lock (`Store::analyze`).
- [x] Requests run under a `cancel::Token` on blocking threads; a timer cancels them at their deadline (`ReadOptions`, the store's timer thread, [ADR 0016](../adr/0016-read-your-writes-and-deadlines.md)).
- [x] Idempotency keys: bounded table of recent keys → `CommitResult`, persisted through the WAL, survives restarts (and checkpoints, backups, restores): WAL format 3, data-dir layout 3 ([ADR 0015](../adr/0015-idempotency-keys.md)).
- [x] `min_seq` on reads: wait (with timeout) until that `seq` is applied (`Store::read_with`, `wait_for_seq`; Python `min_seq=` and `timeout=`).
- [x] Stress tests: readers never observe partial transactions; lock hold times measured (`crates/iwdb/tests/concurrency.rs`, `Store::lock_stats`).

## Notes from step 7

- **Locks in the store.** `Store` has two mutexes: the live namespace (commits and, for now, reads) and the checkpointer. A backup takes the checkpointer's, then the live one for its fsync only, and holds the checkpointer's during its copy. Nothing may take the checkpointer's mutex while holding the live namespace's: keep that order when reads move to an `RwLock<Graph>` in this step. ADR 0009 has the waits: commits wait for the fsync, checkpoints for the copy.
- **Idempotency keys through the WAL** need a record kind or a field: WAL format 2 (step 7) added a commit time to the frame, and the next frame or payload change is format 3 (version bump, a reader for format 2, a `wal-v3` fixture, `wal.md`). A key table rebuilt by replay must also come out of a restore: restore replays the WAL into a namespace and writes one checkpoint (ADR 0009), so whatever the key table needs must be in the checkpoint (the graph meta, which is a layout change: layout 3) or rebuilt from the records after it. `verify` should then check the table against the records too.
- **`CommitResult` and commit times.** The WAL writer gives every record a commit time (`CommitTime`, non-decreasing). Returning it in `CommitResult` (so a client can restore to just before its own commit) needs the writer to report it from `Wal::append`.
- **`min_seq` and backups.** A backup reaches the synced seq at its start; a client that wants its last commit in a backup either commits with `always` or calls `sync` first. The same holds for read-your-writes across a restore: a restored store has a new history, so a `min_seq` from the old one means nothing there (compare history ids).
- **Python.** The bindings release the GIL around every call that may wait, and serialize with a `RwLock<Option<Store>>` (calls share it, `close` takes it). Concurrent readers in step 8 need no change there; a read-your-writes `min_seq` argument on reads should be added to the Python API ([python-api.md](../python-api.md)) and to the step 14 client at the same time.
- **The harness** now runs `verify` before every checked recovery, archives most data directories, takes backups in the child's script and restores to random seqs in a child (ADR 0007, addendum). A stress test for concurrent readers can reuse `iwdb_crash::Model::state_at` to check that every read sees the state at some seq.

## Decisions

| Question | Decision | Recorded in |
|---|---|---|
| Reader/writer scheme | `LoggedNamespace` is shared by `&self`: a mutex around the WAL held for the whole commit (one writer), an `RwLock<Namespace>` whose read side covers prepare and reads, and whose write side covers only apply and the index flush. std locks, no new dependency. Lock order checkpointer → WAL → namespace; ADR 0008's abort unchanged (a reader's panic doesn't poison an `RwLock`) | ADR 0014 |
| Upgrade path | Left-right: two namespace copies, the writer applies to the hidden one, flips, and replays the same record onto the other; records are exact replays, so the copies stay equal. Inside `LoggedNamespace` only | ADR 0014 |
| Analytics | `Projection::collect` under the read lock, `finish` and the job without it, under a cancel token; the result carries the seq it ran on | ADR 0014 |
| Where keys live | In the namespace: a `KeyTable` changed only by applying records, so every replay path rebuilds it; the record carries the key and the result (edge ids, versions); checkpoints save the table (`iwdb.keys`) | ADR 0015 |
| WAL format 3 | Payload `postcard((Option<Keyed>, body))`; frame unchanged; readers for formats 1 and 2; fixture `wal-v3` | ADR 0015, [wal.md](../formats/wal.md) |
| Data-dir layout 3 | `iwdb.keys` in checkpoint graph meta (a JSON string); marker version 3; layout 2 upgraded on open with its history id; fixture `data-dir-v3` | ADR 0015, [data-dir.md](../formats/data-dir.md) |
| Table size and eviction | 10 000 keyed commits (a format constant, so replay doesn't depend on configuration); the lowest seq is evicted | ADR 0015 |
| Same key, other request | `IdempotencyKeyReused`, nothing changes; requests compared by fingerprint (CRC32C of the postcard encoding, maps sorted) | ADR 0015 |
| Keys across a restore | Kept for the commits up to the restored seq (they are in its checkpoint), none after; a retry of a commit after it applies | ADR 0015, guarantees.md |
| Commit time in `CommitResult` | Yes: `Wal::append` returns it; `CommitTime` moved to iwdb-engine; a retry returns the original time | ADR 0015 |
| `min_seq` | Wait on a condition variable for the applied seq; `Timeout` at the deadline (default 30 s); `ReadOnly` at once on a read-only store below the seq | ADR 0016 |
| `min_seq` of another history | Optional `history` with `min_seq`; another history fails with `OtherHistory` | ADR 0016 |
| Deadlines and cancellation | `ReadOptions { timeout, cancel }`; one timer thread per store (started on first use) cancels tokens at their deadline; jobs run under `cancel::run` on the caller's thread; commits take no timeout yet | ADR 0016 |
| Python | `transaction(idempotency_key=)` and on catalog methods; `time` and `deduplicated` in results; `min_seq=` and `timeout=` on `node`, `edge`, `catalog`; `wait_for_seq`; `iwdb.TimeoutError`; GIL released while waiting | [python-api.md](../python-api.md) |

## Format changes

- **WAL format 3** (`FORMAT_VERSION` 3, `READ_VERSIONS` [1, 2, 3]): the payload starts with the optional idempotency key and result. Fixture `crates/iwdb-storage/tests/fixtures/wal-v3/` (two keyed records); `wal-v1` and `wal-v2` still read (as records without keys), and a format 2 log continues in format 3 (test).
- **Data-dir layout 3** (`LAYOUT_VERSION` 3): checkpoints have `iwdb.keys`. Fixture `crates/iwdb/tests/fixtures/data-dir-v3/` (keyed commits before and after its checkpoint, a retry answered from the table); `data-dir-v1` and `data-dir-v2` open and are upgraded (layout 2 keeps its history id). Backups and archives keep their own formats (manifest version 1, archive version 1); a backup of a layout 3 store has a layout 3 marker.

## Acceptance criteria

- [x] A retried commit with the same idempotency key returns the original result and applies once, also across restarts: engine tests (`crates/iwdb-engine/tests/idempotency.rs`, with a replay and checkpoint round-trip property), store tests across restarts, checkpoints, a backup and restore, failed WAL writes and fsyncs (`crates/iwdb/tests/idempotency.rs`), Python tests (`test_step8.py`), and the kill -9 harness, whose children retry the keyed commits the killed child tried last.
- [x] Read latency stays bounded during long analytics jobs (timed test `reads_and_commits_go_on_during_a_long_analytics_job`, Results).

## Bugs found in steps 4 to 7

None found. (The step 4 test `a_poisoned_namespace_is_read_only` caught a gap in this step's own rewrite of `LoggedNamespace`, fixed before it was committed.)

## Findings in ironweaver-core

None new. `Projection::collect` doesn't check the cancel token, so the part of an analytics job that holds the read lock can't be interrupted; it is O(n + m) and its time is measured, so no change is asked for now. Open issues: #26–#33 ([upstream-check.md](upstream-check.md)).

## Results

**Long run** (`iwdb-crash --policy all --seeds 2 --cycles 1000`, seeds 922776117 and 922776118, release build, Apple silicon laptop, macOS, 2026-10-01). All six policy/seed runs passed: **6000 kill/recover cycles, no lost acknowledged commit, no partial transaction, no key applied twice**. The cycles took 1164 s (19.4 minutes); the run took 73 minutes of wall time, most of it spent waiting for fsyncs (4 % CPU).

| Policy, seed | Time | Recoveries checked (+ by digest) | Keyed commits tried / answered from the table | OS crashes (unsynced acks lost) | verify before recovery | Archives verified | Backups complete and restored / interrupted and refused | Restores in a child: complete / interrupted (killed at a failpoint) |
|---|---|---|---|---|---|---|---|---|
| `always` 922776117 | 214 s | 752 (+972) | 7890 / 1258 | 11 (0) | 752 | 752 | 293 / 160 | 93 / 95 (71) |
| `group` 922776117 | 200 s | 756 (+965) | 9062 / 1195 | 66 (77) | 756 | 562 | 368 / 175 | 63 / 90 (67) |
| `off` 922776117 | 159 s | 801 (+982) | 7908 / 1196 | 178 (195) | 805 | 795 | 289 / 143 | 92 / 129 (89) |
| `always` 922776118 | 222 s | 754 (+964) | 8051 / 1301 | 8 (0) | 754 | 641 | 306 / 155 | 80 / 97 (67) |
| `group` 922776118 | 204 s | 763 (+967) | 8892 / 1223 | 54 (55) | 763 | 650 | 382 / 171 | 82 / 103 (76) |
| `off` 922776118 | 164 s | 761 (+984) | 8448 / 1226 | 190 (225) | 767 | 625 | 336 / 160 | 78 / 94 (68) |

In all: 77749 acknowledged commits; 50251 keyed commits tried, of which 7399 were retries of a commit a killed child had tried, answered from the key table (after restarts, checkpoints and torn tails) with the original result. Unsynced acknowledged commits are lost only under `group` and `off`, as documented; their keys are lost with them and the retry applies. verify ran 4597 times before a recovery, 4025 archives were verified, 1974 backups were verified and restored, 964 interrupted ones were refused, and 488 restores to random seqs matched the model while 608 interrupted ones were refused.

**Short run** (as CI, `--policy all --cycles 150`): 450 cycles in 74 s (`always` 28.0 s, `group` 26.1 s, `off` 20.1 s); 3415 keyed commits tried, 545 answered from the table.

**Concurrency** (`crates/iwdb/tests/concurrency.rs`, debug build, same laptop):
- Stress: 11429 reads by 6 readers saw 566 different seqs of 565 commits, each equal to the reference state at its seq (never part of a transaction). The write lock was held 565 times, 24.7 ms in total, at most 199 µs.
- During a 1.50 s analytics job: 913262 reads (p50 667 ns, p99 1.4 µs, max 263 µs) and 45663 commits (max 567 µs); the write lock was held at most 3.1 ms. Up to step 7 a read waited for every commit, fsync included (ADR 0014).
- `min_seq`: the waiting read returned 9 µs after the commit it waited for.

**`cargo test --workspace`**: 108 s of test time (about 100 s in step 7), 3 min 22 s of wall time with the build: `crash_points.rs` 25 s, `model.rs` 11 s, `backup.rs` 9 s, `faults.rs` 9 s, `pitr.rs` 7 s, `archive.rs` 6 s; the new `concurrency.rs` 2.0 s, `idempotency.rs` 1.2 s (store) and 0.3 s (engine). Also clean: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo deny check`, and the build with Rust 1.85 (`--locked`).

**Python** (`pytest crates/iwdb-python/tests`, 78 tests, 11 of them new in `test_step8.py`): passed on macOS arm64 in 12 s (6 s without a concurrent cargo build). CI ran on the pushed commits and passed (4 min 24 s).

## Notes for step 9

See [step_9.md](step_9.md#notes-from-step-8).

## Non-goals

- Several namespaces and catalog operations (step 9), the `Database` trait, query layer and bounded traversals (step 10), servers and the remote client (steps 11–14).
- Timeouts on commits; a configurable key table size.
