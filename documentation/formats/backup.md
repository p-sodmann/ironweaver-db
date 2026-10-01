# Backup directory and manifest, version 2

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage/src/backup.rs` and used by `iwdb::Store::backup`, `iwdb::verify` and `iwdb::restore`. Fixtures: `crates/iwdb/tests/fixtures/backup-v2/` (three namespaces) and `backup-v1/` (still read). Decisions: [ADR 0009](../adr/0009-backup-archive-restore.md), [ADR 0017](../adr/0017-namespaces.md).

A backup is a data directory in [layout 4](data-dir.md) with a manifest. It holds every namespace the store had when the backup started:

```
<backup>/
  IWDB                          the source's marker: layout 4, the source's history id
  BACKUP                        the manifest (below)
  NAMESPACES                    a copy of the source's namespace log, up to the last event before the backup
  ns/<id, 20 digits>/
    checkpoints/
      <seq, 20 digits>.ckpt     every checkpoint of the namespace at or below the backup's seq in it
    wal/
      <first seq, 20 digits>.wal  the segments from the one holding the oldest checkpoint's seq + 1
                                to the one holding the backup's seq, which is cut right after it
```

The checkpoints and segments are byte-for-byte copies of the source's files, except each namespace's last segment, which is a prefix of the source's: its header and every frame up to and including the record at the backup's seq **in that namespace** (each namespace has its own seq, listed in the manifest). So a namespace's WAL in the backup ends exactly at its seq, without a torn tail. A namespace backed up at a checkpoint's seq has no segments; an empty namespace has neither checkpoints nor segments. Checkpoints the source's checkpointer knows to be damaged are left out. **Consistency**: a backup is consistent per namespace, not across namespaces (commits never span two, so there is nothing to be consistent across); the backup takes every namespace's checkpointer lock, in id order, for its duration, and the store's namespace log lock, so no namespace is created or dropped meanwhile.

A version 1 backup (steps 7 and 8) has `checkpoints/` and `wal/` at the top and no `NAMESPACES`; it is one namespace, read as `default` (id 1).

A backup has no `LOCK` file. A store refuses to open it (`IsBackup`, [data-dir.md](data-dir.md)): it is restored instead (Restore, in data-dir.md), which gives the restored store a new history.

## Manifest (`BACKUP`)

All integers little endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBBAK\n` (`49 57 44 42 42 41 4b 0a`) |
| 8 | 4 | version | `2` (`1` is still read) |
| 12 | 4 | len | the length of the document, at most 64 MiB |
| 16 | len | document | JSON, UTF-8 (below) |
| 16 + len | 4 | crc | CRC32C of bytes `0 .. 16 + len` |

The file is exactly `20 + len` bytes. A wrong magic, version, length or CRC makes the manifest invalid (`InvalidManifest`).

The document is a JSON object with exactly these fields (unknown fields are refused):

| Field | Type | Meaning |
|---|---|---|
| `history` | string | the history id, 32 lowercase hex digits; equal to the marker's |
| `created` | number | when the backup was written (microseconds since 1970-01-01 UTC, the writer's clock) |
| `source` | string | the source data directory's path, as given to the backup (informational) |
| `namespaces` | array | the namespaces, by `id`: `{"id": <u64>, "name": "...", "seq": <u64>, "time": <number or null>}`. `seq` is the seq the backup reaches in that namespace: the last record of its WAL, or its newest checkpoint if it has no WAL. `time` is the commit time of record `seq` (microseconds since 1970-01-01 UTC, [wal.md](wal.md)); null if the backup doesn't hold that record or it has no time (WAL format 1) |
| `files` | array | every file of the backup other than `IWDB` and `BACKUP`, sorted by `path`: objects `{"path": "ns/<id>/checkpoints/<name>" or "ns/<id>/wal/<name>" or "NAMESPACES", "len": <bytes>, "crc32c": <CRC32C of the whole file>}` |

The version 1 document has `seq` and `time` at the top instead of `namespaces`, and paths `checkpoints/<name>` and `wal/<name>`.

A path must be `NAMESPACES`, or a checkpoint name in a listed namespace's `checkpoints/` or a segment name in its `wal/` (data-dir.md, wal.md); anything else makes the manifest invalid.

The manifest lets `verify` find what the files' own checksums can't: a missing file, an extra file, and a segment cut at a frame boundary (whose remaining frames are all valid).

## Writing

`Store::backup` holds the checkpointer's lock throughout (no checkpoint or segment is removed meanwhile), fsyncs the WAL, takes its synced seq as the backup's seq, and then writes through `LogFs`:

0. the namespace log lock and every namespace's checkpointer lock (in id order) are taken, and each WAL is fsynced; its synced seq is that namespace's seq;
1. the destination must be missing or an empty directory, and not inside the data directory; it is created, and its parent synced;
2. an empty `BACKUP` (created and fsynced), then `ns/` and each namespace's `checkpoints/` and `wal/`, and the destination synced;
3. each checkpoint, copied in 1 MiB chunks, then fsynced; each segment, read and checked frame by frame (`segment_prefix`), written and fsynced;
4. each namespace's `checkpoints/` and `wal/` synced; `NAMESPACES` written and synced;
5. the manifest, with `write_atomic` over the empty `BACKUP`, and the destination synced;
6. the marker, with `write_atomic`, and the destination synced.

Until step 6 the directory holds `BACKUP` and no marker. A store, `verify` and restore all refuse such a directory (`NotADataDir`), so an interrupted or failed backup is never taken for a complete one. A backup that failed after the marker's rename (its last directory sync) is complete. Before step 2 the directory is empty.

## Reading

`verify` checks a backup like a data directory (per namespace; the findings name the namespace), plus: the manifest's checksum and fields, its history equal to the marker's, every listed file present with its length and CRC32C, no other file in a namespace's `checkpoints/` or `wal/`, every namespace of the manifest present and in the copied log, and each WAL ending at its `seq`. Restore reads each namespace's `seq` as the latest the backup can restore to on its own.

## Versioning

A change to the manifest (its frame or its fields) bumps `MANIFEST_VERSION`, keeps a reader for version N-1, and adds a fixture next to `backup-v2/`. The rest of a backup is a data directory and follows the layout version.
