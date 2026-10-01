# ADR 0017: Namespaces: one WAL per namespace, a namespace log, data-dir layout 4

Status: accepted
Date: 2026-10-01

## Context

Step 9 gives a store several named graphs. Every namespace needs a graph, a commit position, a WAL, checkpoints, a key table, indexes and constraints. The questions are how namespaces share (or don't share) the log, what a seq and a history mean then, how a namespace is created and dropped without a crash leaving half of it, and what backup, archiving, restore and verify do with several of them.

## Options

**A shared WAL**: one log for the store, every record tagged with its namespace, one seq space. Pro: one fsync stream (group commit batches across namespaces), a seq that orders everything, a restore to a seq that is consistent for the whole store, create and drop as ordinary records. Con: the engine's seq is per namespace today (`Namespace::apply` demands `seq + 1`, replay checks for gaps), so every namespace would see gaps or the engine would change; checkpoints can only cut the log up to the *slowest* namespace's checkpoint (one idle namespace pins the whole WAL); recovery reads every namespace's records to rebuild one; a record format change (format 4) and a second code path in every reader; one writer for all namespaces (no parallelism between namespaces).

**One WAL per namespace**: each namespace is today's store in a subdirectory. Pro: the commit pipeline, WAL, checkpointer, recovery, key table and their tests stay as they are (design rules 2 and 3 keep their meaning per namespace); writers of different namespaces run in parallel (one writer mutex and one `RwLock` each, ADR 0014); a namespace is checkpointed, cut, backed up and dropped independently; no WAL format change. Con: there is no store-wide order of commits; fsyncs are per namespace; "restore to a seq" needs a namespace.

## Decision

**One WAL per namespace**, plus a small store-level **namespace log**. Commits stay single-namespace: a transaction changes one namespace (design rule 8 and the plan of steps 10 to 12 route requests per namespace; cross-namespace transactions are out of scope).

### Layout 4 ([data-dir.md](../formats/data-dir.md))

```
<dir>/IWDB, LOCK
<dir>/NAMESPACES                  the namespace log
<dir>/ns/<id, 20 digits>/checkpoints/, wal/
```

A namespace's files are named by its **id**, not its name: ids are assigned at creation, grow, and are never reused, so a name that is dropped and created again can't be confused with its earlier life (in directories, backups, archives, the namespace log). Names (`NamespaceName`, as since step 2): 1 to 64 ASCII letters, digits, `_` or `-`, starting with a letter or digit; case sensitive. `default` is created with the store and can't be dropped, so that the shorthand methods of `Store` (`commit`, `node`, ...) and of the Python `Store` always have a target; a store restored without it gets an empty one when it opens.

### The namespace log

`NAMESPACES` is an append-only file of events (`create` and `drop`, each with id, name, time and an optional idempotency key), CRC-framed, fsynced per event, numbered from 1. It is the only truth about which namespaces exist. It is not a WAL of data, but it is the same kind of artifact (a framed, checksummed, torn-tail-tolerant log), with its own magic and version (data-dir.md). Every namespace operation is two steps with a defined commit point, so that no crash leaves a half-made or half-dropped namespace:

- **Create**: make `ns/<id>/` with `checkpoints/` and `wal/`, synced; *then* append the create event (fsync). A crash before leaves a directory the log doesn't list; the next open removes it. After the event the namespace exists, empty, whatever else the process did.
- **Drop**: stop the namespace's commits, fsync its WAL, archive its remaining segments if the store archives (so a restore to a time before the drop still finds them); *then* append the drop event (fsync); *then* remove `ns/<id>/`. A crash before the event leaves the namespace (it was only quiet); after it, the namespace is gone and the next open removes whatever is left of its directory.
- **Open**: read the log (cut a torn tail), recover the namespaces it lists, remove directories it doesn't list. A listed namespace whose directories are missing is `NamespaceDamaged` (files lost, not a crash state).

If appending the event fails, its outcome is unknown, so the log is *failed*: namespace operations are refused until the store is reopened, which reads the log and tells. A failure after the event is durable (opening the new namespace) fails the log the same way.

### Locks

Lock order: **namespace log mutex** (create, drop, backup) → a namespace's **checkpointer** → its **writer** (WAL) → its **namespace** `RwLock`; the registry of open namespaces is an `RwLock` held only for lookups (never while taking another lock, except by create and drop, which hold the log mutex). Nothing takes two namespaces' writer or namespace locks. A backup takes every checkpointer in id order, so two backups can't deadlock (they also serialize on the log mutex). Commits on different namespaces don't contend with each other at all.

### Seqs, histories, `min_seq`

Every namespace has its **own seq space** starting at 1; a seq means nothing without its namespace. The **history id** is the store's (the marker's), shared by all namespaces and by the namespace log: a restore gives the restored store a new one (ADR 0009). `ReadOptions.min_seq` waits on the addressed namespace; `ReadOptions.history` is the store's, as before. A dropped namespace wakes its waiters with `NamespaceDropped`; a read that started finishes on the state it read; commits fail with `NamespaceDropped`. A namespace created again under the same name is a new namespace (new id, seq 1): a `min_seq` of the old one is meaningless to it, and a client that holds one across a drop and create must compare ids (`Ns::id`).

### Checkpoints

Per namespace, by the same rules as before (WAL size and time triggers per namespace, the checkpointer replays the namespace's own WAL, `keep`, archive before removal). One idle namespace pins nothing else.

### Backup, archive, restore

- **Backup** is of the whole store: each namespace consistent up to its own synced seq (no commit spans namespaces, so no cross-namespace snapshot is needed), plus the namespace log as of the moment of the backup. The manifest lists each namespace with its seq and commit time (manifest version 2, [backup.md](../formats/backup.md)).
- **Archive** (format 2, [archive.md](../formats/archive.md)): `ns/<id>/` directories of segments and a copy of the namespace log, brought up to date after every namespace operation (a failure to copy is logged and retried, not an error: the copy only limits how far a restore can see). A format 1 archive is namespace 1's; a store archiving into one upgrades it in place (rename the segments into `ns/1/`, then the marker).
- **Restore** is of a whole store, optionally limited to named namespaces (`only`). It reads the namespace logs of the backup and the archive (one must be a prefix of the other), decides which namespaces existed at the target, restores each from its own checkpoints and segments, and writes the namespace log of the new history (the events up to the target, with their keys). Targets: `Latest` (each namespace to the end of what the sources reach), `Time(t)` (the namespaces that existed at `t`, each at its last record at or before `t`; one created before `t` with no commit yet is empty), `Seq(n)` only when the restore selects exactly one namespace (seqs of several namespaces can't be compared; `AmbiguousTarget` otherwise). A namespace created after the backup is restored from the archive alone, from seq 1, as far as the archive reaches (the segments the checkpointer cut, or all of them if it was dropped); one dropped after the backup is restored from the backup and the archive's segments if the target is before its drop.
- **Verify** checks the namespace log, the directories against it (an unlisted directory is a note: a crash leaves it, the next open removes it; a missing one is a problem), and every namespace like a layout 3 directory. A backup's manifest is checked per namespace.

### Upgrading layouts 1 to 3

An existing store becomes a layout 4 store with one namespace, `default`, id 1, keeping its history id, its seq space, its key table and its checkpoints and WAL byte for byte: the upgrade renames `checkpoints/` and `wal/` into `ns/<1>/` and writes `NAMESPACES` with one create event at time 0 (a namespace that predates the log has no known creation time; restore to a time treats it as always there), then replaces the marker (the commit point). It runs after the namespace has been read successfully, and every step can be repeated after a crash: the old marker stays until the end, and recovery finds `checkpoints/` and `wal/` in whichever place they are (`DataDir::legacy_paths`).

## Consequences

- No WAL format change: format 3 stays. The data-dir layout (4), the backup manifest (2) and the archive (2) change, each with a reader for the version before and a fixture.
- No store-wide commit order and no store-wide PITR seq. A restore to a time is the store-wide point; its resolution is the WAL's (microseconds, non-decreasing per namespace, ADR 0010), and clocks of different namespaces come from the same process clock.
- `Store` keeps its single-namespace methods as shorthands for `default`; reports (`StoreRecovery`, `BackupReport`, `RestoreReport`, `DirStatus`) have per-namespace parts and dereference to `default`'s for single-namespace callers.
- Each open namespace costs a WAL writer (one open segment), a checkpointer copy of its graph while checkpointing, and a thread-free state; there is one checkpoint thread and one group-commit thread for the store.
- Drop on Windows may leave the directory (open handles) until the next open removes it; the log is already right.
