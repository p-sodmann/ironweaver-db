# Data directory, layout version 1

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage` (`layout.rs`, `checkpoint.rs`, `recovery.rs`) and used by `iwdb::Store`. Fixture: `crates/iwdb/tests/fixtures/data-dir-v1/`. Decisions and alternatives: [ADR 0006](../adr/0006-checkpoints-and-recovery.md).

A store keeps one namespace (step 9 adds more) in one directory:

```
<dir>/
  IWDB                          marker: magic, layout version, CRC32C (16 bytes)
  LOCK                          empty; held with an exclusive lock while a store has the directory open
  checkpoints/
    <seq, 20 digits>.ckpt       checkpoints (below)
  wal/
    <first seq, 20 digits>.wal  WAL segments (formats/wal.md)
```

Temporary files end in `.tmp`: `.<name>.<pid>.<n>.tmp` (the core's `write_atomic`, for checkpoints and the marker) and `<segment>.tmp` (a WAL segment being created). They are never read, and are removed when a store opens.

## Marker (`IWDB`, 16 bytes)

All integers little endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBDIR\n` (`49 57 44 42 44 49 52 0a`) |
| 8 | 4 | version | `1`, the layout version |
| 12 | 4 | crc | CRC32C of bytes 0..12 |

The layout version covers everything in this document: the directory structure, file names, the marker, and the checkpoint content (the graph meta keys and their encoding). The WAL segments carry their own format version. Any change to this document bumps the layout version, keeps a reader for version N-1, and adds a fixture.

Opening a directory:

- **It has a marker** with a valid CRC and version 1: open it. `wal/` and `checkpoints/` must exist (otherwise `InvalidDataDir`).
- **Version above 1**: refused with `UnsupportedLayout`, before anything is changed.
- **Our magic, a wrong length or CRC**: refused with `InvalidDataDir`.
- **Another file named `IWDB`**: refused with `NotADataDir`.
- **No marker**: the directory may only hold what an interrupted initialization leaves (`LOCK`, empty `checkpoints/` and `wal/`, `*.tmp` files). With `create_if_missing` (the default), it is initialized; otherwise it is refused with `NotADataDir`. Anything else is refused with `NotADataDir`, and nothing is created in it, not even `LOCK`. A missing directory is created (with its parents) if `create_if_missing` is set.
- **Initialization**: take the lock, check again that there is no marker, create `checkpoints/` and `wal/`, sync the directory, write the marker with `write_atomic`, sync the directory. The marker comes last, so a directory with a marker is complete.

## Lock (`LOCK`)

The store opens `LOCK` (creating it if needed) and takes an exclusive, non-blocking lock on it: `flock(LOCK_EX | LOCK_NB)` on Unix, `LockFileEx` on Windows, through the `fs4` crate (std's `File::try_lock` needs Rust 1.89; our MSRV is 1.85). If the lock is held, opening fails with `Locked`.

- `flock` locks belong to the open file, not the process, so a second open in the same process fails too.
- The lock is released when the store is closed or dropped, and by the OS when the process exits, however it exits (tested with `kill -9`).
- It is advisory: it keeps out other stores and tools that take it (`iwctl`, step 7), not arbitrary programs. On network file systems `flock` may not work; they are not supported.
- The lock is taken after the marker is checked and before anything is changed, apart from creating `LOCK` itself.

## Checkpoints (`checkpoints/<seq>.ckpt`)

Name: the seq, as 20 decimal digits zero-padded, then `.ckpt`. Names sort like seqs. Only this exact form is a checkpoint.

Content: a graph file in the core's **binary format, version 2** (magic `IRONWEAV`, a CRC32 over the payload, a length trailer), written by `iwdb_engine::codec::write_binary`:

- nodes and edges carry their user attributes and meta, and the version as `iwdb.version` in their meta (see `codec.rs`);
- the graph meta holds exactly two keys: `iwdb.catalog`, the namespace's catalog as a JSON string ([ADR 0003](../adr/0003-catalog-storage.md)), and `iwdb.seq`, an `Int`.

**Contract.** A checkpoint at seq `S` holds exactly the state after the commits `1 ..= S`: data, versions and catalog. `S` equals the number in its name. It includes only commits that were synced to the WAL when it was taken, so the WAL always reaches at least to `S`, even after an OS crash (with the `always` and `group` policies; see below for `off`).

**Writing** (the checkpointer, [ADR 0006](../adr/0006-checkpoints-and-recovery.md)):

1. replay the WAL into the checkpointer's own namespace, up to a target seq that is at most the WAL's synced seq;
2. write `checkpoints/<target>.ckpt` with the core's `write_atomic`: a temporary file, fsynced, renamed over the name;
3. fsync `checkpoints/`, and check the result. The core's own directory sync ignores errors (upstream #32);
4. keep the new checkpoint and the newest `keep - 1` older ones that are not known to be damaged (`keep` defaults to 2). Remove every checkpoint older than the oldest one kept, then fsync `checkpoints/`;
5. remove every WAL segment whose records are all at or below the oldest kept checkpoint's seq (a segment ends right before the next one starts; the last segment is never removed), then fsync `wal/`.

A checkpoint is durable before anything is removed, and the WAL always holds every record after the oldest kept checkpoint. So recovery can start from any kept checkpoint.

**Failures.** If step 1 or 2 fails, nothing is removed, the previous checkpoints are untouched (`write_atomic` removes its temporary file), and the next checkpoint retries. If step 3, 4 or 5 fails, the store disables checkpoints until it is reopened (`CheckpointsDisabled`). A failed directory fsync is never retried, because a retry can succeed without persisting the entries. Commits are not affected; the WAL just isn't cut.

## Recovery

`Store::open` (`iwdb_storage::recover`) does this:

1. Open the directory and take the lock (above).
2. Remove stale `*.tmp` files in the directory, `checkpoints/` and `wal/`, and fsync each directory it removed one from.
3. Load the newest checkpoint that loads, streaming (`codec::from_binary_reader`: peak memory is the graph plus a buffer). A checkpoint that fails to load is **skipped**, and recovery tries the next older one, or an empty namespace at seq 0 if none is left. Failing to load means a checksum, length or format error, a catalog or version error, a seq that differs from the name, or another namespace's name. Skipped checkpoints are reported and left in place. The indexes are rebuilt from the loaded catalog (`NamespaceCatalog::apply_indexes`); differences from the indexes saved in the file are reported as `IndexChanges`, which are always empty for files the database wrote.
4. Replay the WAL from the checkpoint's seq + 1 to its end with `Namespace::replay`.
5. If the last segment has a torn tail, cut it: `set_len(valid_len)` and fsync. If `valid_len` is 0 (not even the header is valid), remove the segment and fsync `wal/`. The report includes the damage and `discarded_frames`: complete frames after the damage that were written before it was synced. That is possible only with `group` or `off` after an OS crash, and those commits were never durable.
6. Start the WAL writer at the log's next seq, in a new segment.

The result is the state after the last complete commit in the log. With `always`, that is every acknowledged commit. With `group`, it is every acknowledged commit except those an OS crash lost within the policy's window ([guarantees.md](../guarantees.md)).

**Errors.** In each of these cases open fails with a typed error, the lock is released, and nothing is truncated, removed or rewritten (only temporary files may have been removed):

| Case | Error |
|---|---|
| The directory is not ours, is newer, or is damaged | `NotADataDir`, `UnsupportedLayout`, `InvalidDataDir` |
| Another store has it open | `Locked` |
| The WAL doesn't reach back to the newest checkpoint that loads: newer ones are damaged and the records they covered were removed from the WAL. Fallback past the retention window is impossible | `NoUsableCheckpoint { from, first_seq, skipped }` |
| The WAL ends before the checkpoint's seq (WAL files removed by hand, or an OS crash with `off`) | `LogEndsBefore` |
| WAL corruption: damage that isn't a torn tail, a gap, an invalid record, an unknown segment version (formats/wal.md) | `Corrupt`, `SeqMismatch`, `InvalidRecord`, `HeaderMismatch`, `UnsupportedVersion`, `SegmentTooLarge` |
| A logged record fails to apply (`ApplyFailed`, including `GraphError::Internal`). This is a bug; report it | `ReplayFailed { seq, source }` |
| An I/O error, including a failed truncation (the next open retries it) | `Io` |

A panic during recovery (a core bug, upstream #28) is a crash: nothing was changed but temporary files and a torn tail, and the next open starts over.

**The `off` policy** (tests only) never fsyncs, so the checkpointer checkpoints up to the last applied seq. After an OS crash the WAL may then end before a checkpoint, and recovery refuses to open (`LogEndsBefore`). That is within what `off` allows.

## Versioning

Layout version 1 is this document. Checkpoints have no version of their own: the core's binary format version is checked by the core, and the meta keys and their encoding belong to the layout version (the catalog JSON also has its own `format`). A newer writer that adds a meta key, changes a file name or adds a directory must bump the layout version, so that an older reader refuses the directory instead of skipping its checkpoints as damaged.
