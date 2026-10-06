# ADR 0012: iwctl for local directories

Status: accepted
Date: 2026-10-01

## Context

Step 7 adds `iwctl` with `status`, `checkpoint`, `backup`, `restore` and `verify` for local data directories. Design rule 8: adapters only translate. The `Database` trait comes in step 10, the server in step 11, and the query shell in step 14, so `iwctl` can't talk to a running store yet. Questions: where the operations live, how to parse arguments, what to print, how to report outcomes to scripts, and what to do about a store that is open or archives its WAL.

## Decision

- **Every operation is in the library.** `iwdb::status`, `Store::checkpoint`, `Store::backup`, `iwdb::restore`, `iwdb::verify`, and underneath `iwdb_storage::inspect`, `verify`, `backup`, `restore`. `iwctl` parses arguments, calls one of them, and prints the result (`src/output.rs` only formats). The Python bindings and the tests call the same functions.
- **A hand-written parser, no new dependency.** Five commands and a handful of flags fit in about a hundred lines with unit tests; the crash harness parses its arguments the same way. `clap` (4.6 needs exactly our MSRV, 1.85) would add a parser, a builder and terminal styling crates to the tree for `--help` niceties we don't need yet. Step 14's shell, which needs line editing and completion anyway, can revisit this.
- **Output**: text by default; `--json` prints one JSON object per result (with `serde_json`, already in the tree). Errors go to stderr, and with `--json` also as `{"error": ...}` on stdout.
- **Exit codes** per outcome: 0 ok, 1 damage found, 2 usage error, 3 locked, 4 any other failure ([iwctl.md](../iwctl.md)). Damage (1) is `verify` finding a problem, or a typed corruption error (WAL corruption, a checkpoint that doesn't load, a damaged marker or manifest, an archive conflict). A restore target beyond what the sources hold is a failure (4), not damage.
- **Opening for status.** `status` opens a data directory that no store has open, which runs recovery, and reports the store's seq and synced seq: that is the only way to know what the directory recovers to and whether recovery has anything to repair. It never creates a directory, writes no checkpoint, and doesn't sync on close. Under `--fsync off` the synced seq is shown as `none` (`StoreStatus::synced_seq` is `None`): a store under `off` knows of no fsync, and must not present 0, or its seq, as durable. A directory that a store has open is only read (`inspect`): exit code 3, with what the files say, marked as possibly changing.
- **Locked elsewhere**: `checkpoint`, `backup` and `verify` fail with exit code 3 while a store has the directory open. Online backups of a running store are taken by the program that runs it.
- **Archiving and `checkpoint`**: whether a store archives its WAL is an option of the program that opens it, not something stored in the data directory. A checkpoint by `iwctl` without the archive would remove segments that never reach the archive, leaving a gap. So `checkpoint` requires `--archive <dir>` or `--no-archive`. `status` and `backup` remove nothing and need neither.
- **`backup` and `restore` verify their result** unless `--no-verify`.

## Consequences

- `iwctl` is a thin binary; what it does is tested where it is implemented, and its own tests (`crates/iwctl/tests/cli.rs`) run the binary for each command and exit code.
- An operator can't back up or verify a running store with `iwctl` until step 11 (a server) or a later `iwctl` mode that asks the store's process. (Step 16e added that mode, `iwctl --server`: [ADR 0055](0055-admin-writes-and-iwctl-against-a-server.md).)
- If the store's archive setting should be enforced rather than repeated, a later layout version can record it in the data directory. (Step 16e decided against it for now: ADR 0055.)
