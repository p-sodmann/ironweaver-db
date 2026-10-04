# WAL archive, version 3

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage/src/archive.rs` and used by the checkpointer (`StoreOptions::archive`), `iwdb::verify` and `iwdb::restore`. Fixtures: `crates/iwdb/tests/fixtures/archive-v3/` (four namespaces: one dropped, one imported), `archive-v2/` and `archive-v1/` (still read). Decisions: [ADR 0009](../adr/0009-backup-archive-restore.md), [ADR 0017](../adr/0017-namespaces.md), [ADR 0033](../adr/0033-bulk-import-export.md) (version 3: checkpoints of imports).

A store with `StoreOptions::archive` copies every WAL segment into its archive directory before its checkpointer removes the segment from `wal/`. With a backup, the archive allows restoring to any later seq it holds; an archive that a store wrote from its creation holds its whole history, from seq 1.

```
<archive>/
  IWDBARCH                      marker: magic, version, history id, CRC32C (32 bytes)
  LOCK                          held with an exclusive lock by the store that archives into it
  NAMESPACES                    a copy of the store's namespace log, kept up to date (best effort)
  ns/<id, 20 digits>/
    <first seq, 20 digits>.wal  archived WAL segments of the namespace, byte for byte (formats/wal.md)
    <seq, 20 digits>.ckpt       the checkpoint an imported namespace starts from (version 3)
    <name>.tmp                  a segment or checkpoint being archived
```

Every namespace archives into its own `ns/<id>/`: a seq space is per namespace, so each directory is one log from the first archived segment on. **A dropped namespace's directory stays**, with the segments up to and including the ones the drop archived (its remaining segments are archived before the drop event is logged), so the archive holds the history of namespaces that no longer exist and a restore to a time before the drop can bring them back. The copy of the namespace log tells restore which namespaces existed when; it is rewritten after each create or drop, and a failure to update it is logged and doesn't fail the operation (the store's own log is the truth, and the next create or drop rewrites the copy).

A **version 1** archive (steps 7 and 8: one namespace, segments at the top, no `NAMESPACES`) is namespace 1's, read as such. A store that archives into one upgrades it in place when it opens it (`Archive::open`): `ns/` and `ns/00000000000000000001/` are created, the segments renamed into it, and the marker replaced with a version 2 one (the commit point; a crash before it leaves a version 1 archive some of whose segments have moved, which the next open finishes).

A **version 2** archive (step 9) is version 3 without checkpoints. A store that archives into one replaces its marker with a version 3 one; nothing else changes.

## Marker (`IWDBARCH`, 32 bytes)

All integers little endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBARC\n` (`49 57 44 42 41 52 43 0a`) |
| 8 | 4 | version | `3` (`1` and `2` are still read) |
| 12 | 16 | history | the history id of the store whose segments it holds ([data-dir.md](data-dir.md)) |
| 28 | 4 | crc | CRC32C of bytes 0..28 |

An archive holds the segments of **one history**. A store opens its archive only if the marker's history is its own; otherwise it fails with `ArchiveMismatch`. A restored store has a new history and needs a new archive directory.

## Segments

Archived segments are copies of the namespace's segments, in the format they were written in (WAL format 1 or 2), named like them. They are complete: a segment is archived only when the checkpointer removes it, which is never the last one, and closed segments have no torn tail. They follow each other from the first archived segment to the last, like the segments of a log; an archive that a store started using later begins with the first segment it removed after that.

## Checkpoints of imports (version 3)

An imported namespace starts from a checkpoint at seq 1, and its WAL from seq 2 ([ADR 0033](../adr/0033-bulk-import-export.md)): no segment holds the import. So the archive holds that checkpoint, `ns/<id>/00000000000000000001.ckpt`, byte for byte as the store wrote it (the core's binary format with the database's graph meta, [data-dir.md](data-dir.md)). The import copies it after its create event (as `<name>.tmp`, fsynced, renamed, and the directory synced) and before it acknowledges; a store that opens with an archive copies it for every namespace that has a checkpoint at seq 1 and no record 1 in its WAL or the archive, which covers a crash in between and an archive set up after the import. A copy that is there already must have the same bytes (`ArchiveConflict` otherwise). Restore uses archived checkpoints as bases, like a backup's; `verify` loads each and checks that the segments go on from it.

## Archiving

When a checkpoint run removes WAL segments (data-dir.md, step 5 of Writing), it first archives them:

1. for each segment of the namespace: if the archive has a file of the same name, it must have the same bytes (otherwise `ArchiveConflict`, and nothing more happens); the segment is copied to `<name>.tmp` in 1 MiB chunks, fsynced, and renamed to `<name>`;
2. the archive directory is synced;
3. then the segments are removed from `wal/`, and `wal/` is synced.

So a segment leaves `wal/` only once its copy and its directory entry are durable in the archive. **Idempotent**: a crash between 2 and 3 leaves a segment in both places, and the next checkpoint that removes segments archives it again (the same bytes, rewritten rather than trusted, since its earlier fsync may not have completed) and removes it.

**Failures**: a failed copy (create, write, fsync, rename, a full disk, a conflict) fails the checkpoint run, and no segment is removed; the next checkpoint that writes a file retries. A failed sync of the archive directory disables checkpoints until the store is reopened, like any failed directory sync (a retried directory fsync can report success without making the renames durable). In both cases the WAL grows, and `Store::checkpoint_failure` reports the error. No record is ever lost: what isn't archived stays in `wal/`.

**Opening**: a missing directory is created; an empty one (or one with only `LOCK` and `*.tmp` files) is initialized: the marker is written with `write_atomic`, then the directory synced. A directory with other files and no marker is refused (`NotAnArchive`). The store holds the archive's lock while it is open (`Locked` for a second store). Readers (restore, verify) don't take the lock: segments appear by rename, complete, and `*.tmp` files are never read.

## Versioning

A change to the archive's marker or layout bumps `ARCHIVE_VERSION`, keeps a reader for version N-1, and adds a fixture next to `archive-v3/`. Version 3 (step 13) added the checkpoints of imports; version 2 (step 9) the namespaces. The segments carry their own WAL format version.
