# iwctl

`iwctl` is the admin CLI for local Ironweaver DB directories (step 7). It works on directories that no store has open; a running store is backed up from the program that runs it (`Store::backup`, or `store.backup()` in Python). The query shell (`iwctl shell`) comes with the server in step 14. Decisions: [ADR 0012](adr/0012-iwctl.md).

```
iwctl [--json] <command> [options]
```

## Commands

| Command | What it does |
|---|---|
| `status <dir>` | What a data directory, a backup or a WAL archive holds. A data directory that no store has open is **opened** (recovery runs: it may cut a torn tail, remove temporary files, and upgrade a layout 1 directory) and closed again without a checkpoint; the output says what recovery did, the seq, and the synced seq. One that a store has open is only read: `in use`, exit code 3. A backup shows its manifest; an archive its segments. |
| `checkpoint <dir> (--archive <archive> \| --no-archive) [--keep <n>]` | Open the store, write a checkpoint of every commit of every namespace (`-n` to name some), keep `n` checkpoints (default 2), remove the WAL segments the oldest kept one covers, and close. If the store archives its WAL, name the archive: the segments the checkpoint removes go there first. `--no-archive` says it doesn't; one of the two is required, so that a checkpoint never removes segments an archive should have received. With nothing new to write, it removes nothing ([data-dir.md](formats/data-dir.md), "Interrupted cleanup"). |
| `namespaces <dir>` | List the namespaces of a store with their seqs, counts, memory and indexes (opens the store, like `status`). |
| `create-namespace <dir> <name> [--key <key>]`, `drop-namespace <dir> <name> (--archive <archive> \| --no-archive) [--key <key>]` | Create or drop a namespace ([ADR 0017](adr/0017-namespaces.md)). A drop needs `--archive` (the store's archive: the namespace's last segments go there first) or `--no-archive`, as `checkpoint` does. With `--key`, a retry returns the original result (`deduplicated`). |
| `indexes <dir> [-n <namespace>]` | The namespace's indexes with their state (`ready`, or `building`), paths, whether declared or needed by a unique constraint, and entries. |
| `create-index <dir> <path> [-n <ns>] [--key <key>]`, `drop-index <dir> <path> ...` | Create (online build) or drop a property index; `path` is attribute names joined by `.`. |
| `add-constraint <dir> unique\|required <label> <path> [-n <ns>] [--key <key>]`, `drop-constraint <dir> ...` | Add a constraint (the existing data is validated first: exit code 4 with the violating node if it fails) or drop one. |
| `import <dir> <name> <file> [--format json\|binary\|lgf]` | Create the namespace `<name>` from a graph file: a core JSON or binary file (as the Ironweaver library writes them) or an LGF file, detected from its first bytes unless `--format` names it ([api/import-export.md](api/import-export.md)). One checkpoint, all or nothing; prints the counts, the indexes and what the file held that a namespace has no place for. Take a backup afterwards if you rely on backups: the WAL archive doesn't hold an import. Progress goes to stderr on a terminal. |
| `export <dir> <file> [-n <ns>] [--format json\|binary]` | Write a namespace's graph to `<file>` (atomically) as a core file: JSON for a `.json` file, binary otherwise, unless `--format` says. |
| `backup <dir> <dest> [--no-verify]` | Open the store, back it up into `<dest>` (missing or empty) up to its last commit, close it, and verify the backup ([backup.md](formats/backup.md)). |
| `restore <dest> [--backup <dir>] [--archive <archive>] [--seq <n> \| --time <time>] [--no-verify]` | Restore into `<dest>` (missing or empty) from a backup (or a data directory no store has open), an archive, or both, to seq `n`, to the last commit at or before `time`, or to the latest seq they reach; then verify the result. `time` is RFC 3339 with a UTC offset: `2026-10-01T12:30:00Z`, `2026-10-01T14:30:00+02:00`. The restored store has a new history: give it a new archive. |
| `verify <dir>` | Check every file and invariant of a data directory, a backup or an archive, and change nothing ([ADR 0011](adr/0011-verify.md)). Problems are damage; notes are what a crash leaves (a torn tail, temporary files, an interrupted cleanup). |
| `help`, `--help`, `--version` | |

Options:

| Option | Meaning |
|---|---|
| `--json` | One JSON object per result on stdout (`backup` and `restore` print two: the operation's, then the verification's). Errors print `{"error": "..."}` on stdout too, and the message on stderr. |
| `--fsync always\|group\|off` | The fsync policy to open a store with (`status`, `checkpoint`, `backup`; default `always`). Under `off` the synced seq shows as `none`: the store knows of no fsync, so it claims no durable seq. |
| `--keep <n>` | Checkpoints to keep (`checkpoint`). |
| `--archive <archive>` | The store's WAL archive (`status`, `checkpoint`, `backup`), or the archive to restore from (`restore`). |
| `-n`, `--namespace <name>` | The namespace a command acts on (default `default`); for `checkpoint` and `restore`, repeatable: only those namespaces (a `--seq` restore needs exactly one). |
| `--key <key>` | An idempotency key for the operation (`create-namespace`, `drop-namespace`, index and constraint commands). |
| `--no-verify` | Don't verify after `backup` or `restore`. |
| `--format <format>` | The file format of `import` (`json`, `binary`, `lgf`) or `export` (`json`, `binary`). |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Done; for `verify`, no damage (notes may be printed) |
| 1 | Damage found: `verify` found a problem, or an operation failed on damaged data (corruption in the WAL, a checkpoint that doesn't load, a damaged marker or manifest, an archive conflict) |
| 2 | Usage error: unknown command or option, a missing or extra argument |
| 3 | Locked: a store has the directory open (for `status`, after printing what the files say) |
| 4 | Any other failure: not a data directory, a destination that isn't empty, a restore target the sources don't reach, an I/O error, ... |

## Examples

```
$ iwctl status data
data directory  data
  layout 4, history 0e456595a462b8189be548cc0810328b
  store: seq 58, synced seq 58, fsync always
  recovery: replayed 29 records onto checkpoint 29
  checkpoints: 29
  wal: 9 segments from seq 29, last record 58

$ iwctl backup data backups/2026-10-01
backed up to seq 58 (2026-10-01T01:53:04.882200+00:00) into backups/2026-10-01: checkpoints 29, 58, 8 WAL segments, 8403 bytes
verify backups/2026-10-01 (backup): ok
  2 checkpoints (2 loaded and checked), 8 WAL segments, 30 records (seq 29 to 58)
  recovers to seq 58

$ iwctl restore restored --backup backups/2026-10-01 --archive archive --time 2026-10-01T12:00:00Z
```
