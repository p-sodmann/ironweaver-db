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

## Acceptance criteria

- With `always`, an acknowledged commit is readable after reopening the files.
- Truncated and bit-flipped tails are detected and treated as the end of the log.

## Non-goals

- Checkpoints and recovery (step 5).
