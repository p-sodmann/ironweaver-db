# Data directory, layout version 5

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage` (`layout.rs`, `history.rs`, `checkpoint.rs`, `recovery.rs`) and `iwdb_engine::codec` / `idempotency` / `mark`, used by `iwdb::Store`. Fixtures: `crates/iwdb/tests/fixtures/data-dir-v5/` (three namespaces with marks and a dropped one) and, for older layouts, `data-dir-v4/`, `data-dir-v3/`, `data-dir-v2/` and `data-dir-v1/`. Decisions and alternatives: [ADR 0006](../adr/0006-checkpoints-and-recovery.md) (layout 1), [ADR 0009](../adr/0009-backup-archive-restore.md) (layout 2: histories, backups, restore) [ADR 0015](../adr/0015-idempotency-keys.md) (layout 3: the idempotency key table in checkpoints) and [ADR 0017](../adr/0017-namespaces.md), [ADR 0018](../adr/0018-namespace-keys-and-restore.md) (layout 4: namespaces) and [ADR 0032](../adr/0032-projection-mode.md) (layout 5: marks in checkpoints).

A store keeps its namespaces in one directory, each with its own WAL and checkpoints:

```
<dir>/
  IWDB                          marker: magic, layout version, history id, CRC32C (32 bytes)
  LOCK                          empty; held with an exclusive lock while a store has the directory open
  NAMESPACES                    the namespace log: every namespace ever created or dropped (below)
  ns/
    <id, 20 digits>/            one directory per live namespace
      checkpoints/
        <seq, 20 digits>.ckpt   checkpoints (below)
        import.staged           only after a crash during an import: its checkpoint (Import, below)
      wal/
        <first seq, 20 digits>.wal  WAL segments (formats/wal.md)
  BACKUP                        only in a backup: its manifest (formats/backup.md)
  RESTORING                     only while a restore writes the directory (Restore, below)
```

Layout 5 (step 13) differs from layout 4 in the checkpoints' graph meta, which holds the namespace's marks (`iwdb.marks`), in `checkpoints/import.staged` (Import, below) and in the marker's version; its stores write WAL format 4. Layout 4 (step 9) differed from layout 3 in the directory structure (`checkpoints/` and `wal/` moved under `ns/<id>/`, `NAMESPACES` added) and the marker's version. Checkpoints and WAL segments are unchanged byte for byte: **the WAL format stays at 3**, and a checkpoint's contents are those of layout 3. Layout 3 (step 8) differed from layout 2 in the checkpoints' graph meta, which holds the idempotency key table (`iwdb.keys`). Layout 2 (step 7) differed from layout 1 (step 5) in the marker, which holds a history id since, and in the `BACKUP` and `RESTORING` files. The WAL segments carry their own format version (formats/wal.md).

**One WAL per namespace** ([ADR 0017](../adr/0017-namespaces.md)): every namespace has its own seq space (its commits are `1, 2, ...`), its own segments and checkpoints, its own idempotency key table, marks, indexes and constraints. A commit goes to one namespace. The history id is the store's, shared by all of them.

Temporary files end in `.tmp`: `.<name>.<pid>.<n>.tmp` (the core's `write_atomic`, for checkpoints and the marker), `<segment>.tmp` (a WAL segment being created) and `ns/import-<pid>-<time>-<n>.tmp` (an import's checkpoint, before it is placed). They are never read, and are removed when a store opens.

## Marker (`IWDB`, 32 bytes)

All integers little endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBDIR\n` (`49 57 44 42 44 49 52 0a`) |
| 8 | 4 | version | `4`, the layout version (`2` or `3` in an older marker, otherwise the same) |
| 12 | 16 | history | the history id: 16 random bytes (printed as 32 hex digits) |
| 28 | 4 | crc | CRC32C of bytes 0..28 |

Every layout keeps this frame: the magic, the version at offset 8, then the layout's own fields, and a CRC32C of everything before it as the last 4 bytes. So a reader can check the CRC of a marker of any length and tell a newer layout from damage. (Layout 1's marker is the frame without fields: 16 bytes, CRC at 12. Versions before step 7 checked the length first, and report a layout 2 directory as damaged, `InvalidDataDir`, rather than newer.)

**The history id** names the history of commits the directory holds. A new directory gets a random one, and so does every restore ([ADR 0009](../adr/0009-backup-archive-restore.md)): after a restore to seq `N`, the restored store's commits `N + 1, ...` differ from the original's. A backup keeps the id of the store it copies; a WAL archive holds one history's segments ([archive.md](archive.md)). A store archives only into an archive of its own history, and a restore combines a backup and an archive only if their histories match.

The layout version covers everything in this document: the directory structure, file names, the marker, and the checkpoint content (the graph meta keys and their encoding). The WAL segments carry their own format version. Any change to this document bumps the layout version, keeps a reader for version N-1, and adds a fixture.

Opening a directory:

- **It has a `RESTORING` file**: refused with `InterruptedRestore`, before anything is changed (Restore, below).
- **It has a marker and a `BACKUP` file**: a backup, refused with `IsBackup`. A backup is restored, not opened: so it stays as it was, and two stores never continue one history.
- **It has a marker** with a valid CRC and version 5: open it. `NAMESPACES` and `ns/` must exist, and every live namespace's `checkpoints/` and `wal/` (otherwise `InvalidDataDir` or `NamespaceDamaged`).
- **Version 4**: open it as version 5. Once recovery has read every namespace, and before it writes anything (the WAL writers start after it), the **upgrade** replaces the marker with a layout 5 one with the same history id (`write_atomic`, then a directory sync). Nothing else changes: a layout 4 checkpoint is a layout 5 checkpoint without marks. A failed open leaves the old marker. The recovery report says `upgraded_from: 4`. Older versions can't open the directory afterwards (they refuse layout 5 with `UnsupportedLayout`).
- **Version 1, 2 or 3**: open it. Recovery reads its one namespace where it is (`checkpoints/` and `wal/` in the directory itself, or already under `ns/00000000000000000001/` if an earlier upgrade was interrupted), and once that has succeeded the **upgrade** runs, each step repeatable: create `ns/` and `ns/00000000000000000001/` and sync; rename `checkpoints/` and `wal/` into it (each rename is atomic) and sync; write `NAMESPACES` with one create event for `default` (id 1, time 0) and sync; replace the marker with a current one (`write_atomic`, then a directory sync), with the same history id (layouts 2 and 3) or a new one (layout 1, which has none). **The marker is the commit point**: a crash before it leaves the old marker and the next open finds the files where they are and does the rest (the crash tests kill the upgrade at each file operation). A failed open leaves the old marker. The namespace is `default` with its history, seq, checkpoints, key table and keys unchanged. The recovery report says `upgraded_from: 1`, `2` or `3`. Older versions can't open the directory afterwards (step 8 refuses layout 4 with `UnsupportedLayout`).
- **Version above 5**: refused with `UnsupportedLayout`, before anything is changed.
- **Our magic, a wrong length or CRC**: refused with `InvalidDataDir`.
- **Another file named `IWDB`**: refused with `NotADataDir`.
- **No marker**: the directory may only hold what an interrupted initialization leaves (`LOCK`, empty `checkpoints/` and `wal/`, `*.tmp` files). With `create_if_missing` (the default), it is initialized; otherwise it is refused with `NotADataDir`. Anything else is refused with `NotADataDir`, and nothing is created in it, not even `LOCK`: an interrupted backup or restore leaves such a directory. A missing directory is created (with its parents) if `create_if_missing` is set.
- **Initialization**: take the lock, check again that there is no marker, create `ns/` and the `default` namespace's directory (`ns/00000000000000000001/{checkpoints,wal}`), write `NAMESPACES` with the create event of `default` (id 1), sync the directory, write the marker (layout 5, a new random history id) with `write_atomic`, sync the directory. The marker comes last, so a directory with a marker is complete.

## Namespaces (`NAMESPACES`, `ns/`)

**The log** is the only truth about which namespaces exist. It is small and append-only; every event is fsynced before it is acknowledged. All integers little endian.

| Part | Size | Content |
|---|---|---|
| header | 16 | magic `IWDBNSL\n`, version `u32` (`1`), CRC32C of the 12 bytes before |
| frame | 24 + len | `len u32`, `seq u64`, `time i64`, CRC32C `u32`, payload (JSON, at most 64 KiB) |

The CRC covers `len`, `seq`, `time` and the payload. `seq` numbers the events `1, 2, ...` without gaps. `time` is microseconds since 1970 UTC (non-decreasing; 0 for the `default` event of an upgraded store). The payload is `{"op": "create" | "drop", "id": <u64>, "name": "...", "key": "...", "fingerprint": <u32>}` (`key` and `fingerprint` only if the request had an idempotency key; unknown fields are refused). A **name** is 1 to 64 ASCII letters, digits, `_` or `-`, starting with a letter or digit; names are case sensitive. An **id** is assigned at creation (the next after the largest ever logged) and never reused; `default` is id 1 and can't be dropped.

**Torn tail.** Events are fsynced one at a time, so an incomplete last frame, or a complete last frame with a wrong CRC, is an event that was never acknowledged: recovery cuts it. Damage followed by a valid frame is corruption (`InvalidNamespaceLog`).

**Create.** (1) create `ns/<id>/` with its `checkpoints/` and `wal/`, and sync; (2) append the create event: **the commit point**. A crash before (2) leaves an empty directory the log doesn't know, which the next open removes. **Drop.** (1) mark the namespace dropped (commits and waits on it fail with `NamespaceDropped`; a read that started finishes) and fsync its WAL; (2) with an archive, copy its remaining segments there; (3) append the drop event: **the commit point**; (4) remove `ns/<id>/` and sync `ns/`. A crash after (3) leaves a directory of a dropped namespace, which the next open removes.

**Import** ([ADR 0033](../adr/0033-bulk-import-export.md)): a namespace created with content, its first state a checkpoint at seq 1 and its WAL starting at seq 2. (1) Write the checkpoint to `ns/import-*.tmp` and fsync it; (2) create `ns/<id>/` as for a create, move the file to `ns/<id>/checkpoints/import.staged`, and sync both directories; (3) append the create event: **the commit point**; (4) rename `import.staged` to `00000000000000000001.ckpt` and sync. `import.staged` is not a checkpoint: a crash before (3) leaves a directory without data, which the next open removes (and the `.tmp` file of a crash before (2)). After (3), recovery finishes the import: in a listed namespace, `import.staged` is renamed to checkpoint 1 if the namespace has no checkpoint, and removed otherwise (step 2 of Recovery, below). `verify` reads a staged import as the namespace's checkpoint 1.

**On open**, recovery reads the log, recovers every live namespace, and removes directories the log doesn't list: always for a dropped namespace; for one the log never mentions only if it holds no data. A directory with data that the log doesn't list means the log lost its create event (damage to the last event looks like a torn tail), and removing it would destroy a namespace: the open fails with `NamespaceDamaged`, changing nothing. A live namespace whose directory is missing is also `NamespaceDamaged`. `verify` reports all of these.

**Idempotency keys** of creates and drops live in the log ([ADR 0018](../adr/0018-namespace-keys-and-restore.md)); the keys of data and catalog commits stay in each namespace's key table.

## Lock (`LOCK`)

The store opens `LOCK` (creating it if needed) and takes an exclusive, non-blocking lock on it: `flock(LOCK_EX | LOCK_NB)` on Unix, `LockFileEx` on Windows, through std's `File::try_lock`. If the lock is held, it tries again a few times over about 80 ms, then opening fails with `Locked`. (A process that another thread spawns holds a copy of every open file until it execs, and so, for that moment, the lock of a store that was just closed; without the retries a reopen failed now and then. Step 7 found and fixed this.)

- `flock` locks belong to the open file, not the process, so a second open in the same process fails too. For the same reason a child process **forked** without exec (Python's `multiprocessing` with the `fork` start method, `os.fork()`) inherits the lock: the store stays locked until that child exits, even after the parent closes it. Don't fork while a store is open, or use the `spawn` start method.
- The lock is released when the store is closed or dropped, and by the OS when the process exits, however it exits (tested with `kill -9`).
- It is advisory: it keeps out other stores and tools that take it (`iwctl`, step 7), not arbitrary programs. On network file systems `flock` may not work; they are not supported.
- The lock is taken after the marker is checked and before anything is changed, apart from creating `LOCK` itself.
- **Readers** that must not run while a store has the directory open (`verify`, and restore reading a data directory or backup) take a **shared** lock (`flock(LOCK_SH | LOCK_NB)`) on `LOCK` if the file exists, and never create it. They fail with `Locked` while a store has the directory open, and keep stores out while they read. Shared locks don't exclude each other.

## Checkpoints (`ns/<id>/checkpoints/<seq>.ckpt`)

Name: the seq, as 20 decimal digits zero-padded, then `.ckpt`. Names sort like seqs. Only this exact form is a checkpoint.

Content: a graph file in the core's **binary format, version 2** (magic `IRONWEAV`, a CRC32 over the payload, a length trailer), written by `iwdb_engine::codec::write_binary`:

- nodes and edges carry their user attributes and meta, and the version as `iwdb.version` in their meta (see `codec.rs`);
- the graph meta holds exactly four keys: `iwdb.catalog`, the namespace's catalog as a JSON string ([ADR 0003](../adr/0003-catalog-storage.md)); `iwdb.seq`, an `Int`; `iwdb.keys`, the idempotency key table (below); and `iwdb.marks`, the marks (below). A checkpoint written before layout 3 has no `iwdb.keys`: its key table is empty. A checkpoint written before layout 5 has no `iwdb.marks`: it has no marks.

**The key table** (`iwdb.keys`, step 8): a JSON string `{"format": 1, "entries": [...]}`, entries by seq, one per keyed commit the namespace remembers: `{"seq", "key", "fingerprint", "time", "edge_ids", "versions"}`, where `time` is microseconds since 1970 UTC or `null`, `edge_ids` a list of numbers, and `versions` a list of `[{"n": "<node id>"} | {"e": <edge id>}, version]`. It holds at most 10 000 entries (`KEY_TABLE_CAPACITY`): the keyed commits with the highest seqs at or below `S`. Loading refuses (as a damaged checkpoint) a table of another format, with more entries, with seqs not strictly increasing or outside `1 ..= S`, with a key twice or an invalid key, or with unknown fields. The table is a function of the records `1 ..= S` (ADR 0015): `verify` compares it with the WAL replay.

**The marks** (`iwdb.marks`, step 13, [ADR 0032](../adr/0032-projection-mode.md)): a JSON string `{"format": 1, "marks": [...]}`, marks by name (strictly increasing, by byte), each `{"name", "position", "seq"}`: the mark's position (at most `i64::MAX`) and the seq of the commit that set it. At most 1 024 marks (`MAX_MARKS`). Loading refuses (as a damaged checkpoint) a table of another format, with more marks, names out of order or twice, an invalid name, a seq outside `1 ..= S`, a position above `i64::MAX`, or unknown fields. Like the key table, the marks are a function of the records `1 ..= S`: `verify` compares them with the WAL replay.

**Contract.** A checkpoint at seq `S` holds exactly the state after the commits `1 ..= S`: data, versions, catalog, key table and marks. `S` equals the number in its name. It includes only commits that were synced to the WAL when it was taken, so the WAL always reaches at least to `S`, even after an OS crash (with the `always` and `group` policies; see below for `off`).

**Writing** (the checkpointer, [ADR 0006](../adr/0006-checkpoints-and-recovery.md)):

1. replay the WAL into the checkpointer's own namespace, up to a target seq that is at most the WAL's synced seq;
2. write `checkpoints/<target>.ckpt` with the core's `write_atomic`: a temporary file, fsynced, renamed over the name;
3. fsync `checkpoints/`, and check the result. The core's own directory sync ignores errors (upstream #32);
4. keep the new checkpoint and the newest `keep - 1` older ones that are not known to be damaged (`keep` defaults to 2). Remove every checkpoint older than the oldest one kept, then fsync `checkpoints/`;
5. remove every WAL segment whose records are all at or below the oldest kept checkpoint's seq (a segment ends right before the next one starts; the last segment is never removed) and that the WAL retention doesn't keep for the change stream (`WalRetention`, step 13: segments holding one of the last `records` commits, or a commit younger than `age`), then fsync `wal/`. Retention changes no file format: it only keeps segments longer.

A checkpoint is durable before anything is removed, and the WAL always holds every record after the oldest kept checkpoint. So recovery can start from any kept checkpoint.

**Failures.** If step 1 or 2 fails, nothing is removed, the previous checkpoints are untouched (`write_atomic` removes its temporary file), and the next checkpoint retries. If step 3, 4 or 5 fails, the store disables checkpoints until it is reopened (`CheckpointsDisabled`). A failed directory fsync is never retried, because a retry can succeed without persisting the entries. Commits are not affected; the WAL just isn't cut.

**Interrupted cleanup.** A crash or a failure after step 2 leaves checkpoints and WAL segments that steps 4 and 5 would have removed. The next checkpoint that writes a new file removes them. A run with nothing new to write (the newest checkpoint is at the target) removes nothing. Its newest checkpoint may be one whose directory entry was never synced (a crash between steps 2 and 3), and removing files on the strength of it could lose data after an OS crash. The extra files cost space only. Recovery reads them correctly, and the step 6 crash tests cover these points.

## Recovery

`Store::open` (`iwdb_storage::recover`) does this:

1. Open the directory and take the lock (above).
2. Read `NAMESPACES` (cut a torn tail), remove the namespace directories it doesn't list (above), and remove stale `*.tmp` files in the directory, `ns/` and each namespace's `checkpoints/` and `wal/`, fsyncing each directory it removed one from. Finish a staged import in each live namespace (Import, above). Steps 3 to 7 run for every live namespace (in parallel in the store's open, one WAL writer each).
3. Load the newest checkpoint that loads, streaming (`codec::from_binary_reader`: peak memory is the graph plus a buffer). A checkpoint that fails to load is **skipped**, and recovery tries the next older one, or an empty namespace at seq 0 if none is left. Failing to load means a checksum, length or format error, a catalog or version error, a seq that differs from the name, or another namespace's name. Skipped checkpoints are reported and left in place. The indexes are rebuilt from the loaded catalog (`NamespaceCatalog::apply_indexes`); differences from the indexes saved in the file are reported as `IndexChanges`, which are always empty for files the database wrote.
4. Replay the WAL from the checkpoint's seq + 1 to its end with `Namespace::replay`.
5. If the last segment has a torn tail, cut it: `set_len(valid_len)` and fsync. If `valid_len` is 0 (not even the header is valid), remove the segment and fsync `wal/`. The report includes the damage and `discarded_frames`: complete frames after the damage that were written before it was synced. That is possible only with `group` or `off` after an OS crash, and those commits were never durable.
6. If the directory is in layout 1 to 3, upgrade it (Marker, above).
7. Start the WAL writer at the log's next seq, in a new segment.

The result is the state after the last complete commit in the log. With `always`, that is every acknowledged commit. With `group`, it is every acknowledged commit except those an OS crash lost within the policy's window ([guarantees.md](../guarantees.md)).

**Errors.** In each of these cases open fails with a typed error, the lock is released, and nothing is truncated, removed or rewritten (only temporary files may have been removed):

| Case | Error |
|---|---|
| The directory is not ours, is newer, or is damaged | `NotADataDir`, `UnsupportedLayout`, `InvalidDataDir` |
| A backup, or an interrupted restore | `IsBackup`, `InterruptedRestore` |
| Another store has it open | `Locked` |
| The WAL doesn't reach back to the newest checkpoint that loads: newer ones are damaged and the records they covered were removed from the WAL. Fallback past the retention window is impossible | `NoUsableCheckpoint { from, first_seq, skipped }` |
| The WAL ends before the checkpoint's seq (WAL files removed by hand, or an OS crash with `off`) | `LogEndsBefore` |
| WAL corruption: damage that isn't a torn tail, a gap, an invalid record, an unknown segment version (formats/wal.md) | `Corrupt`, `SeqMismatch`, `InvalidRecord`, `HeaderMismatch`, `UnsupportedVersion`, `SegmentTooLarge` |
| A logged record fails to apply (`ApplyFailed`, including `GraphError::Internal`). This is a bug; report it | `ReplayFailed { seq, source }` |
| An I/O error, including a failed truncation (the next open retries it) | `Io` |

A panic during recovery (a core bug, upstream #28) is a crash: nothing was changed but temporary files and a torn tail, and the next open starts over.

**The `off` policy** (tests only) never fsyncs, so the checkpointer checkpoints up to the last applied seq. After an OS crash the WAL may then end before a checkpoint, and recovery refuses to open (`LogEndsBefore`). That is within what `off` allows.

## Restore

`iwdb::restore` (`iwdb_storage::restore`) writes a new data directory at a target seq `N` from a backup (or a data directory no store has open), a WAL archive ([archive.md](archive.md)), or both ([ADR 0009](../adr/0009-backup-archive-restore.md)):

1. **Sources.** A backup is read under a shared lock on its `LOCK` (`Locked` if a store has it open) and must have a marker (an interrupted backup has none: `NotADataDir`) and, if it has one, a valid manifest ([backup.md](backup.md)). An archive must have its marker. Their history ids must be equal (`HistoryMismatch`; a layout 1 directory has none, so it can't be combined with an archive).
2. **One log.** The backup's and the archive's segments are read as one log. Where both have a segment of the same name, one must be a prefix of the other (the backup's copy of the segment it cut), and the longer is used; otherwise `ArchiveConflict`.
3. **Target.** Every namespace that existed at the target is restored (or just those named in `only`). A seq is one namespace's seq, so it needs exactly one namespace (`AmbiguousTarget` otherwise); a time: the last record in seq order whose commit time is at or before it ([wal.md](wal.md), "Commit time"; `NoCommitAtOrBefore` if there is none); or the latest seq the sources reach, in each namespace (a backup alone: its manifest's seqs). A namespace created at or before a time target with no commit by then is restored empty, at seq 0; one dropped before it is not restored. A restored store without `default` gets an empty one when it is opened.
4. **Replay.** The newest backup checkpoint at or below `N` that loads (older ones on failure, then an empty namespace at seq 0) is loaded and the log replayed onto it up to `N`. `MissingRecords` if the log doesn't reach back to the checkpoint's seq + 1; `LogEndsBefore` if it ends before `N`.
5. **Write.** The destination must be missing or empty, and not inside a source. In this order, through `LogFs`: a `RESTORING` file, fsynced; `LOCK` (locked) and `ns/`, the directory synced; for each restored namespace its directory with `checkpoints/` and `wal/`, its checkpoint at its target (`write_atomic`; none at seq 0) and `checkpoints/` synced; `NAMESPACES` (the log's events up to the target, with their keys) and the directory synced; `RESTORING` removed and the directory synced; the marker with a **new** history id (`write_atomic`) and the directory synced.

The result opens as a store at `N` with an empty WAL: its first commit is `N + 1`, in a new history, which needs a new archive directory. Its checkpoint holds the key table at `N`: the keys of the restored commits, and none of the commits after `N`, which the new history doesn't contain (ADR 0015). An interrupted restore leaves either `RESTORING` (open refuses with `InterruptedRestore`), or a checkpoint without a marker (`NotADataDir`), or, for a restore to seq 0 or a failure before anything was written, an empty directory. It is never finished by the next open: remove the directory and restore again. A restore never writes to its sources.

## Versioning

Layout version 5 is this document. Layout 4 is the same without `iwdb.marks` in checkpoints and without `import.staged`, and wrote WAL format 3. (`import.staged` was added to layout 5 before layout 5 was released, in the same step.) Layout 3 is layout 4 with one namespace and no `NAMESPACES`, its `checkpoints/` and `wal/` in the directory itself; layout 2 is layout 3 without `iwdb.keys` in checkpoints; layout 1 is layout 2 without the history id in the marker and without `BACKUP` and `RESTORING`. All are upgraded when a store opens them, and read as they are by `verify` and restore. Checkpoints have no version of their own: the core's binary format version is checked by the core, and the meta keys and their encoding belong to the layout version (the catalog JSON also has its own `format`). A newer writer that adds a meta key, changes a file name or adds a directory must bump the layout version, so that an older reader refuses the directory instead of skipping its checkpoints as damaged.
