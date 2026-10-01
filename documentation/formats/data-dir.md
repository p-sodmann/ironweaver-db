# Data directory, layout version 3

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage` (`layout.rs`, `history.rs`, `checkpoint.rs`, `recovery.rs`) and `iwdb_engine::codec` / `idempotency`, used by `iwdb::Store`. Fixtures: `crates/iwdb/tests/fixtures/data-dir-v3/` and, for older layouts, `data-dir-v2/` and `data-dir-v1/`. Decisions and alternatives: [ADR 0006](../adr/0006-checkpoints-and-recovery.md) (layout 1), [ADR 0009](../adr/0009-backup-archive-restore.md) (layout 2: histories, backups, restore) and [ADR 0015](../adr/0015-idempotency-keys.md) (layout 3: the idempotency key table in checkpoints).

A store keeps one namespace (step 9 adds more) in one directory:

```
<dir>/
  IWDB                          marker: magic, layout version, history id, CRC32C (32 bytes)
  LOCK                          empty; held with an exclusive lock while a store has the directory open
  checkpoints/
    <seq, 20 digits>.ckpt       checkpoints (below)
  wal/
    <first seq, 20 digits>.wal  WAL segments (formats/wal.md)
  BACKUP                        only in a backup: its manifest (formats/backup.md)
  RESTORING                     only while a restore writes the directory (Restore, below)
```

Layout 3 (step 8) differs from layout 2 in the checkpoints' graph meta, which holds the idempotency key table (`iwdb.keys`), and in the marker's version. Layout 2 (step 7) differed from layout 1 (step 5) in the marker, which holds a history id since, and in the `BACKUP` and `RESTORING` files. The WAL segments carry their own format version (formats/wal.md).

Temporary files end in `.tmp`: `.<name>.<pid>.<n>.tmp` (the core's `write_atomic`, for checkpoints and the marker) and `<segment>.tmp` (a WAL segment being created). They are never read, and are removed when a store opens.

## Marker (`IWDB`, 32 bytes)

All integers little endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBDIR\n` (`49 57 44 42 44 49 52 0a`) |
| 8 | 4 | version | `3`, the layout version (`2` in a layout 2 marker, otherwise the same) |
| 12 | 16 | history | the history id: 16 random bytes (printed as 32 hex digits) |
| 28 | 4 | crc | CRC32C of bytes 0..28 |

Every layout keeps this frame: the magic, the version at offset 8, then the layout's own fields, and a CRC32C of everything before it as the last 4 bytes. So a reader can check the CRC of a marker of any length and tell a newer layout from damage. (Layout 1's marker is the frame without fields: 16 bytes, CRC at 12. Versions before step 7 checked the length first, and report a layout 2 directory as damaged, `InvalidDataDir`, rather than newer.)

**The history id** names the history of commits the directory holds. A new directory gets a random one, and so does every restore ([ADR 0009](../adr/0009-backup-archive-restore.md)): after a restore to seq `N`, the restored store's commits `N + 1, ...` differ from the original's. A backup keeps the id of the store it copies; a WAL archive holds one history's segments ([archive.md](archive.md)). A store archives only into an archive of its own history, and a restore combines a backup and an archive only if their histories match.

The layout version covers everything in this document: the directory structure, file names, the marker, and the checkpoint content (the graph meta keys and their encoding). The WAL segments carry their own format version. Any change to this document bumps the layout version, keeps a reader for version N-1, and adds a fixture.

Opening a directory:

- **It has a `RESTORING` file**: refused with `InterruptedRestore`, before anything is changed (Restore, below).
- **It has a marker and a `BACKUP` file**: a backup, refused with `IsBackup`. A backup is restored, not opened: so it stays as it was, and two stores never continue one history.
- **It has a marker** with a valid CRC and version 3: open it. `wal/` and `checkpoints/` must exist (otherwise `InvalidDataDir`).
- **Version 1 or 2**: open it. Once recovery has read it successfully (and before the WAL writer starts), its marker is replaced by a layout 3 marker (`write_atomic`, then a directory sync): with the same history id (layout 2), or a new random one (layout 1, which has none). Nothing else changes: its checkpoints have no `iwdb.keys` and load with an empty key table. The recovery report says `upgraded_from: 1` or `2`. A crash leaves either marker. A failed open leaves the old one. Older versions can't open the directory afterwards (step 7 refuses layout 3 with `UnsupportedLayout`).
- **Version above 3**: refused with `UnsupportedLayout`, before anything is changed.
- **Our magic, a wrong length or CRC**: refused with `InvalidDataDir`.
- **Another file named `IWDB`**: refused with `NotADataDir`.
- **No marker**: the directory may only hold what an interrupted initialization leaves (`LOCK`, empty `checkpoints/` and `wal/`, `*.tmp` files). With `create_if_missing` (the default), it is initialized; otherwise it is refused with `NotADataDir`. Anything else is refused with `NotADataDir`, and nothing is created in it, not even `LOCK`: an interrupted backup or restore leaves such a directory. A missing directory is created (with its parents) if `create_if_missing` is set.
- **Initialization**: take the lock, check again that there is no marker, create `checkpoints/` and `wal/`, sync the directory, write the marker (layout 3, a new random history id) with `write_atomic`, sync the directory. The marker comes last, so a directory with a marker is complete.

## Lock (`LOCK`)

The store opens `LOCK` (creating it if needed) and takes an exclusive, non-blocking lock on it: `flock(LOCK_EX | LOCK_NB)` on Unix, `LockFileEx` on Windows, through the `fs4` crate (std's `File::try_lock` needs Rust 1.89; our MSRV is 1.85). If the lock is held, it tries again a few times over about 80 ms, then opening fails with `Locked`. (A process that another thread spawns holds a copy of every open file until it execs, and so, for that moment, the lock of a store that was just closed; without the retries a reopen failed now and then. Step 7 found and fixed this.)

- `flock` locks belong to the open file, not the process, so a second open in the same process fails too. For the same reason a child process **forked** without exec (Python's `multiprocessing` with the `fork` start method, `os.fork()`) inherits the lock: the store stays locked until that child exits, even after the parent closes it. Don't fork while a store is open, or use the `spawn` start method.
- The lock is released when the store is closed or dropped, and by the OS when the process exits, however it exits (tested with `kill -9`).
- It is advisory: it keeps out other stores and tools that take it (`iwctl`, step 7), not arbitrary programs. On network file systems `flock` may not work; they are not supported.
- The lock is taken after the marker is checked and before anything is changed, apart from creating `LOCK` itself.
- **Readers** that must not run while a store has the directory open (`verify`, and restore reading a data directory or backup) take a **shared** lock (`flock(LOCK_SH | LOCK_NB)`) on `LOCK` if the file exists, and never create it. They fail with `Locked` while a store has the directory open, and keep stores out while they read. Shared locks don't exclude each other.

## Checkpoints (`checkpoints/<seq>.ckpt`)

Name: the seq, as 20 decimal digits zero-padded, then `.ckpt`. Names sort like seqs. Only this exact form is a checkpoint.

Content: a graph file in the core's **binary format, version 2** (magic `IRONWEAV`, a CRC32 over the payload, a length trailer), written by `iwdb_engine::codec::write_binary`:

- nodes and edges carry their user attributes and meta, and the version as `iwdb.version` in their meta (see `codec.rs`);
- the graph meta holds exactly three keys: `iwdb.catalog`, the namespace's catalog as a JSON string ([ADR 0003](../adr/0003-catalog-storage.md)); `iwdb.seq`, an `Int`; and `iwdb.keys`, the idempotency key table (below). A checkpoint written before layout 3 has no `iwdb.keys`: its key table is empty.

**The key table** (`iwdb.keys`, step 8): a JSON string `{"format": 1, "entries": [...]}`, entries by seq, one per keyed commit the namespace remembers: `{"seq", "key", "fingerprint", "time", "edge_ids", "versions"}`, where `time` is microseconds since 1970 UTC or `null`, `edge_ids` a list of numbers, and `versions` a list of `[{"n": "<node id>"} | {"e": <edge id>}, version]`. It holds at most 10 000 entries (`KEY_TABLE_CAPACITY`): the keyed commits with the highest seqs at or below `S`. Loading refuses (as a damaged checkpoint) a table of another format, with more entries, with seqs not strictly increasing or outside `1 ..= S`, with a key twice or an invalid key, or with unknown fields. The table is a function of the records `1 ..= S` (ADR 0015): `verify` compares it with the WAL replay.

**Contract.** A checkpoint at seq `S` holds exactly the state after the commits `1 ..= S`: data, versions, catalog and key table. `S` equals the number in its name. It includes only commits that were synced to the WAL when it was taken, so the WAL always reaches at least to `S`, even after an OS crash (with the `always` and `group` policies; see below for `off`).

**Writing** (the checkpointer, [ADR 0006](../adr/0006-checkpoints-and-recovery.md)):

1. replay the WAL into the checkpointer's own namespace, up to a target seq that is at most the WAL's synced seq;
2. write `checkpoints/<target>.ckpt` with the core's `write_atomic`: a temporary file, fsynced, renamed over the name;
3. fsync `checkpoints/`, and check the result. The core's own directory sync ignores errors (upstream #32);
4. keep the new checkpoint and the newest `keep - 1` older ones that are not known to be damaged (`keep` defaults to 2). Remove every checkpoint older than the oldest one kept, then fsync `checkpoints/`;
5. remove every WAL segment whose records are all at or below the oldest kept checkpoint's seq (a segment ends right before the next one starts; the last segment is never removed), then fsync `wal/`.

A checkpoint is durable before anything is removed, and the WAL always holds every record after the oldest kept checkpoint. So recovery can start from any kept checkpoint.

**Failures.** If step 1 or 2 fails, nothing is removed, the previous checkpoints are untouched (`write_atomic` removes its temporary file), and the next checkpoint retries. If step 3, 4 or 5 fails, the store disables checkpoints until it is reopened (`CheckpointsDisabled`). A failed directory fsync is never retried, because a retry can succeed without persisting the entries. Commits are not affected; the WAL just isn't cut.

**Interrupted cleanup.** A crash or a failure after step 2 leaves checkpoints and WAL segments that steps 4 and 5 would have removed. The next checkpoint that writes a new file removes them. A run with nothing new to write (the newest checkpoint is at the target) removes nothing. Its newest checkpoint may be one whose directory entry was never synced (a crash between steps 2 and 3), and removing files on the strength of it could lose data after an OS crash. The extra files cost space only. Recovery reads them correctly, and the step 6 crash tests cover these points.

## Recovery

`Store::open` (`iwdb_storage::recover`) does this:

1. Open the directory and take the lock (above).
2. Remove stale `*.tmp` files in the directory, `checkpoints/` and `wal/`, and fsync each directory it removed one from.
3. Load the newest checkpoint that loads, streaming (`codec::from_binary_reader`: peak memory is the graph plus a buffer). A checkpoint that fails to load is **skipped**, and recovery tries the next older one, or an empty namespace at seq 0 if none is left. Failing to load means a checksum, length or format error, a catalog or version error, a seq that differs from the name, or another namespace's name. Skipped checkpoints are reported and left in place. The indexes are rebuilt from the loaded catalog (`NamespaceCatalog::apply_indexes`); differences from the indexes saved in the file are reported as `IndexChanges`, which are always empty for files the database wrote.
4. Replay the WAL from the checkpoint's seq + 1 to its end with `Namespace::replay`.
5. If the last segment has a torn tail, cut it: `set_len(valid_len)` and fsync. If `valid_len` is 0 (not even the header is valid), remove the segment and fsync `wal/`. The report includes the damage and `discarded_frames`: complete frames after the damage that were written before it was synced. That is possible only with `group` or `off` after an OS crash, and those commits were never durable.
6. If the directory is in layout 1, upgrade its marker (Marker, above).
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
3. **Target.** A seq; a time: the last record in seq order whose commit time is at or before it ([wal.md](wal.md), "Commit time"; `NoCommitAtOrBefore` if there is none); or the latest seq the sources reach (a backup alone: its manifest's seq).
4. **Replay.** The newest backup checkpoint at or below `N` that loads (older ones on failure, then an empty namespace at seq 0) is loaded and the log replayed onto it up to `N`. `MissingRecords` if the log doesn't reach back to the checkpoint's seq + 1; `LogEndsBefore` if it ends before `N`.
5. **Write.** The destination must be missing or empty, and not inside a source. In this order, through `LogFs`: a `RESTORING` file, fsynced; `LOCK` (locked), `checkpoints/` and `wal/`; the directory synced; the checkpoint at `N` (`write_atomic`; none if `N` is 0) and `checkpoints/` synced; `RESTORING` removed and the directory synced; the marker with a **new** history id (`write_atomic`) and the directory synced.

The result opens as a store at `N` with an empty WAL: its first commit is `N + 1`, in a new history, which needs a new archive directory. Its checkpoint holds the key table at `N`: the keys of the restored commits, and none of the commits after `N`, which the new history doesn't contain (ADR 0015). An interrupted restore leaves either `RESTORING` (open refuses with `InterruptedRestore`), or a checkpoint without a marker (`NotADataDir`), or, for a restore to seq 0 or a failure before anything was written, an empty directory. It is never finished by the next open: remove the directory and restore again. A restore never writes to its sources.

## Versioning

Layout version 3 is this document. Layout 2 is the same without `iwdb.keys` in checkpoints; layout 1 is layout 2 without the history id in the marker and without `BACKUP` and `RESTORING`. Both are upgraded when a store opens them, and read as they are by `verify` and restore. Checkpoints have no version of their own: the core's binary format version is checked by the core, and the meta keys and their encoding belong to the layout version (the catalog JSON also has its own `format`). A newer writer that adds a meta key, changes a file name or adds a directory must bump the layout version, so that an older reader refuses the directory instead of skipping its checkpoints as damaged.
