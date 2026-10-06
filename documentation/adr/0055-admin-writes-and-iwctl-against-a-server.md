# ADR 0055: Admin writes, and iwctl against a running server

Status: accepted
Date: 2026-10-06

## Context

Step 16e asks that every admin task can be done with `iwctl` against a running server, without its data directory, and that the WAL archive and online backups stay manageable at scale. Until now `iwctl` checkpoints, backs up and verifies only a data directory that no store has open (ADR 0012), and the server has no way to do these at all. The server also has no WAL archive: `[store]` has no setting for it.

Points to decide: where the admin writes live, who may call them and what the audit log records, where a backup may be written on the server, what `restore` does against a server, how the writes behave under the memory limit (ADR 0054), how backups are throttled, how an archive is pruned, and whether the data directory records its archive.

## Decision

### Four admin writes on the `Admin` trait

`Admin` (ADR 0051) gets four methods. They are implemented once, in `iwdb::Embedded`, on the store's own functions. They are translated by gRPC (`AdminService`), REST, both Rust clients and `iwctl`, which decide nothing (design rule 8).

| Method | RPC / REST | What |
|---|---|---|
| `checkpoint(namespace)` | `Checkpoint`, `POST /v1/checkpoint` | `Ns::checkpoint` of one namespace, or of every namespace (`Store::checkpoint_all`). The store archives what it removes, if it has an archive. |
| `backup(name, max_bytes_per_second, verify)` | `Backup`, `POST /v1/backups` | `Store::backup` into `<backup dir>/<name>`, optionally throttled, then `verify` of the result unless `verify: false` |
| `verify(target)` | `Verify`, `POST /v1/verify` | the live store (`store`), a backup in the backup directory (`backup`), or the store's archive (`archive`) |
| `prune_archive(before, dry_run)` | `PruneArchive`, `POST /v1/archive/prune` | removes from the store's archive what no restore from the backup `before` (or a later one) can need |

All four run on a thread of their own, not on a query worker. A throttled backup can run for hours, and a checkpoint can wait that long for a backup's copy (ADR 0009). On a worker, either would hold back the reads queued behind it.

**Verifying a running store.** `iwdb_storage::verify` takes a shared lock, so it can't read a data directory that a store has open (ADR 0011). `Store::verify` checks the open store instead, namespace by namespace, holding each namespace's checkpointer lock (as a backup does, so that no file vanishes meanwhile):

- every checkpoint loads and keeps the invariants;
- the WAL reaches back to every checkpoint;
- the WAL replays without a gap onto the oldest checkpoint that loads, up to the namespace's synced seq. Records after it may still be appended, so they are never decoded (the backup's bounded read). At each newer checkpoint's seq the replayed state must equal that checkpoint.

Then, under the namespace's read lock, it checks the live state's invariants. When nothing was committed during the check, it also compares the live state with the replayed one. Damage is reported in the `VerifyReport` (the same shape as `iwctl verify`'s), not as an error.

Checkpoints of a namespace wait while it is checked. Commits don't wait, except for the read lock at the end.

### The operation table

New rows in ADR 0045's table, with their audit column (ADR 0049):

| Operation | Requirement | Audited | Cancellable |
|---|---|---|---|
| `Checkpoint` | server admin | always | no |
| `Backup` | server admin | always | no |
| `Verify` | server admin | always | no |
| `PruneArchive` | server admin | always | no |

- **Server admin, not namespace admin.** A checkpoint of one namespace waits for, and holds back, every other namespace's backup. The backup and the archive are server-wide. A backup writes files on the server's disk, which is a server admin's business.
- **Always audited.** Each write changes files on the server and is rare, so recording every call costs nothing. `Verify` changes nothing but reads every file of the store, so it is recorded too: who started a full read of the store is worth knowing. The entry names the namespace (checkpoint) or the backup's name (`backup`, a new field: a name in the backup directory, never a full path).
- **Not cancellable.** Dropping the future doesn't stop the thread, so a cancel that answered "cancelled" would be wrong. A backup cut midway would also leave a directory without a marker that someone must remove (ADR 0009). Step 16f's managed jobs can make backups cancellable between files.

### Where a backup may be written

A backup is named, not given as a path. `[backup] dir` names the **backup directory** on the server (`Embedded::with_backup_dir`). A backup's name is one path component of 1 to 128 ASCII letters, digits, `.`, `_` or `-`, not starting with `.`. The backup is `<backup dir>/<name>`. So:

- **No traversal.** A name can't hold `/`, `\` or `..`, and can't be absolute.
- **No overwriting.** The target must not exist in any form: no file, no directory (even an empty one), no symlink, dangling or not. It is created with `create_dir`, which doesn't follow a symlink in the last component and fails if anything is there. A race with another process is then a failure, never a write through a link. The answer is `conflict`.
- **Symlinks** in the backup directory's own path are the operator's choice. It is resolved once, when the server starts, must be a directory, and must not be inside the data directory (a backup into the data directory is refused anyway).
- **Without `[backup] dir`**, remote backups, backup verification and pruning are refused (`invalid_argument`, "the server has no backup directory: set [backup] dir"). Nothing is written anywhere by default.
- The backup directory should be writable only by the server's user. Anyone else who can write there can plant a backup for `verify` or `prune_archive` to read. Nothing they plant can make the server write outside the backup directory.

`iwctl backup <dir> <dest>` on a local data directory keeps its free destination path: the operating system's permissions are the boundary there (ADR 0045).

### `restore`: offline only

`iwctl restore --server ...` is refused at parsing (exit code 2): "restore writes a new data directory and doesn't touch a running store; run `iwctl restore` on the server's host, then start a server on the restored directory". The reasons:

- A restore makes a **new** data directory with a new history (ADR 0009). The running store can't switch to it in place without closing every namespace, cutting every client off and moving directories under a running process. That is a stop and a start, which the operator does better with their supervisor.
- An online restore *into a new directory on the server* would only save a shell on the server's host, and would add a second writable path and a large write that nothing then uses.
- A namespace-level online restore (bring back one namespace as a new namespace of the running store) is useful, but it is a new write path through the namespace log and needs its own design. It is out of this step.

### Under the memory limit

- **Checkpoint and backup are allowed in every memory state.** They write no WAL record, and a checkpoint is how the WAL shrinks. A backup streams its files in 1 MiB chunks; a WAL segment it cuts is read whole, which is at most a segment (64 MiB).
- **`verify` of the store is refused while refusing writes** (`resource_exhausted`). It builds a copy of each namespace by replay, which the limit doesn't count, and it is never urgent. Verifying a backup or the archive takes the same memory and is refused too.
- **Pruning is allowed.** It only removes files.

### Throttling backups

- `[backup] max_bytes_per_second` (default 0: unthrottled) caps how fast a backup copies. The `Backup` request can override it, so an operator can run one backup faster or slower than the default. `StoreOptions::backup.max_bytes_per_second` is the same setting for embedded stores, and `iwctl backup --max-bytes-per-second` for an offline backup.
- The throttle counts the bytes written to the backup's files and sleeps whenever the copy is ahead of its rate, measured from the start. A burst is at most one 1 MiB chunk.
- **A throttled backup holds checkpoints back longer.** Checkpoints wait for the whole copy (ADR 0009), so at 10 MiB/s a 50 GiB store holds them back for about 85 minutes, and the WAL grows by everything committed meanwhile. The guarantees and config docs say so, and the metrics show it: `iwdb_backup_running` is 1 for as long as checkpoints wait. Releasing the checkpointer between files would need the backup to re-plan when a checkpoint removes a segment it hasn't copied yet. That is ADR 0009's unbounded-retry alternative, and it stays rejected.
- Design rule 3: a test runs a checkpoint during a throttled backup. The checkpoint completes once the copy is done, the backup restores and verifies, and the restored state equals the store's at the backup's seq (canonical-state helper).

### Pruning the archive

`iwdb::prune_archive(archive, backup, dry_run)` (and the trait method, on the store's archive and a backup in the backup directory; and `iwctl archive prune <archive> --before <backup>` locally) removes what only restores to before `backup` need:

- **The backup and the archive must have the same history** (`HistoryMismatch`). The backup's manifest must be valid.
- **Per namespace the backup holds**, with `C` its oldest checkpoint in the backup:
  - an archived segment is removed only if the next archived segment starts at or before `C + 1`, so every record after `C` stays;
  - an archived checkpoint (an import's) is removed only if its seq is below `C`;
  - a namespace whose oldest checkpoint in the backup is missing (`C = 0`) loses nothing.
- **Namespaces the backup doesn't hold** (created after it, or dropped before it) are left alone. A restore to after the backup brings the ones created later back from the archive alone. A dropped namespace's history goes with older backups, which the operator decides about.
- **The last segment of each namespace is never removed**, since no later one proves where it ends. So a restore from the backup plus the archive can reach every seq it could reach before.
- **Order and crashes.** Files are removed oldest first, and the directory is synced at the end. An interruption leaves a contiguous suffix, which is again a valid archive.
- **A running store can keep archiving into it.** Prune doesn't take the archive's lock. The store only adds segments it removes from its own WAL, which are newer than anything in a backup it took earlier, so the two never touch the same file. A restore reading the archive while it is pruned can fail on a missing file; run them apart.
- `dry_run` lists what would go and removes nothing.

The rule protects one backup: the oldest the operator keeps. Pruning before a newer backup removes what the older ones need, which is the point.

### The archive in the data directory: not now

Recording the archive's path in the data directory, so that an offline `iwctl checkpoint` needn't be told it, is a layout change (version, an N-1 reader, a fixture: design rule 4). It would still be wrong if the archive moved. With a server, `iwctl --server ... checkpoint` asks the store, which knows its archive, so the offline case is rare. An offline `checkpoint` keeps requiring `--archive` or `--no-archive` (ADR 0012).

The server gets **`[store] archive`**: the WAL archive the store copies segments into before it removes them (`StoreOptions::archive`). Without it, a server can't archive at all.

### `iwctl --server <endpoint>`

`status`, `checkpoint`, `backup <name>`, `verify [--backup <name> | --archive]`, `archive prune --before <name>`, `namespaces`, `create-namespace`, `drop-namespace`, `indexes`, `create-index`, `drop-index`, `add-constraint`, `drop-constraint`, `requests` and `cancel <id>`. Each makes one call through `client::Remote` (the `Database` and `Admin` traits) and prints its answer in the existing text and `--json` forms. Credentials work as for `user` and `token`: `--token` or `IWDB_TOKEN`, `--user` with a password prompt, or a client certificate. `import`, `export` and `restore` stay offline and are refused with `--server`.

Exit codes for remote errors: `corrupt` is 1 (damage), as for local damage. A verify that finds problems is 1. `unavailable` is 3 (the server is busy or drains, which is like "locked"). Every other error is 4. Remote calls have no deadline: a backup takes as long as it takes.

### Visible

- Metrics: `iwdb_backup_running` (1 while a backup copies), `iwdb_backup_bytes_total` (bytes copied by backups, counted as they are written, so a throttled backup's progress shows), `iwdb_backups_total{outcome}` (`ok`, `failed`) and `iwdb_last_backup_timestamp_seconds`.
- The console's status page doesn't show backups, so it is unchanged. The metrics carry them.

## Consequences

- An operator can check, back up, verify, prune and manage a running server's namespaces, indexes, constraints and requests with `iwctl --server`. Only `restore`, `import` and `export` need the server's host.
- A server can archive its WAL (`[store] archive`) and back up only into `[backup] dir`.
- Remote verification takes memory the size of the largest namespace for its replay, uncounted, so it is refused while refusing writes.
- Backups and checkpoints still exclude each other. Throttling trades backup speed for disk bandwidth, not for checkpoint delay.
- Four new rows in the operation table, and in the role test.
