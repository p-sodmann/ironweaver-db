# Guarantees

What Ironweaver DB promises, and under which conditions. This document grows with each step. Step 17 turns it into the user-facing guarantees page. Sections are marked with the step that provides them. Anything not listed here is not guaranteed.

## Durability of commits (step 4)

Every commit is appended to the write-ahead log ([format](formats/wal.md)) before it is applied in memory and acknowledged. When it becomes durable depends on the fsync policy ([ADR 0005](adr/0005-wal-fsync-and-failures.md)):

| Policy | Acknowledged commits lost on a process crash | Acknowledged commits lost on an OS crash or power loss |
|---|---|---|
| `always` (default) | none | none |
| `group { max_delay, max_batch }` | none | only the commits after the last completed fsync: a suffix of the log, fewer than `max_batch` commits, written within `max_delay` of each other. With `sync_due` called every `P`, no commit older than `max_delay + P`; the store's timer uses `P = max_delay`, so no commit older than `2 × max_delay` |
| `off` (tests only) | none | anything not covered by an explicit sync (`Store::sync`, `checkpoint`, `close`), which fsyncs every segment with unsynced records and the directory. Recovery may also refuse to open the log: with `LogEndsBefore` if a checkpoint is newer than what is left of the log, and with a corruption error if the loss hit an earlier segment, or a segment's header while later frames survived |

Conditions:

- The storage honours flushes. On macOS, `fsync` alone doesn't flush the drive's cache; we use `File::sync_all`, which issues `F_FULLFSYNC` there. On Linux it is `fsync`. A drive or virtual disk that acknowledges flushes without persisting them voids every durability guarantee.
- New segment files and renames are made durable with a directory fsync (Unix only).
- Commits are applied in `seq` order, and any lost commits are a suffix: if commit `n` survives, so do all commits before it.
- `Store::open` recovers every commit in the log (step 5, below). The kill -9 harness checks this across thousands of crashes per policy (step 6, below).
- **Group commit timer (step 5).** The store calls `sync_due` every `max_delay` in a background thread, so `P = max_delay`: after an OS crash, no acknowledged commit older than about `2 × max_delay` is lost (plus the time the fsync itself takes, and scheduling delays). `Store::close` syncs everything; dropping a store without `close` doesn't.

## Failed commits (steps 4 and 5)

- A commit that fails validation (a conflict, constraint violation, reserved name, a record above 64 MiB, ...) changes nothing, isn't logged, and the namespace stays writable.
- A commit that fails because the log can't be written or fsynced is not applied and not acknowledged. **Its outcome is unknown**: the record may still reach the log and be recovered after a restart. Treat it like a timeout, and retry with an idempotency key (step 8, below): then the retry applies it exactly once.
- After such a failure, the namespace is **read-only until reopened**. Reads keep working and see every applied commit, including group-committed ones whose durability the failure may have cost. Writes fail with a read-only error. A failed fsync is never retried.
- The same holds if applying a logged commit fails. That is a bug, and it poisons the namespace.
- The store exposes this state (`Store::read_only`, step 5). Reopening the store runs recovery and makes it writable again.
- **A panic while the store changes its namespace or WAL** (a commit, an fsync, the group commit timer) aborts the process (step 6, [ADR 0008](adr/0008-panics-in-the-commit-path-abort.md)). It is a crash: no reader sees a partially applied transaction, and the next open recovers every logged commit (the one in flight like a failed fsync: its outcome is unknown). A panic in a `Store::read` closure changes nothing and leaves the store writable.
- **An apply that fails inside the core** with `GraphError::Internal` (a failed rollback, or a graph the core found inconsistent) aborts the process the same way (step 11, [ADR 0028](adr/0028-internal-apply-errors-abort.md)): the graph may hold part of the transaction. Any other apply failure was rolled back by the core: the namespace becomes read-only, as after a WAL failure, and reads see the state before the commit.

## Concurrency, idempotency and read-your-writes (step 8)

[ADR 0014](adr/0014-concurrent-readers.md), [ADR 0015](adr/0015-idempotency-keys.md), [ADR 0016](adr/0016-read-your-writes-and-deadlines.md).

**Readers and the writer.**

- Commits are serialized (one writer per namespace). Reads run concurrently with each other and with a commit's validation, WAL append and fsync; they wait only while a commit applies its record and flushes the indexes (the namespace's write lock). Measured (`crates/iwdb/tests/concurrency.rs`, debug build, this machine): the write lock is held at most a few milliseconds for small commits; during a 1.5 s analytics job with commits going on, the slowest of ~900 000 reads took 0.27 ms.
- **A read never sees part of a transaction**: it sees the state after some commit, all of whose ops are applied, and the seq it reports is that commit's. Checked by readers against a committing writer, every read compared with the reference state at its seq.
- A long `Store::read` closure delays commits (they wait to apply) and the reads queued behind them; long work belongs in `Store::analyze`, which copies the graph into a `Projection` under the read lock (O(n + m)) and runs the job without any lock, on the state at the seq it reports.

**Idempotency keys.**

- A commit with an idempotency key (1 to 255 bytes) applies **at most once**: a retry with the same key and the same request returns the original result (seq, edge ids, versions, commit time) with `deduplicated` set and commits nothing; a retry with the same key and a different request fails with `IdempotencyKeyReused` and changes nothing. So a commit whose outcome is unknown (an `Io` error, a timeout, a crash before the answer) is applied **exactly once** by retrying it with its key until it succeeds.
- This holds across restarts, checkpoints, WAL cuts, backups and restores, and process crashes: the key table is part of the namespace's state, rebuilt from the records and saved in checkpoints. Tested in `crates/iwdb/tests/idempotency.rs` (restarts, checkpoints, backup and restore, failed WAL writes and fsyncs) and by the kill -9 harness, whose children retry the keyed commits the killed child tried last; the model applies each key once.
- **Limits**: the store remembers the last 10 000 keyed commits; a retry after more keyed commits than that applies again. A key's entry is exactly as durable as its commit: under `group`, an OS crash that loses an acknowledged commit loses its key too, and its retry applies it (once).
- **Restore**: a restore to seq `N` keeps the keys of the commits `1 ..= N` and none after. A retry of a commit after `N`, which the restored history doesn't contain, applies it.
- A duplicate is answered even while the store is read-only (its commit was applied).

**Read-your-writes and deadlines.**

- A read with `min_seq` sees the commit `min_seq` and all before it: it waits until that commit is applied, returning as soon as it is, and fails with `Timeout` after its timeout (30 s by default), at once with `ReadOnly` if the store is read-only below `min_seq`, and with `OtherHistory` if the caller says the seq belongs to another history (a store restored since, or another store). Every commit is applied before it is acknowledged, so a client's own commits are always visible to its later reads.
- Commit results carry the commit time (the WAL's, ADR 0010).
- An analytics job runs under a cancel token that the store cancels at its deadline: it fails with `Timeout` (or `Cancelled`, when the caller cancels it), returns no partial result, and leaves the store usable.

## Namespaces and the catalog (step 9)

[ADR 0017](adr/0017-namespaces.md), [ADR 0018](adr/0018-namespace-keys-and-restore.md), [ADR 0019](adr/0019-online-index-build.md). Formats: [data-dir.md](formats/data-dir.md) (layout 4, the namespace log), [backup.md](formats/backup.md), [archive.md](formats/archive.md).

- **Independence.** Each namespace is its own graph with its own seq space, WAL, checkpoints, idempotency key table, indexes and constraints. A commit goes to **one** namespace; there are no transactions across namespaces. `min_seq` and `ReadOptions.history` refer to one namespace's seqs (the history id is the store's). Unique constraints are scoped to one namespace: the same value in two namespaces is fine, and under concurrent writers the constraint holds within each (tested with racing writers).
- **Create and drop are atomic and crash-safe.** A namespace exists exactly when its create event is in the namespace log, and not any more once its drop event is: the event is fsynced before the call returns, whatever the fsync policy. A kill (or failed write) at any point leaves the namespace either as it was or created / dropped completely; directories the log doesn't list are removed on the next open (an empty one the log never mentions; the one of a dropped namespace), and a directory *with data* that the log doesn't list makes the open fail with `NamespaceDamaged` rather than delete it. Tested by killing at every file operation of create, drop and the upgrade (`namespace_points.rs`), failing each one (`namespaces.rs`), and by the crash harness's catalog scenario.
- **Ids are never reused.** A dropped namespace's name can be created again; it is a different namespace with a new id.
- **Dropping.** Once the drop starts, commits to the namespace fail with `NamespaceDropped` and waits on its seqs (`min_seq`) fail with it at once; a read that already started finishes on the state it had. Everything in the namespace is gone for good (its WAL is archived first if the store archives: the archive keeps it).
- **Idempotency keys** (ADR 0018). Data and catalog commits: per namespace, as before. Creating and dropping a namespace take keys too: a retry returns the original event (`deduplicated`) also after a restart and after the namespace was dropped since; the same key for another request is `IdempotencyKeyReused`. Keys of namespace operations are never evicted.
- **Layout upgrade.** A layout 1, 2 or 3 store opens as one namespace, `default`, keeping its history, seqs, checkpoints, key table and history id; a crash at any point of the upgrade is finished by the next open (its commit point is the marker).
- **Indexes.** Created online ([ADR 0019](adr/0019-online-index-build.md)): readers and commits go on while the keys are read, and the index is installed in time proportional to the nodes changed during the build. A commit waits at most for one scan chunk (measured at 500 000 nodes: longest stall about 5 ms, p99 1.3 ms, against 183 ms for a plain build). A unique constraint's validation still holds the writer for its whole scan (about 430 ms at that size). The catalog is the truth about which indexes exist; they are rebuilt from it on recovery. `status` shows `building (scanned/total)` while a build runs.
- **Backups, archives, restores.** A backup is consistent per namespace (each at its own seq) and lists them in its manifest; no namespace is created or dropped while it runs. An archive keeps each namespace's segments in its own directory, including those of dropped ones. A restore to the latest brings back every namespace that existed then, each at the last seq the sources reach; to a time, every namespace as of that time (one created before it with no commit is empty, one dropped before it is absent); to a seq, one namespace (`AmbiguousTarget` otherwise). The restored namespace log keeps the keys of the namespace operations up to the target. Tested with several namespaces, one created and one dropped after the backup (`namespace_backup.rs`, `pitr.rs`), and by the fixtures `data-dir-v4`, `backup-v2` and `archive-v2`.
- **verify** checks each namespace (findings name it) and the log: damaged or missing namespace directories, directories the log doesn't list, a damaged or missing log, a backup whose manifest and files disagree (`namespace_verify.rs`).
- **Not guaranteed**: atomicity across namespaces; a consistent point in time across namespaces in a backup; that an online build finishes if the process is killed (it simply isn't there, and no record was logged).

## Crashes and simulated failures (step 6)

What happens at each failure the storage layer can meet. Each row has a failpoint test ([ADR 0007](adr/0007-failpoints-and-crash-harness.md): `crates/iwdb/tests/faults.rs`, `panics.rs`, `crates/iwdb-storage/tests/wal_faults.rs`) that checks the outcome, reopens and compares with a reference. The kill -9 harness (`tests/crash`) runs them as real crashes.

| Failure | Defined behaviour |
|---|---|
| **Process crash** (`kill -9`, abort) anywhere: in an append, a rotation, a checkpoint, a removal, recovery, initialization | Nothing written is lost under any policy (the page cache survives). The next open recovers the last acknowledged commit, or the one in flight if its record is complete. It removes temporary files, cuts a torn tail and finishes an interrupted initialization. A cut or a cleanup that recovery or a checkpoint didn't finish is done by the next open, or by the next checkpoint that writes a file |
| **Torn WAL frame** (a crash or an error halfway through an append) | A torn tail: recovery cuts it. The commit was never acknowledged |
| **Torn segment header** | A segment is created as `<name>.tmp` and renamed only after its header is written (and, except with `off`, fsynced), so a torn header is in a temporary file, which open removes. A last segment without a valid header (damage outside that protocol) is a torn tail at offset 0: removed |
| **Partial checkpoint** | Exists only as the temporary file of `write_atomic`, never under a checkpoint's name: open removes it, and recovery uses the previous checkpoint. A damaged checkpoint under its name (disk damage) is skipped (Recovery, below) |
| **Full disk (`ENOSPC`) or I/O error on a WAL write**, whole or halfway, in an append or a rotation | The commit fails with `Io` (`ErrorKind::StorageFull` for a full disk), is not applied, and the store is read-only until reopened. Its record is not in the log, or is a torn tail that recovery cuts. Opening needs room for a new segment. Until there is room, open fails with `Io` and loses nothing |
| **A WAL write or fsync that reports an error after the data reached the file** | Like any failed write or fsync: not applied, read-only, never retried. The outcome is unknown, and recovery finds the record |
| **Failed WAL fsync** (the commit's, a rotation's, the group commit timer's) | The commit fails, or with the timer no commit notices, and the store is read-only until reopened. The fsync is never retried. With `group`, the unsynced acknowledged commits may be lost if the OS also crashes (within the policy's window) |
| **Failed directory fsync of `wal/`** (a new segment) | The commit that needed it fails, and the store is read-only until reopened |
| **`ENOSPC` or I/O error on a checkpoint write** (before, halfway, when complete but not renamed) | The checkpoint fails with `Io`. Its temporary file is removed, and nothing else changes. Commits go on. The next trigger retries |
| **A failure after a checkpoint's rename**: the directory fsync of `checkpoints/` or `wal/`, removing an old checkpoint or a WAL segment | The checkpoint fails. The new file stays, and what wasn't removed yet stays. Checkpoints are disabled until the store is reopened (`CheckpointsDisabled`), because a retried directory fsync can succeed without persisting anything. Commits go on, and recovery uses the new checkpoint |
| **A failure while opening**: removing temporary files, cutting the torn tail (or its fsync), removing a headerless segment, starting the writer, initializing | Open fails with `Io` and releases the lock. What it did is part of recovery's defined repairs, and the next open finishes the rest |
| **A panic in the commit path**, or an apply that fails with `GraphError::Internal` | The process aborts (ADR 0008, ADR 0028), which is a process crash (first row) |
| **A crash or a failure during an online backup** (step 7): any file write, fsync, directory sync, the manifest, the marker | The backup fails (or the process dies), and the store is unaffected: a backup only reads the store's files. The backup directory has no marker until its last step, and holds an empty `BACKUP` from its first: a store, `verify` and restore all refuse it (`NotADataDir`). A failure after the marker's rename leaves a complete backup |
| **A crash or a failure while archiving** (step 7): a segment's copy (create, write, fsync, rename) or the archive's directory sync | No segment leaves `wal/` before it is durable in the archive, so none is lost. A failed copy fails the checkpoint and is retried by the next one; a failed directory sync disables checkpoints until reopening. A crash between archiving and removal leaves a segment in both places, archived again (idempotently) by the next checkpoint |
| **A crash or a failure during a restore** (step 7) | The target is refused by the next open (`InterruptedRestore` while `RESTORING` exists; `NotADataDir` for a checkpoint without a marker), or is empty, or complete once its marker is in place. Never a wrong state. The sources are only read |
| **OS crash or power loss** | Per policy (table above). Checked by simulation: the harness cuts or zeroes the unsynced end of the last segment. With `always` every acknowledged commit survives. With `group` at least those up to the last completed fsync survive. With `off` recovery may refuse only as the table says |

**How it is checked (step 6).** The harness kills a child process running a random workload with background checkpoints, at random moments and at failpoints on every write-side call. Sometimes it also simulates an OS crash. Then it recovers and compares the seq, the canonical state and the catalog with a reference model at the recovered seq. Every run prints its seed. `cargo test` runs a short run and the crash points step 5 listed. CI runs 150 cycles per policy on every PR, and thousands nightly (step_6.md has the numbers). No acknowledged commit was ever lost and no partial transaction seen, except commits after the last completed fsync under `group` and `off` in the simulated OS crashes, which those policies allow.

## Backup, archiving and restore (step 7)

[ADR 0009](adr/0009-backup-archive-restore.md); formats: [data-dir.md](formats/data-dir.md), [backup.md](formats/backup.md), [archive.md](formats/archive.md).

**Online backup** (`Store::backup`, `iwctl backup`, `store.backup()` in Python):

- **What it reaches**: the WAL is fsynced first; the backup holds every commit up to the seq that was then synced, which is the last commit (if the store is read-only, its synced seq). It never holds a commit the store could still lose in a crash, so its history is a prefix of the store's.
- **What it holds**: every checkpoint at or below that seq (not those known to be damaged), and the WAL from the oldest of them up to the seq, cut right after it. It restores to any seq in that range.
- **Consistent while the store runs**: it holds the checkpointer's lock, so no checkpoint or segment it copies is removed during the copy. Commits wait only for the fsync; checkpoints wait for the copy, and the WAL grows meanwhile.
- **Complete or refused**: every file is fsynced, then the manifest, then the marker; an interrupted backup is refused by a store, `verify` and restore. The manifest's lengths and CRC32Cs let `verify` find a missing, extra or truncated file.
- A store refuses to open a backup (`IsBackup`): it is restored instead.
- **Throttled** (step 16e, `StoreOptions::backup.max_bytes_per_second`, `[backup] max_bytes_per_second`): the copy writes at most that many bytes per second, measured from its start (a burst is at most one 1 MiB chunk). It changes nothing else: the backup is the same, and **checkpoints wait for the whole copy**, so a slower backup holds them back longer and the WAL grows meanwhile. A checkpoint started during a throttled backup completes once the copy is done, and both are consistent: the backup restores to the state at its seq, and with the archive to the later checkpoint's (`crates/iwdb/tests/admin_writes.rs`, canonical-state comparison against the reference).
- **On a server** (step 16e, [ADR 0055](adr/0055-admin-writes-and-iwctl-against-a-server.md)) a backup is written only into `[backup] dir`, under a name that is one path component; nothing that exists under the name (file, directory, symlink) is ever written to or replaced, and a failed backup is removed.

**Continuous WAL archiving** (`StoreOptions::archive`):

- A WAL segment leaves `wal/` only after its copy is in the archive and both the copy and the archive's directory are fsynced. No segment the checkpointer removes is lost: the archive and the WAL together always hold the history from the archive's first segment (seq 1 for an archive set up with the store).
- Archiving is idempotent: a segment left in both places by a crash is archived again (rewritten, its bytes compared first) and then removed.
- **If archiving fails**, nothing is removed from `wal/`: the WAL grows, `Store::checkpoint_failure` reports the error, and the next checkpoint that writes a file retries. A failed fsync of the archive's directory isn't retried: checkpoints stay off until the store is reopened. Commits go on in every case.
- An archive belongs to one history: a store refuses an archive of another (`ArchiveMismatch`), and two stores never write one archive (its lock).
- **Pruning** (`iwdb::prune_archive`, `iwctl archive prune`, `PruneArchive`; step 16e) before a backup removes only what restores to before that backup's oldest checkpoint need: per namespace the backup holds, a segment is removed only if the next archived segment starts at or before that checkpoint's seq + 1, and the last segment never is. A restore from that backup (or a later one) and the archive reaches every seq it reached before; namespaces the backup doesn't hold are untouched; a backup of another history is refused. Files go oldest first, so an interruption leaves a valid archive. A store may keep archiving meanwhile. Tested by restoring to the backup's oldest checkpoint, its seq, a later seq and the latest after pruning (`crates/iwdb/tests/admin_writes.rs`).

**Restore and point-in-time recovery** (`iwdb::restore`, `iwctl restore`, `iwdb.restore()` in Python):

- A restore to seq `N` (from a backup, an archive, or both, of the same history) gives exactly the state after the commits `1 ..= N`: data, versions and catalog. The restored store's next commit is `N + 1`.
- To a time `T`: the state after the last commit, in seq order, whose commit time is at or before `T` ([ADR 0010](adr/0010-commit-times.md)). Commit times are the writer's wall clock, made non-decreasing, in microseconds.
- Every restore starts a **new history**: the restored store's commits after `N` can't be mixed up with the original's. It needs a new archive directory.
- A restore never writes to its sources, and an interrupted restore is never opened as a store (table above).
- How it is checked: PITR to random mid-history seqs and times from a backup alone, an archive alone and both (`crates/iwdb/tests/pitr.rs`), failpoints on every write of backups, archiving and restores (`backup.rs`, `archive.rs`, `pitr.rs`), and the kill -9 harness, which kills children during backups, archiving and restores and restores to random seqs (step_7.md has the numbers).

## Verify (step 7)

`iwdb::verify` (`iwctl verify`, `iwdb.verify()`; [ADR 0011](adr/0011-verify.md)):

- **Never writes** anything, and never creates `LOCK`. It takes a shared lock, so it fails with `Locked` while a store has the directory open, and a store can't open it while verify runs.
- Finds damage in any file the database writes: every checksum (marker, WAL headers and frames, checkpoints, manifest, archive marker), a checkpoint whose state (the idempotency key table included, step 8) differs from what the WAL replays to, an invalid key table, a WAL that doesn't reach a checkpoint, and broken invariants (edge endpoints, versions, reserved keys, value depth, indexes against a scan, constraints). The tests flip bytes in every kind of file and plant states that violate each invariant.
- Reports what a crash leaves (a torn tail, temporary files, an interrupted cleanup) as notes, not damage, and changes nothing about it. The harness runs verify before every recovery it checks: it finds no problem exactly when recovery succeeds, and reaches the same seq.

**An open store** (`Store::verify`, the `Verify` RPC, `iwctl --server ... verify`; step 16e, [ADR 0055](adr/0055-admin-writes-and-iwctl-against-a-server.md)) is verified without stopping it: per namespace, the same checks of its checkpoints and WAL, with the WAL fsynced first and read only up to the synced seq (records appended meanwhile are never decoded, so a growing tail is no false alarm), then the live state's invariants, and the live state against the replayed one if nothing was committed during the check. It holds the namespace's checkpointer lock while it reads (checkpoints wait) and changes nothing. It isn't run while the server refuses writes for memory: its replay uses memory the limit doesn't count. Tested with commits running during the check and with a damaged checkpoint (`admin_writes.rs`), and over every API by the admin conformance suite.

## Python (step 7)

The Python bindings ([python-api.md](python-api.md), [ADR 0013](adr/0013-python-bindings.md)) give the same guarantees as the store (since step 8 also idempotency keys and `min_seq`, with the GIL released while waiting): a transaction is one commit, all or nothing; nothing is committed when its `with` block raises. A panic in the commit path aborts the interpreter (a crash, recovered by the next open); any other panic raises `iwdb.InternalError`. A child forked without exec inherits the store's lock and must not use the store.

## The gRPC server (step 11)

The server ([api/grpc.md](api/grpc.md)) gives the guarantees of the store it serves, through the `Database` trait, and adds:

- **Shutdown loses nothing acknowledged.** On SIGINT/SIGTERM the server drains running calls (up to its drain timeout), cancels the rest, lets accepted commits finish, then fsyncs every WAL and checkpoints (if configured) before it exits. Every commit acknowledged before shutdown survives an OS crash after it, under every fsync policy ([ADR 0027](adr/0027-graceful-shutdown.md); `crates/iwdb-server/tests/shutdown.rs` simulates the crash under `off` and `group`).
- **Every read ends at its deadline**: the smaller of `grpc-timeout` and the request's `timeout_ms`, capped by the server's maximum, including the time it waits for a worker or for `min_seq` ([ADR 0026](adr/0026-deadlines-over-grpc.md)). A read whose client goes away is cancelled.
- **A commit has no deadline on the server.** Once accepted it runs to the end, even if its client's deadline passes or the client disconnects; that client doesn't learn the outcome and retries with the same idempotency key, which applies the commit at most once.
- **Errors carry their code** (`iwdb-code`) next to the gRPC status of [errors.md](api/errors.md).
- A bug in a commit's apply path aborts the whole server (all namespaces), as a crash: run it under a supervisor ([ADR 0028](adr/0028-internal-apply-errors-abort.md)).
- **Ready means recovered** (step 16b). The server opens its port before the store and answers health while it recovers; until recovery has finished it is not ready (`GET /v1/health/ready` 503, `grpc.health.v1` `NOT_SERVING`) and every database call fails with `unavailable`, so no client reads a namespace halfway through its replay. The first ready answer comes after recovery has applied every record in the log: every commit acknowledged before the last stop or crash is visible then. A shutdown turns readiness off before it drains. Tested in-process with the store's open held at a gate, and against the binary recovering a 100 000-node WAL (`crates/iwdb-server/tests/health.rs`, `tests/binary.rs`; [ADR 0040](adr/0040-health-and-readiness.md)).

## Authentication and authorisation (step 15a)

[ADRs 0043 to 0047](adr/0045-the-authorisation-point.md); configuration: [api/config.md](api/config.md#authentication-and-the-first-admin).

What it guarantees, with `[auth] enabled` (the default):

- **Every database call to the server has a principal.** Everything but health, login, the console's pages and the OpenAPI document needs a session or API token; without one it fails with `unauthenticated` before any operation runs.
- **Roles are checked once, for every operation**, in front of the `Database` trait: `read`, `write` and `admin` per namespace, and a server-wide admin. A refused call changes nothing (`permission_denied`). Every role × operation combination is tested over gRPC and REST from one table (`crates/iwdb-server/tests/roles.rs`).
- **User, grant and token changes are all or nothing and durable** per the store's fsync policy when they return: each is one commit to the store's reserved system namespace, recovered, backed up and restored with the rest of the store ([ADR 0043](adr/0043-users-and-roles-in-the-system-namespace.md)). Checked by the kill -9 harness at every WAL write and fsync of a sequence of changes, under `always` and `group` (`tests/crash/tests/auth_points.rs`): recovery finds the users of the acknowledged changes, or of the one in flight, never part of one.
- **No password or token is stored, logged or put in an error message**: passwords as argon2id hashes (compared in constant time), tokens as SHA-256 hashes; tested by grepping the server's logs at `debug` through logins, failures, tokens and password changes (`tests/binary.rs`).
- **No default password**: a server with authentication on and no users refuses to start.
- **Sessions end** at their lifetime, at logout, when the user's password changes, when the user is deleted, and when the server restarts. API tokens end when revoked, when they expire (if they do), or with their user. A revoked grant takes effect at the next request.
- **Failed logins are slowed down**: at most `login_max_failures` per user and per client address within `login_window_secs`.

What it doesn't guarantee:

- **No secrecy on the wire with TLS off.** With `[tls] enabled = false` (step 15b, below) the server speaks plain TCP: passwords, tokens and data cross the network in clear, so anyone who can see the traffic can take a session. A non-loopback listen address then also needs `[server] plaintext_public = true`.
- **No protection of the data directory.** The embedded store, Python's `Store.open` and `iwctl` on a data directory are unauthenticated: whoever can open the directory owns the store, users included. Its file permissions are the boundary. A backup holds the password hashes.
- **The slowdown is per process**, forgotten at a restart, and can lock a known user out for one window at a time.
- **A restore brings back the users of its point in time**, passwords included.

## TLS and mTLS (step 15b)

[ADR 0048](adr/0048-tls-and-mtls.md); configuration: [api/config.md](api/config.md#tls-and-mtls).

What it guarantees:

- **A default server speaks only TLS.** Without `[tls] enabled = false` it serves nothing in plaintext, and without a certificate and key it doesn't start. Plaintext on a non-loopback address needs a second flag, `[server] plaintext_public = true`; either flag alone is refused before the store opens (`crates/iwdb-server/tests/binary.rs`, the config tests; CI's docker job for the image).
- **Clients verify the server.** `Remote`, `RestRemote`, Python's `iwdb.connect`, `iwctl` and `serve.py` refuse a certificate of a CA they don't trust and an expired one: the call is `unavailable`, with the reason (`crates/iwdb-server/tests/tls.rs`, the Python and iwctl suites).
- **A client certificate is verified before it counts**: an expired one or one of another CA fails the handshake; nothing is served on that connection, not even login.
- **A client certificate authenticates as the user its subject's common name names**, with that user's roles, checked by the same authorisation point as a token (ADR 0045); a certificate that names no user, or not exactly one common name, is `unauthenticated`. A token or session in the request wins over the certificate. Tested over gRPC and REST for reads, writes and server-admin operations.
- **With `[tls] client_auth = "required"`, every request without a client certificate is refused** (`unauthenticated`), login and tokens included; only health and the console's pages are answered.
- **A REST write authenticated by a client certificate alone needs the CSRF header**, as one authenticated by the console's cookie does.
- **The console's session cookie is `Secure` over TLS** (and `HttpOnly`, `SameSite=Strict`); the console keeps nothing in `localStorage`.
- **SIGHUP reloads the certificate, key and client CA** for new connections; a reload that fails keeps the ones in use, and the server goes on serving (`sighup_reloads_the_certificate`).
- **No private key is logged, printed or put in an error message**: `--check-config` prints its path; errors name the file and what is wrong with it. Tested by grepping the server's logs at `debug` through reloads of a mismatched and a cut-off key (`no_secret_reaches_the_logs`), and the errors of mangled keys (`errors_never_quote_a_private_key`).
- **No certificate or key is in the Docker image**; CI's docker job checks it.

What it doesn't guarantee:

- **The probe doesn't verify the server's certificate** (`iwdb-server --probe`, the image's `HEALTHCHECK`): it sends no credentials and reads only readiness.
- **No certificate management.** Nothing renews a certificate or checks revocation (no CRL or OCSP); replace the files and send SIGHUP. `docker/dev-cert.sh`'s certificates are for development only.
- **Open connections keep their certificate** after a reload, until they close.
- **The Postgres source of projections connects without TLS.**

## Audit log (step 15c)

[ADR 0049](adr/0049-audit-log.md); configuration: [api/config.md](api/config.md#audit-log).

What it guarantees:

- **Every login, logout, user, grant, token, namespace and catalog change leaves one entry**, with its principal (user, and how it authenticated: session, API token, client certificate, or authentication off), the client's address, the operation, the namespace, and the outcome (and error code). This holds whether the request came over gRPC or REST, and so from Python, `iwctl --server` and the console. **Every refusal** (`unauthenticated`, `permission_denied`) of any operation leaves one too. All are made at the authorisation point, never in an adapter. Tested for every role × operation cell over both APIs (`crates/iwdb-server/tests/roles.rs`) and for certificate principals (`tls.rs`).
- **No secret or data value in an entry.** An entry never holds a password, a token, a token's hash, a certificate, an error message or an attribute value. The binary test greps the log and the audit file at `debug` through logins, failures, tokens, password changes, a commit and a catalog change (`no_secret_reaches_the_logs`).
- **The log level doesn't hide entries** unless it names `iwdb::audit`.
- **Bounded files.** With `[audit] dir`, files older than `[audit] retention_days` are deleted.
- **No telemetry.** Without projections, the server opens no connection of its own (`opens_no_connection_it_was_not_configured_for`, by `lsof`), and the console loads nothing from another site ([SECURITY.md](../SECURITY.md)).

What it doesn't guarantee:

- **Entries are not durable with the change.** They are written after the outcome, without fsync; a crash can lose the last ones, never the changes, which are in the WAL. A call whose client goes away before it finishes can leave no entry.
- **User, grant and token changes carry no seq** (catalog changes and namespace events do).
- **Successful reads and data commits aren't audited** (the change stream records commits), nor failed TLS handshakes, nor in-process access to a data directory.
- **Entries in stderr are kept as long as the log collector keeps them.**

## Metrics, status views and cancel (step 16c)

[ADRs 0050 to 0052](adr/0051-the-status-views.md); [api/metrics.md](api/metrics.md), [api/rest.md](api/rest.md#operator-reads).

What it guarantees:

- **Every metric is documented, and every documented one exported**: `metrics.md`'s table equals the code's list, and a scrape of the binary's `/metrics` holds exactly those metrics. Labels are bounded (operation, outcome code, lock kind, live namespace, version), never an id, a user, a client or a data value.
- **Every status read is bounded** (requests and log events at most 1000 per read, readers at most 1024, the rest O(namespaces)) and implemented once (`Embedded`); gRPC and REST give the same answers (the `Admin` conformance suite). The `schema` read (step 16c-2) is O(sample) with the sample bounded by `max_visited` and `max_edges`, and in the `Database` conformance suite.
- **The operator's reads don't wait** for a namespace lock or a worker: requests, cancel, readers, metrics and the log answer while every worker is busy and while a commit waits for an fsync.
- **Cancel**: a running read cancelled with `CancelRequest` ends with `cancelled` for its caller, unless its answer was ready first; a cancel of a request that has ended is `not_found`. Commits and other changes can't be cancelled (`invalid_argument`). Users see and cancel only their own requests; server admins anyone's. Every cancel is audited.
- **The log tail holds what the log holds, no more**: the same events after the same level filter, so no secret (`no_secret_reaches_the_logs` reads the whole tail too). Only server admins read it.
- **Pulled, never pushed.** `/metrics` needs credentials like any route; the server opens no connection to send metrics or logs anywhere.

What it doesn't guarantee:

- **The status isn't one consistent cut across namespaces**: each namespace's part is consistent on its own.
- **Counts start when the database starts serving**, and calls made on an embedded store in-process aren't counted or listed.
- **A running request's visited count isn't reported** (the core counts it only inside a search; upstream draft 23).
- **The schema's keys are sampled** ([ADR 0053](adr/0053-the-schema-read.md)): labels and edge types are complete with exact counts (the core's, upstream [#60](https://github.com/p-sodmann/Ironweaver/issues/60)), up to 10 000 of each (`truncated` beyond), but attribute keys come from the first `max_visited` nodes. They are complete when `sampled_nodes` equals `nodes`.
- **The log tail is in memory**: a restart empties it, and older events fall out once `[log] tail_events` are kept.

## Managed analytics jobs (step 16f)

[ADR 0056](adr/0056-managed-analytics-jobs.md); [api/rest.md](api/rest.md#managed-jobs), [api/config.md](api/config.md#managed-jobs) (`[jobs]`).

What it guarantees:

- **A job outlives any request's timeout.** It runs on the server's job threads until it ends, its own timeout (`[jobs] timeout_secs`, default an hour), a cancel or the drain, and its result can be fetched, in bounded pages, until it expires; then `not_found`. Tested with a 50 ms maximum request timeout, over gRPC and REST (`crates/iwdb-server/tests/jobs.rs`).
- **The same answer as `analyze`**: the same projection, limits and ranking, and the rows of a page sequence equal `analyze`'s (the admin conformance suite, over embedded, gRPC and REST).
- **Jobs never take a query worker**, so a long job doesn't hold back reads.
- **Every bound has a cap and a defined answer**: a full queue or a user with `[jobs] per_user` jobs is refused with `unavailable`; a projection too large for the limits with `budget_exceeded`, at once; ended jobs are kept for `[jobs] retention_secs` and at most `[jobs] max_finished` (the first to end goes first); stored results hold at most `[jobs] result_bytes` together (the oldest are dropped first, their jobs `expired`), and a result alone larger fails its job with `budget_exceeded`.
- **A cancel takes effect at once**: the job is `cancelled` when `CancelJob` (or `CancelRequest` on its id) answers, its thread stops at the core's next check, and a queued one never starts. A job that had ended keeps its outcome.
- **Who may**: starting needs `read` on the namespace; a job is seen, cancelled and fetched only by its owner and server admins (others get `not_found`); fetching a result needs `read` on the namespace still. Starting and cancelling are audited, with the job's id (`roles.rs`).
- **Counted memory**: a running job's projection and every stored result count in the memory limit's `working` part, and leave it when the job ends and the result expires (`crates/iwdb/tests/memory.rs`).
- **The drain cancels every job** and refuses new ones (`unavailable`); the jobs stay readable during it, and the drain still ends in time (`jobs.rs`).

What it doesn't guarantee:

- **Progress is the core's report**, in its units (iterations, runs, nodes or sources) per phase, not a time estimate: a converging algorithm (PageRank, label propagation) ends below its total, and Leiden's runs vary in length. It is read while the job runs and kept as it was when the job ended (`jobs.rs`, `algorithms_report_progress` in `core_smoke.rs`).
- **Nothing persists**: a restart or a drain loses every job and result. Ids restart at 1, so an id kept across a restart can name a newer job.
- **Expiry is checked when the registry is used**, not by a timer: an expired result is never served, but may hold its memory until the next call that looks.
- **Jobs start while writes are refused**, as reads do: what they add is bounded (`[jobs] running` projections and `[jobs] result_bytes`) and counted, not refused.

## The memory limit (step 16d)

[ADR 0054](adr/0054-the-memory-limit.md); [api/config.md](api/config.md) (`[memory]`), [api/errors.md](api/errors.md) (`resource_exhausted`), [api/metrics.md](api/metrics.md).

What it guarantees:

- **A refused write is never logged.** Above `refuse_writes_at` of the limit, a commit, an index or constraint creation, a namespace creation or an import fails with `resource_exhausted` before anything reaches the WAL or the namespace log. Nothing changes, and recovery finds none of it. `a_refused_commit_is_never_logged_and_one_below_the_line_is_durable` (`crates/iwdb-storage/tests/memory.rs`) fails every write and fsync from the refusal on and finds none attempted. `a_crash_after_a_refused_commit_recovers_exactly_the_accepted_ones` (`crates/iwdb/tests/memory.rs`) does the same through the store, then crashes and recovers.
- **A commit accepted below the line is durable** like any other, per the fsync policy (the same tests).
- **One check, in the commit pipeline** (design rule 2): every adapter gets the same answer (`over_grpc`, `over_rest` in `crates/iwdb-server/tests/memory.rs`).
- **What goes on while writes are refused:** reads, `analyze` included; commits that only remove (nodes, edges, labels, attributes); dropping an index, a constraint or a namespace; the system namespace (users, grants, logins); repeats of keyed commits, which answer their original result.
- **Hysteresis:** the state rises when memory reaches a line, and falls only once memory is 5 % of the limit below it, so a store at the line doesn't flip between accepting and refusing (`writes_resume_only_once_memory_is_below_the_band`).
- **Visible:** the status and the metrics report the counted parts, the limit, the lines, the state and where the limit came from; each change of state is logged once.

What it doesn't guarantee:

- **The accounting is an estimate.** It counts the live graphs (the core's figure), their payloads (ours, from lengths: 3–8 % above the measured heap), the checkpointers' copies, and projections and index builds while they run (by a formula per node and edge). It doesn't count request buffers, the WAL's buffer, the allocator's slack or the runtime. The process's resident memory can be higher than `used`; the 10 % between the refusal line and the limit is for that.
- **One commit can cross the line.** A commit is admitted on the state before it, so `used` can pass the line by one commit (a WAL record is at most 64 MiB). The next one is refused.
- **Reads aren't limited by memory.** An `analyze` is counted, and can push the store into refusing writes, but it isn't refused.
- **An embedded store has no limit** unless `StoreOptions::memory` sets one: it can't see the memory of the application it runs in. Without a limit nothing is refused.
- **Per-namespace and per-client limits** are step 15d's, and nothing is evicted to disk.

## The change stream (step 13)

The change stream ([api/changes.md](api/changes.md), [ADR 0031](adr/0031-change-stream.md)) returns a namespace's commits from a seq on, as logged:

- **Only durable commits**: up to the lower of the applied and the synced seq (the applied seq under `off`). So no crash takes back a commit it returned, and a seq never comes back with other content (`an_os_crash_under_group_commit_takes_back_no_streamed_commit` in `crates/iwdb/tests/changes.rs` cuts the WAL to its last fsync under `group` and checks it). Under `group` it lags behind acknowledged commits by up to `2 * max_delay`.
- **In order, without gaps**: a consumer that resumes from the seq after the last one it processed sees every commit once, across restarts (`a_consumer_resumes_after_restarts_without_gaps_or_duplicates`).
- **As long as the WAL holds it**: older seqs fail with `not_retained`. Retention (`WalRetention`) keeps segments that checkpoints no longer need. Damage in the WAL it reads is `corrupt`, never skipped.

## Projection mode (step 13)

A projection ([api/projections.md](api/projections.md), [ADR 0032](adr/0032-projection-mode.md)) moves its mark in the commit that applies its events (compare-and-set), and resumes from it:

- **Exactly once across crashes**: a failure at each point of the commit path (the WAL write before, halfway and after, the fsync before and after) stops it with the outcome unknown; after a restart it finishes, and every event is applied once, checked with a mapping that isn't idempotent (`a_projection_survives_crashes_without_applying_an_event_twice` in `crates/iwdb/tests/projection.rs`; checked to fail when the mark is committed apart from the events). An OS crash under `group` loses events and their marks together (`an_os_crash_under_group_commit_loses_events_and_their_marks_together`).
- **One writer per mark**: a commit with a stale mark fails with `conflict` and changes nothing.
- **Postgres holes**: positions are read as dense; a hole is waited for up to `gap_timeout`, then skipped. An event whose transaction commits later than that is missed: set the timeout above your longest writing transaction.

## Import and export (step 13)

An import ([api/import-export.md](api/import-export.md), [ADR 0033](adr/0033-bulk-import-export.md)) creates a namespace from a file as one checkpoint at seq 1, without WAL records:

- **All or nothing across crashes**: the namespace is there with all the file's data, or not at all. The create event in the namespace log is the commit point; a checkpoint staged before it is finished by the next open (`every_file_operation_of_an_import_can_fail` in `crates/iwdb/tests/import.rs` fails every file operation in turn, reopens and verifies; checked to fail when recovery doesn't finish the staged import).
- **Checked before anything is written**: the graph is checked like a recovered namespace; a file that is invalid, or breaks an invariant, creates nothing (`invalid_argument`).
- **Not in the WAL, but in the archive**: the change stream of an imported namespace starts at seq 2 (seq 1 is `not_retained`). A store with a WAL archive copies the import's checkpoint there before the import returns (archive format 3), and on every open if it is missing, so the namespace restores from the archive alone, and from a backup taken after the import (`an_imported_namespace_restores_from_a_backup_and_from_the_archive_alone`, `an_open_archives_the_checkpoint_of_an_import`; both checked to fail without the copy). Without an archive, take a backup after an import.
- **Merges** ([api/import-export.md](api/import-export.md)) are ordinary commits, in batches: durable, streamed and archived like any; a failure leaves the batches before it committed.
- **Round trip**: an export imports back to the same graph; re-exported, to the same bytes (`an_import_creates_its_namespace_from_one_checkpoint_and_an_export_imports_back`). Versions restart at 1; constraints, idempotency keys and marks are not exported.
- An export reads the namespace at one seq; commits to the namespace wait while it writes.

## Platforms (step 7)

Linux and macOS. **Windows is not supported yet**: there the directory fsync is a no-op, so after an OS crash a rotation, a checkpoint, a backup or a restore can lose a directory entry, and none of this is tested on Windows. No Windows wheel is shipped (ADR 0013).

## Recovery (step 5)

`Store::open` takes the data directory's exclusive lock, then rebuilds the namespace from the newest checkpoint that loads and every WAL record after it ([data-dir.md](formats/data-dir.md)).

- **What it recovers to**: the state after the last complete commit in the log, with the catalog as of that commit. That includes every acknowledged commit the fsync policy made durable (table above): with `always`, every acknowledged commit after any crash. No partial transaction is ever visible: a commit is one log record, applied whole or not at all.
- **A crash during an append** leaves a torn tail, which recovery cuts off. The commit it belonged to was never acknowledged.
- **A crash during a checkpoint** leaves either the previous checkpoints and a temporary file (removed on open), or the new checkpoint without the WAL cut yet. Either way nothing is lost.
- **A damaged checkpoint** (checksum, truncation, format) is skipped, and recovery falls back to the next older one and replays more WAL. The WAL is kept from the oldest kept checkpoint on (2 by default), so this works as long as one kept checkpoint loads.
- **It refuses rather than repairs.** Corruption in the WAL, WAL records missing behind the newest usable checkpoint, a WAL that ends before a checkpoint, or a record that fails to replay: open fails with a typed error and changes no data file.
- **The lock**: a second `Store::open` of the same directory fails with `Locked`, in the same process or another one. The lock is released by `close`, by dropping the store, and when the process exits, also on `kill -9`. An open retries a held lock for about 80 ms first, so that a process another thread is spawning (which holds a copy of the lock file until its exec) doesn't make a reopen fail (step 7).

## Checkpoints (step 5)

- A checkpoint holds exactly the commits up to its seq, catalog included, and only commits that were synced to the WAL, so the WAL always reaches it.
- Checkpoints don't block commits: the checkpointer replays the WAL into its own copy of the namespace. Measured commit latency is the same during a checkpoint as outside one (step_5.md). The cost is a second copy of the graph in memory.
- A failed checkpoint never deletes a WAL segment or changes a previous checkpoint. If the failure happens after the new file is in place (a directory fsync or a removal), checkpoints stay off until the store is reopened, and commits continue. `Store::checkpoint_failure` reports it.

## Integrity of the log (step 4)

- Every record and segment header is checksummed (CRC32C over all of its fields and payload, the commit time included since WAL format 2, step 7, and the idempotency key and result since format 3, step 8).
- A torn or damaged record at the end of the log (from a crash during a write) marks the end of the log. The records before it are intact, and its position is reported for recovery to truncate.
- Damage anywhere else is never skipped silently: in an earlier segment, before a record that proves the damaged one was synced, a gap or a repeat in `seq`, or an unreadable record with a valid checksum. The reader reports it as an error.
- Reading never panics on corrupt input and never allocates more than the file's size for a corrupt length.

## Limits (step 4)

- A commit's WAL record is at most 64 MiB, otherwise the commit is rejected with `RecordTooLarge`.
- Attribute values are nested at most 100 levels deep (`MAX_VALUE_DEPTH`).
- Segment size is 1 KiB to 1 GiB (64 MiB by default). A reader holds one segment in memory at a time.
