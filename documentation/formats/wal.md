# WAL format, version 1

Status: stable contract (design rule 4). Implemented in `crates/iwdb-storage` (`format.rs`, `reader.rs`, `writer.rs`). Fixture: `crates/iwdb-storage/tests/fixtures/wal-v1/`.

The write-ahead log of a namespace is a directory of **segment files**. Each segment holds a header and then a sequence of **record frames**, one per committed transaction (`iwdb_engine::CommitRecord`). Records are numbered by `seq`, without gaps, across segments.

## Conventions

- All integers are **little endian**.
- Nothing is aligned or padded: frames follow each other byte by byte.
- Checksums are **CRC32C** (Castagnoli polynomial, as in iSCSI and ext4), stored as `u32`.
- `seq` values are `1 ..= u64::MAX - 1`. `u64::MAX` is never used, so the seq after any record exists.

## Segment files

Name: the `seq` of the segment's first record, as **20 decimal digits, zero-padded**, then `.wal`: `00000000000000000001.wal`. Twenty digits hold every `u64`, so names sort like seqs. Only this exact form is a segment. Other files in the directory are ignored by the reader, in particular `<name>.tmp`, which is how a segment is created (below).

### Segment header (24 bytes)

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | `IWDBWAL\n` (`49 57 44 42 57 41 4c 0a`) |
| 8 | 4 | version | `1` |
| 12 | 8 | first_seq | seq of the segment's first record; equals the file name |
| 20 | 4 | crc | CRC32C of bytes 0..20 |

A segment may have no records (header only): its next seq is `first_seq`.

### Record frame (25 bytes + payload)

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 4 | len | payload length in bytes, at most `MAX_RECORD_LEN` = 64 MiB (67 108 864) |
| 4 | 8 | seq | the record's seq |
| 12 | 8 | synced_seq | the highest seq whose fsync had completed when this frame was written (0: none). Always `< seq` |
| 20 | 1 | kind | `1` = data change, `2` = catalog change. `0` and everything else are invalid |
| 21 | 4 | crc | CRC32C of bytes 0..21 of the frame, followed by the payload |
| 25 | len | payload | see below |

The CRC covers every header field (`len`, `seq`, `synced_seq`, `kind`) as well as the payload. So a damaged length or seq is caught like damaged data.

### Payload

The payload is postcard (`postcard` 1.x, serde) of the record's change, without the variant tag, because `kind` carries it:

- kind 1 (`Change::Data`): `Vec<ironweaver_core::Op<DbRecord, DbRecord>>`, the resolved core ops with explicit edge ids and versions (ADR 0004);
- kind 2 (`Change::Catalog`): `iwdb_engine::CatalogChange`.

The payload must decode completely, with no bytes left over. The serde encodings of `Op`, `Value`, `DbRecord` and the catalog types are part of this contract: `DbRecord` and `Value` dicts are written sorted by key, so equal records give equal bytes. The fixture test catches a change to any of them.

Every value in a logged record is nested at most `MAX_VALUE_DEPTH` deep, because the commit pipeline enforces it. That keeps records within the depth limit of `Value`'s serde (upstream #31).

## Writing

- **Creating a segment**: the header is written to `<name>.tmp`, which is fsynced and renamed to `<name>`, and then the directory is fsynced. So a segment file either exists with a valid header or doesn't exist.
- **Rotation**: before appending a frame that would make the segment larger than the configured segment size (1 KiB to 1 GiB, 64 MiB by default), the writer fsyncs the current segment and creates the next one, named by the frame's seq. A segment with no records takes any frame, so one frame can exceed the segment size. The largest segment file is therefore `1 GiB + 24 + 25 + 64 MiB` bytes, and the reader rejects larger files before it reads them.
- **Appending**: one `write` of the whole frame. It is then fsynced per the fsync policy (ADR 0005, [guarantees.md](../guarantees.md)).
- **Starting a writer** always creates a new segment at the next seq. The log must end right before it with no torn tail, and its last segment is fsynced first. A header-only segment with the same name is replaced.
- With the `off` policy, none of these fsyncs happen. A writer then starts with `synced_seq` 0 (it knows of no fsync), and only an explicit sync fsyncs: every segment that may hold records after `synced_seq` (rotations and earlier writers left them unsynced), then the directory.

## Reading

The reader lists the segments, starts at the last one whose `first_seq` is at or before the requested seq, and checks every frame from there:

1. The header must be valid, with `first_seq` equal to the file name. Each segment after the first one read must start at the seq after the previous segment's last record.
2. A frame is **damaged** if the file ends inside it (`Truncated`), its `len` is above `MAX_RECORD_LEN` (`BadLength`) or its CRC doesn't match (`Checksum`). A header is damaged if it is truncated or its magic or CRC is wrong. A corrupt length is rejected before anything is allocated.
3. A frame with a valid CRC must have the expected seq, `synced_seq < seq`, a known kind and a payload that decodes exactly. Anything else is an error, never a torn tail: the writer doesn't produce such frames, and a torn write can't (except with a chance of 2⁻³²).

### Torn tail or corruption

Damage in a segment **other than the last** is corruption (an error): a segment is fsynced before the next one is created.

Damage in the **last** segment at the frame for seq `d` is a **torn tail**, the clean end of the log, unless a later frame proves that record `d` had been synced. The reader looks for later frames at every offset after the damage. A candidate frame must have a seq in `d ..= d + (file length / 26)`, which rejects almost every offset before any CRC is computed, and then a valid CRC. If any such frame has `synced_seq >= d`, record `d` was durable before that frame was written, so the damage is corruption (an error). Otherwise the damage is a torn tail. The frames found after it were written before `d` was synced and are discarded with it.

With the `always` policy every frame has `synced_seq = seq - 1`, so any valid frame after damage makes it an error. Under `group` or `off`, an OS crash can write back the unsynced end of the file out of order (a lost page before a surviving one). That is a legitimate torn tail. `synced_seq` is what lets the reader tell the two apart without trusting the damaged bytes.

A damaged segment header in the last segment is a torn tail at offset 0 if no valid frame follows it, and corruption otherwise. Headers are synced before any frame is written, so any valid frame counts as proof.

The reader reports where the log ends (`LogEnd`): the next seq, and for the last segment its valid length and, if torn, the damage and the number of discarded frames. Recovery (step 5) truncates the segment to its valid length.

### Starting position

Reading from seq `s` fails with `MissingRecords` if the first segment starts after `s`, and with `LogEndsBefore` if the log ends before `s` (its next seq is below `s`). Records before `s` in the first segment read are checked but not returned. An empty directory is an empty log whose next seq is `s`.

A **bounded read** (`WalReader::open_until(dir, s, u)`, step 5) returns the records `s ..= u` and stops right after record `u`, without decoding anything after it. The checkpointer uses it to read the segment the writer is appending to, up to a synced seq: a frame in progress after `u` is never examined, so it can't be mistaken for damage. It fails with `LogEndsBefore` if the log ends before record `u`.

## Versioning

`version` in the segment header is the format version. A change to anything above bumps it, in `iwdb_storage::format::FORMAT_VERSION`. The reader keeps reading version N-1, and a fixture for each version stays in `tests/fixtures/wal-vN/` (design rule 4). A frame format or payload change needs a new segment version, because the version is only stored per segment.
