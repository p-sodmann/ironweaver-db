# ADR 0011: verify

Status: accepted
Date: 2026-10-01

## Context

Step 7 adds `verify`, like SQLite's `PRAGMA integrity_check`: a check of every file and of the invariants the commit pipeline keeps, that an operator runs on a data directory, a backup or an archive, and that the crash harness runs after every crash. Recovery already reads the files, but only as far as it needs them: the newest checkpoint that loads and the WAL after it. It skips damaged checkpoints, never reads older WAL, and trusts a checkpoint's contents.

Questions: what exactly to check; what to tolerate; whether verify may write (cut a torn tail, say); whether it may run on a directory a store has open; and what verify needs from the core.

## Decision

**What it checks** (`iwdb_storage::verify`, `iwdb::verify`):

- The marker: magic, layout version, checksum. A damaged marker is a problem; a missing or foreign one is an error (`NotADataDir`): there is nothing to verify.
- Every WAL segment from the first one, not only from the newest checkpoint: header and frame checksums, seqs, contents (every record decodes), and the chain across segments.
- Every checkpoint: it loads (checksum, format, a seq equal to its name, the namespace), the indexes saved in it match its catalog (`IndexChanges` is empty), and its state keeps the invariants. Its 16-byte header has zero `flags` and `reserved` fields (a workaround for upstream [#33](https://github.com/p-sodmann/Ironweaver/issues/33): the core checks neither, and its CRC doesn't cover the header).
- Coverage: the WAL reaches from each checkpoint's seq + 1 to the newest checkpoint, without a gap, so recovery can fall back to any of them; and it doesn't end before the newest one.
- Replay: the WAL replayed onto the oldest checkpoint that loads equals each newer checkpoint at its seq, and its end state keeps the invariants. This finds a checkpoint that is internally valid but wrong, which recovery would load without complaint.
- The invariants (`iwdb_engine::invariants::check`): edge endpoints are live nodes; versions are 1 to `i64::MAX`; no reserved keys; values within the depth limit; the graph's indexes are exactly the catalog's, flushed, and equal to a scan; every constraint holds. The index check enumerates an index completely with range lookups (a range below and one above a value of each key kind), so a stale entry under a key that no node holds is found too. The core exposes all of this; no upstream change is needed.
- A backup's manifest: checksum, history equal to the marker's, every listed file with its length and CRC32C, no unlisted file, the WAL ending at its seq ([backup.md](../formats/backup.md)).
- An archive: its marker, every segment complete and checked, the chain from the first segment to the last ([archive.md](../formats/archive.md)). An archive is not replayed: it has no checkpoint to compare with.

**What it tolerates**, as notes rather than problems: a torn tail in the last segment (reported with its offset and the frames after it, and not cut), temporary files, files that aren't the database's, and the extra segments an interrupted checkpoint cleanup leaves ([data-dir.md](../formats/data-dir.md), "Interrupted cleanup"). These are what a crash leaves and recovery handles. The result: `problems` (damage: the directory isn't what the database writes, or a store couldn't recover it) and `notes`.

**Verify never writes.** It opens files for reading only, and never creates `LOCK`.

**It takes a shared lock.** Verify takes `flock(LOCK_SH)` on `LOCK` if the file exists. So it fails with `Locked` while a store has the directory open, and a store can't open the directory while verify runs; several verifies can run at once. Reading a live directory without the lock would race with the checkpointer (a file removed mid-read reads as a missing file, a segment being appended to as a torn tail), and the report would be wrong in ways an operator can't tell from real damage. A live store can be verified by backing it up and verifying the backup. An archive has no such lock for readers: its files appear by rename, complete.

**Errors vs. problems.** `verify` returns an error only when it can't verify at all: `Locked`, `NotADataDir`, `UnsupportedLayout`, an unlistable directory. Everything it finds in the files is a problem in the report, and it goes on checking the rest.

**Cost.** Every file is read once; every checkpoint is loaded once (while the replay passes its seq). Memory: the replayed namespace and one checkpoint. At most 100 invariant violations are listed per state.

**In the harness.** `check_recovery` runs verify before every recovery it checks: on the directory as the crash left it (torn tails, temporary files and interrupted cleanups included). It must find no problem, and its replayed seq must equal the seq recovery reaches, except in the one case the guarantees allow recovery to refuse (`off` after an OS crash), where verify must find a problem.

## Consequences

- Damage anywhere is found, not only where recovery happens to look; tests flip bytes in every kind of file and plant states that violate each invariant.
- Verify costs a full read and a replay from the oldest checkpoint: more than an open. It is an operator's and a test's tool, not part of opening.
- Verify on a running store isn't possible (`Locked`); verify its backup instead.
