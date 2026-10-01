# ADR 0009: Online backup, WAL archiving, restore and histories

Status: accepted
Date: 2026-10-01

## Context

Step 7 adds the operations that let a store be copied and rewound: an online backup (like `pg_basebackup`), continuous WAL archiving, and point-in-time restore (to a seq, or to a time: [ADR 0010](0010-commit-times.md)). The pieces they build on:

- A data directory ([data-dir.md](../formats/data-dir.md)) holds checkpoints and WAL segments. The checkpointer **removes** old checkpoints and segments while the store runs, under its own mutex. Nothing else removes them while the store is open.
- Every write goes through `LogFs`, so failpoints (ADR 0007) cover it. Initialization writes the marker last, so a directory without one is never mistaken for a store.
- `Wal::synced_seq` lags behind the last commit under `group` (and is 0 under `off` until an explicit sync). Records after it may be lost in an OS crash, and the store would then continue with *other* commits under the same seqs.
- Records are numbered by seq only. Two stores that started from the same state and then committed independently have different records with the same seqs. Nothing on disk tells them apart.

## Decision

### A history id per data directory (layout 2)

The data directory's marker gets a random 128-bit **history id** (layout 2). A new directory gets a new id, and so does every restore: a restore to seq `N` starts a new history whose commits `N + 1, ...` are not the original's. A backup keeps the id of the store it copies. A WAL archive ([archive.md](../formats/archive.md)) belongs to one history: its own marker holds the id.

- A store archives only into an archive of its own history (or an empty directory, which it initializes); otherwise open fails with `ArchiveMismatch`. So a restored store can't write its new commits into the old store's archive, where they would collide with, or silently continue, the old history.
- A restore combines a backup and an archive only if their history ids are equal (`HistoryMismatch`).
- **A store doesn't open a backup** (`IsBackup`). A backup is restored instead, which gives it a new history. Otherwise the backup, opened as a store, would continue the original's history under the same id, and the id would no longer separate the two. A restore to the backup's own seq is the "open a copy" case.
- Layout 1 directories (step 5) have no id. Opening one as a store writes a layout 2 marker with a new id once recovery has read it (the only content change; [data-dir.md](../formats/data-dir.md)). Readers (verify, restore) accept layout 1 without changing it; a layout 1 data directory can be restored on its own, but not combined with an archive, since its history is unknown.

Alternatives: a history id in every WAL segment header (PostgreSQL's timelines) would let one archive hold several histories, and let the reader check that consecutive segments belong together. It is a WAL format change that buys little while every archive belongs to one store, and the archive's marker plus the open-time check give the same protection for the cases we support. Copying a data directory by hand and opening both copies is not supported, like copying a PostgreSQL data directory without `pg_basebackup`.

### Backup: a consistent copy of the data directory, cut at a synced seq

A backup is a directory in data-dir layout 2: the source's marker (same history id), `checkpoints/`, `wal/`, and a `BACKUP` manifest ([backup.md](../formats/backup.md)), so verify and restore read it with the same code as a data directory.

- **What it reaches.** `Store::backup` first fsyncs the WAL (like `Store::checkpoint`), then takes the WAL's synced seq `B`, which is then the last applied seq. Only synced records are copied, so `B` is a seq the source can never lose, even in an OS crash under `group` or `off`: the backup's history is always a prefix of the source's. If the store is read-only (its WAL failed), nothing more can be synced, and `B` is the synced seq as it is.
- **What it holds.** Every checkpoint at or below `B` that the store doesn't know to be damaged, and every WAL segment from the one holding the oldest copied checkpoint's seq + 1 to the one holding `B`. The last of these is copied only up to the end of record `B` (`segment_prefix`: every frame up to `B` is checked, and the bytes after it, which the writer may be appending, are never decoded). So the backup's WAL ends exactly at `B`, without a torn tail, and restores to any seq from its oldest checkpoint to `B`.
- **Consistency with the checkpointer: hold its lock.** The backup takes the checkpointer's mutex, then the live namespace's for the fsync and `B` only, and holds the checkpointer's mutex while it lists and copies. No checkpoint or segment can be removed meanwhile, and no file vanishes mid-copy. The lock order (checkpointer, then live) is the one every other path uses or is compatible with: nothing takes the checkpointer's mutex while holding the live one. **Commits wait only for the fsync**, as in `Store::checkpoint`. **Checkpoints wait for the whole copy** (background, explicit and on close), and the WAL grows meanwhile. The alternative, copying without the lock and starting over from a newer checkpoint when a file vanishes, has no bound on retries under a busy checkpointer and is harder to test; the copy is O(data directory), which an operator schedules anyway.
- **Writing.** Every write goes through `LogFs`, in initialization's order: create the directory (which must be missing or empty), `checkpoints/` and `wal/`, sync; each file written (`create`, `write_all` in 1 MiB chunks, `sync`); sync both directories; the manifest with `write_atomic`; the marker last with `write_atomic`; sync. An interrupted backup has no marker and holds files, so a store, verify and restore all refuse it (`NotADataDir`). The manifest records `B`, the history, the commit time of `B`, and every file with its length and CRC32C, so `verify` notices a missing, truncated or extra file even where the files' own checksums can't (a segment cut at a frame boundary).
- An offline backup (`iwctl backup`) opens the store (recovery), backs it up, and closes it without a checkpoint.

### Continuous WAL archiving: archive before removal

`StoreOptions::archive` names an archive directory. The checkpointer, before it removes WAL segments, copies them into the archive:

1. for each segment to remove: if the archive has a file of that name, read it: if it differs from the segment, fail (`ArchiveConflict`: another history, or damage; nothing is removed). Otherwise, or if it is identical, write the segment to `<name>.tmp` in the archive (`create`, `write_all`, `sync`) and rename it over `<name>`;
2. sync the archive directory;
3. only then remove the segments from `wal/`, and sync `wal/`.

So a segment is durable in the archive (its file and its directory entry fsynced) before it leaves `wal/`. A crash between 2 and 3 leaves the segment in both places, and the next checkpoint archives it again: **idempotent**. An identical file is rewritten rather than trusted, because after a crash we can't know whether its earlier fsync ever completed; a fresh copy and a fresh directory sync make it durable without retrying a failed fsync (fsyncgate, ADR 0005).

**When archiving fails**: nothing is removed from `wal/`, so no record is lost; the checkpoint itself (written and synced before) stays valid.

- A failed copy (a write, a file fsync, a rename, a full disk, a conflict) fails the checkpoint run with the error; `Store::checkpoint_failure` reports it, and the next checkpoint that writes a file retries. Commits go on, and the WAL grows until archiving works again.
- A failed **directory** fsync of the archive disables the checkpointer until the store is reopened (`CheckpointsDisabled`), like the checkpointer's own failed directory syncs: a retried directory fsync could report success without making the renames durable, and removing segments on that basis could lose them.
- The archive directory has an exclusive `LOCK` while a store archives into it, so two stores (say, hand-made copies of one directory) never write one archive. Readers (restore, verify) don't take it: archived files appear by rename, complete, so a restore can read an archive that a running store is adding to.

Archiving starts with the segments a checkpoint removes after the option is set. An archive is therefore contiguous from the first segment the store removed while archiving; one set up on a new store holds the whole history from seq 1.

### Restore: a new directory with one checkpoint at the target seq

`iwdb::restore` writes a new data directory at seq `N` from a backup, an archive, or both:

- **Target.** A seq `N`; a time `T` (the last record in seq order whose commit time is at or before `T`, ADR 0010); or the latest seq the sources reach.
- **Sources.** The backup's checkpoints and segments, and the archive's segments, read as one log (`WalReader::from_segments`). Where both have a segment of the same name, they must agree: one is a prefix of the other (the backup's copy of the segment it cut at `B`), and the longer is used; otherwise `ArchiveConflict`. The backup (or data directory) is read under a shared lock: `Locked` if a store has it open.
- **Replay in memory, write a checkpoint.** Restore loads the newest checkpoint at or below `N` that loads (falling back to older ones, or to an empty namespace at seq 0 if there is none and the WAL reaches back to seq 1), replays the WAL up to `N` (a bounded reader: nothing after `N` is decoded), and writes **one checkpoint at `N`** into the new directory, with an empty `wal/`. That is "the end at `N`": the next open recovers from the checkpoint, finds no WAL after it, and the next commit is `N + 1`. No WAL is copied or cut, so no WAL file of the restored store can hold records of the old history after `N`. The cost is one load, the replay and one save, all of which the first open would otherwise do.
- **Writing, and interrupted restores.** The target must be missing or empty. Restore creates it, takes its lock, writes a `RESTORING` file (synced), creates `checkpoints/` and `wal/` (synced), writes the checkpoint with `write_atomic` and syncs `checkpoints/`, removes `RESTORING` and syncs, then writes the marker (layout 2, a **new** history id) with `write_atomic` and syncs. Every state an interruption can leave is refused or right:
  - `RESTORING` present: refused (`InterruptedRestore`);
  - a checkpoint, no `RESTORING`, no marker: refused (`NotADataDir`, a non-empty `checkpoints/`);
  - restore to seq 0 (no checkpoint), after `RESTORING` was removed and before the marker: an empty directory that open initializes, which is the restored state;
  - the marker: complete.
  A restore is never finished by the next open; the operator removes the directory and restores again.
- **Restore never writes to its sources.**

### Not done

- Incremental backups (only the WAL since the last backup): an archive gives the same effect.
- Restoring in place over a damaged store: restore into a new directory and swap.
- Pruning an archive: an operator removes segments older than the oldest backup they keep; a tool for it can come with step 16.

## Consequences

- Backups are verifiable (`verify` reads them like a data directory, plus the manifest) and restorable to any seq between their oldest checkpoint and `B`; with an archive, to any seq the archive and the backup reach together.
- The guarantees ([guarantees.md](../guarantees.md), "Backup, archiving and restore") are tested with failpoints on every new write and by the kill -9 harness, which kills children during backups, archiving and restores and checks that whatever is left is refused or equal to the reference.
- A busy store's checkpoints wait for a backup to finish. A backup of a large store should run when a checkpoint delay is acceptable; step 16 can add throttling.
- The layout version moved to 2 for the history id. Opening a step 5 directory with this version upgrades it; older versions can't open it afterwards.
