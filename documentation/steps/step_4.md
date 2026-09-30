# Step 4: Write-ahead log

Status: todo
Milestone: M1 Embedded durable
Depends on: step 3

## Goal

Every commit is appended to a CRC-checked log before it is acknowledged, according to a configurable fsync policy.

## Tasks

- [ ] Create `crates/iwdb-storage`.
- [ ] Format spec `documentation/formats/wal.md`: segment header (magic, format version, first `seq`), records (length, `seq`, record kind, CRC32C, postcard-encoded resolved ops or catalog change).
- [ ] Segment rotation by size; segment naming by first `seq`.
- [ ] fsync policies `always`, `group` (batch every N ms / N records, like Redis `everysec`), `off`. Guarantees documented in `documentation/guarantees.md`.
- [ ] Commit order: resolve → validate → append → fsync (per policy) → apply in memory → acknowledge. If the append or fsync fails, don't apply; the store becomes read-only until reopened.
- [ ] Reader: iterate from a `seq`, stop cleanly at a torn or corrupt tail and report its position.
- [ ] `cargo-fuzz` target for the reader.

## Notes from step 3

- The record to log is `iwdb_engine::CommitRecord { seq, change }` (serde, postcard). Its record kind is the `Change` variant: `Data` (resolved core ops) or `Catalog` (`CatalogChange`).
- The commit order maps onto `Namespace::prepare` / `prepare_catalog` (resolve + validate), then append and fsync, then `Namespace::apply`. `apply` rejects a record that another commit overtook (`OutOfOrder`). If applying fails, it returns `ApplyFailed` and poisons the namespace, which should also make the store read-only.
- The reader's records go through `Namespace::replay`, which applies them without validating them again and requires `seq` order without gaps.
- Values in ops are at most `MAX_VALUE_DEPTH` deep and versions at most `i64::MAX`, so every record the pipeline produces encodes and decodes (upstream #31 is why the depth rule is one level stricter than the file format's).

## Acceptance criteria

- With `always`, an acknowledged commit is readable after reopening the files.
- Truncated and bit-flipped tails are detected and treated as the end of the log.

## Non-goals

- Checkpoints and recovery (step 5).
