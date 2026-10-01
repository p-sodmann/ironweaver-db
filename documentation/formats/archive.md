# WAL archive, version 1

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage/src/archive.rs` and used by the checkpointer (`StoreOptions::archive`), `iwdb::verify` and `iwdb::restore`. Fixture: `crates/iwdb/tests/fixtures/archive-v1/`. Decisions: [ADR 0009](../adr/0009-backup-archive-restore.md).

A store with `StoreOptions::archive` copies every WAL segment into its archive directory before its checkpointer removes the segment from `wal/`. With a backup, the archive allows restoring to any later seq it holds; an archive that a store wrote from its creation holds its whole history, from seq 1.

```
<archive>/
  IWDBARCH                      marker: magic, version, history id, CRC32C (32 bytes)
  LOCK                          held with an exclusive lock by the store that archives into it
  <first seq, 20 digits>.wal    archived WAL segments, byte for byte (formats/wal.md)
  <first seq, 20 digits>.wal.tmp a segment being archived
```

## Marker (`IWDBARCH`, 32 bytes)

All integers little endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBARC\n` (`49 57 44 42 41 52 43 0a`) |
| 8 | 4 | version | `1` |
| 12 | 16 | history | the history id of the store whose segments it holds ([data-dir.md](data-dir.md)) |
| 28 | 4 | crc | CRC32C of bytes 0..28 |

An archive holds the segments of **one history**. A store opens its archive only if the marker's history is its own; otherwise it fails with `ArchiveMismatch`. A restored store has a new history and needs a new archive directory.

## Segments

Archived segments are copies of the store's segments, in the format they were written in (WAL format 1 or 2), named like them. They are complete: a segment is archived only when the checkpointer removes it, which is never the last one, and closed segments have no torn tail. They follow each other from the first archived segment to the last, like the segments of a log; an archive that a store started using later begins with the first segment it removed after that.

## Archiving

When a checkpoint run removes WAL segments (data-dir.md, step 5 of Writing), it first archives them:

1. for each segment: if the archive has a file of the same name, it must have the same bytes (otherwise `ArchiveConflict`, and nothing more happens); the segment is copied to `<name>.tmp` in 1 MiB chunks, fsynced, and renamed to `<name>`;
2. the archive directory is synced;
3. then the segments are removed from `wal/`, and `wal/` is synced.

So a segment leaves `wal/` only once its copy and its directory entry are durable in the archive. **Idempotent**: a crash between 2 and 3 leaves a segment in both places, and the next checkpoint that removes segments archives it again (the same bytes, rewritten rather than trusted, since its earlier fsync may not have completed) and removes it.

**Failures**: a failed copy (create, write, fsync, rename, a full disk, a conflict) fails the checkpoint run, and no segment is removed; the next checkpoint that writes a file retries. A failed sync of the archive directory disables checkpoints until the store is reopened, like any failed directory sync (a retried directory fsync can report success without making the renames durable). In both cases the WAL grows, and `Store::checkpoint_failure` reports the error. No record is ever lost: what isn't archived stays in `wal/`.

**Opening**: a missing directory is created; an empty one (or one with only `LOCK` and `*.tmp` files) is initialized: the marker is written with `write_atomic`, then the directory synced. A directory with other files and no marker is refused (`NotAnArchive`). The store holds the archive's lock while it is open (`Locked` for a second store). Readers (restore, verify) don't take the lock: segments appear by rename, complete, and `*.tmp` files are never read.

## Versioning

A change to the archive's marker or layout bumps `ARCHIVE_VERSION`, keeps a reader for version N-1, and adds a fixture next to `archive-v1/`. The segments carry their own WAL format version.
