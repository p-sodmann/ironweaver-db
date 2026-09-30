# Step 4: Write-ahead log

Status: done
Milestone: M1 Embedded durable
Depends on: step 3

## Goal

Every commit is appended to a CRC-checked log before it is acknowledged, according to a configurable fsync policy.

## Tasks

- [x] Create `crates/iwdb-storage`.
- [x] Format spec `documentation/formats/wal.md`: segment header (magic, format version, first `seq`), records (length, `seq`, record kind, CRC32C, postcard-encoded resolved ops or catalog change).
- [x] Segment rotation by size; segment naming by first `seq`.
- [x] fsync policies `always`, `group` (batch every N ms / N records, like Redis `everysec`), `off`. Guarantees documented in `documentation/guarantees.md`.
- [x] Commit order: resolve → validate → append → fsync (per policy) → apply in memory → acknowledge. If the append or fsync fails, don't apply; the store becomes read-only until reopened.
- [x] Reader: iterate from a `seq`, stop cleanly at a torn or corrupt tail and report its position.
- [x] `cargo-fuzz` target for the reader.

## Notes from step 3

- The record to log is `iwdb_engine::CommitRecord { seq, change }` (serde, postcard). Its record kind is the `Change` variant: `Data` (resolved core ops) or `Catalog` (`CatalogChange`).
- The commit order maps onto `Namespace::prepare` / `prepare_catalog` (resolve + validate), then append and fsync, then `Namespace::apply`. `apply` rejects a record that another commit overtook (`OutOfOrder`). If applying fails, it returns `ApplyFailed` and poisons the namespace, which should also make the store read-only.
- The reader's records go through `Namespace::replay`, which applies them without validating them again and requires `seq` order without gaps.
- Values in ops are at most `MAX_VALUE_DEPTH` deep and versions at most `i64::MAX`, so every record the pipeline produces encodes and decodes (upstream #31 is why the depth rule is one level stricter than the file format's).

## Acceptance criteria

- With `always`, an acknowledged commit is readable after reopening the files.
- Truncated and bit-flipped tails are detected and treated as the end of the log.

## Outcome

- `crates/iwdb-storage`:
  - `format`: the WAL format, version 1 ([formats/wal.md](../formats/wal.md)). A segment header holds the magic, version, first `seq` and a CRC32C. A frame holds `len`, `seq`, `synced_seq`, `kind` and a CRC32C over all of them plus the payload, then the postcard payload. Records are at most 64 MiB, and segment names are the first `seq` zero-padded to 20 digits.
  - `Wal`: appends `CommitRecord`s and rotates by size (1 KiB to 1 GiB, 64 MiB by default). It creates segments through a temporary file, fsync, rename and directory fsync. It supports the fsync policies `always`, `group { max_delay, max_batch }` (with `sync_due` for a timer) and `off`. Any I/O error puts it into a failed, read-only state, and a failed fsync is never retried. `Wal::create` checks that the log ends right before its first `seq`, with no torn tail, and fsyncs the last segment.
  - `WalReader` / `read_log` / `read_segment`: iterate from a `seq` and report the end of the log (`LogEnd`: next `seq`, valid length, torn tail). A torn tail is told apart from corruption, and `seq` gaps, repeats and invalid records are errors.
  - `io::LogFs` / `LogFile`: the file operations behind a trait, the seam for fault injection.
  - `LoggedNamespace`: prepare → append → fsync → apply → acknowledge. It becomes read-only after a log failure or when the namespace is poisoned.
  - `Error`: typed errors for writing (`Io`, `ReadOnly`, `RecordTooLarge`, `Encode`, `OutOfOrder`, `InvalidOptions`, `LogAhead`, `TornTail`) and reading (`Corrupt`, `InvalidRecord`, `SeqMismatch`, `HeaderMismatch`, `UnsupportedVersion`, `SegmentTooLarge`, `MissingRecords`, `LogEndsBefore`).
- [ADR 0005](../adr/0005-wal-fsync-and-failures.md) covers the fsync policies, group-commit semantics, failure behaviour, `synced_seq`, and `F_FULLFSYNC` on macOS. [guarantees.md](../guarantees.md) states what each policy can lose.
- Tests:
  - `wal_durability`: random workloads (the step 3 strategies) committed with `always`, then reread and replayed. The records, canonical state, catalog and `seq` match, also when reading from later seqs and across writer sessions and rotations. Also: fsync counts per policy, group commit by count and age, the record size limit and the writer start rules.
  - `wal_faults`: failed writes (whole and partial), failed fsyncs (`always`, `group`, `sync_due`) and a failure at every step of a rotation, through an injecting `LogFs`. In every case the commit is not applied, the namespace becomes read-only, nothing is retried, and the log holds every acknowledged commit.
  - `wal_damage`: truncation at every byte offset, every single-bit flip, random multi-byte damage, damage in earlier segments, gaps, repeats, and out-of-order loss of unsynced group-commit records.
  - `wal_fixture`: a committed format-1 segment must read as its records, and the writer must reproduce it byte for byte.
  - Unit tests and proptests over arbitrary bytes in `format` and `reader`.
- `fuzz/wal_reader`: a cargo-fuzz target in its own nightly workspace, run for 60 s in CI. It ran locally for 120 s (4.4M runs, 256 MiB malloc limit) with no findings.
- No new findings in `ironweaver-core`.

### Changes to this step, and why

- **`synced_seq` in every frame** (not in the original task list). Under `group`, an OS crash can lose an unsynced record while a later unsynced one survives (out-of-order write-back). Without a marker, the reader would have to either refuse such a log, which makes group mode unrecoverable after a power loss, or end the log at the first damage, which silently drops commits after corruption in a synced log. With `synced_seq`, damage is corruption exactly when a later valid frame proves the damaged record had been synced ([ADR 0005](../adr/0005-wal-fsync-and-failures.md)). It costs 8 bytes per record.
- **The frame's `kind` byte replaces the variant tag of `Change`**, so the payload is the variant's contents. The kind isn't stored twice, and an unknown kind (from a newer writer) is reported as such.
- **Group commit batches by count and by age, with no background thread.** The append that closes a batch fsyncs it. `sync_due` bounds the loss window in time and needs a timer, which the store (step 5) provides.
- **A failed commit's outcome is unknown.** After a failed fsync, the record may still reach the disk and be recovered. The guarantees say so, and step 8's idempotency keys make retries safe.
- **Segments are created complete** (temporary file, fsync, rename, directory fsync), and the old segment is fsynced before rotation. So only the last segment can have a torn tail, and damage anywhere else is corruption.
- **A new writer always starts a new segment**, after checking that the log ends right before it (no torn tail, no gap, no overlap) and fsyncing the last segment. Step 5 must truncate a torn tail before it starts the writer.
- **`LoggedNamespace`** (a namespace and its log) is the composition this step needs for its tests. The embedded `Store` in step 5 builds on it.
- **Workload strategies are shared.** The step 3 strategies moved to `crates/iwdb-engine/tests/workload/mod.rs`, which the storage tests include with `#[path]`. This is test code only; `iwdb-engine` itself is unchanged.
- **The design doc's "group (every N ms)"** is now "every N ms or N records".

## Non-goals

- Checkpoints and recovery (step 5).
