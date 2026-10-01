# Backup directory and manifest, version 1

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage/src/backup.rs` and used by `iwdb::Store::backup`, `iwdb::verify` and `iwdb::restore`. Fixture: `crates/iwdb/tests/fixtures/backup-v1/`. Decisions: [ADR 0009](../adr/0009-backup-archive-restore.md).

A backup is a data directory in [layout 2](data-dir.md) with a manifest:

```
<backup>/
  IWDB                          the source's marker: layout 2, the source's history id
  BACKUP                        the manifest (below)
  checkpoints/
    <seq, 20 digits>.ckpt       every checkpoint of the source at or below the backup's seq
  wal/
    <first seq, 20 digits>.wal  the segments from the one holding the oldest checkpoint's seq + 1
                                to the one holding the backup's seq, which is cut right after it
```

The checkpoints and segments are byte-for-byte copies of the source's files, except the last segment, which is a prefix of the source's: its header and every frame up to and including the record at the backup's seq. So the backup's WAL ends exactly at its seq, without a torn tail. A backup at a checkpoint's seq has no segments; a backup of an empty store (seq 0) has neither checkpoints nor segments. Checkpoints the source's checkpointer knows to be damaged are left out.

A backup has no `LOCK` file. A store refuses to open it (`IsBackup`, [data-dir.md](data-dir.md)): it is restored instead (Restore, in data-dir.md), which gives the restored store a new history.

## Manifest (`BACKUP`)

All integers little endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBBAK\n` (`49 57 44 42 42 41 4b 0a`) |
| 8 | 4 | version | `1` |
| 12 | 4 | len | the length of the document, at most 64 MiB |
| 16 | len | document | JSON, UTF-8 (below) |
| 16 + len | 4 | crc | CRC32C of bytes `0 .. 16 + len` |

The file is exactly `20 + len` bytes. A wrong magic, version, length or CRC makes the manifest invalid (`InvalidManifest`).

The document is a JSON object with exactly these fields (unknown fields are refused):

| Field | Type | Meaning |
|---|---|---|
| `history` | string | the history id, 32 lowercase hex digits; equal to the marker's |
| `seq` | number | the seq the backup reaches: the last record of its WAL, or its newest checkpoint if it has no WAL |
| `time` | number or null | the commit time of record `seq` (microseconds since 1970-01-01 UTC, [wal.md](wal.md)); null if the backup doesn't hold that record or it has no time (WAL format 1) |
| `created` | number | when the backup was written (microseconds since 1970-01-01 UTC, the writer's clock) |
| `source` | string | the source data directory's path, as given to the backup (informational) |
| `files` | array | every file of the backup other than `IWDB` and `BACKUP`, sorted by `path`: objects `{"path": "checkpoints/<name>" or "wal/<name>", "len": <bytes>, "crc32c": <CRC32C of the whole file>}` |

A path must be a checkpoint name in `checkpoints/` or a segment name in `wal/` (data-dir.md, wal.md); anything else makes the manifest invalid.

The manifest lets `verify` find what the files' own checksums can't: a missing file, an extra file, and a segment cut at a frame boundary (whose remaining frames are all valid).

## Writing

`Store::backup` holds the checkpointer's lock throughout (no checkpoint or segment is removed meanwhile), fsyncs the WAL, takes its synced seq as the backup's seq, and then writes through `LogFs`:

1. the destination must be missing or an empty directory, and not inside the data directory; it is created, and its parent synced;
2. an empty `BACKUP` (created and fsynced), then `checkpoints/` and `wal/`, and the destination synced;
3. each checkpoint, copied in 1 MiB chunks, then fsynced; each segment, read and checked frame by frame (`segment_prefix`), written and fsynced;
4. `checkpoints/` and `wal/` synced;
5. the manifest, with `write_atomic` over the empty `BACKUP`, and the destination synced;
6. the marker, with `write_atomic`, and the destination synced.

Until step 6 the directory holds `BACKUP` and no marker. A store, `verify` and restore all refuse such a directory (`NotADataDir`), so an interrupted or failed backup is never taken for a complete one. A backup that failed after the marker's rename (its last directory sync) is complete. Before step 2 the directory is empty.

## Reading

`verify` checks a backup like a data directory, plus: the manifest's checksum and fields, its history equal to the marker's, every listed file present with its length and CRC32C, no other file in `checkpoints/` or `wal/`, and the WAL ending at `seq`. Restore reads the manifest's `seq` as the latest seq the backup can restore to on its own.

## Versioning

A change to the manifest (its frame or its fields) bumps `MANIFEST_VERSION`, keeps a reader for version N-1, and adds a fixture next to `backup-v1/`. The rest of a backup is a data directory and follows the layout version.
