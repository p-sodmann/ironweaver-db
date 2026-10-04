# iwctl

`iwctl` is the admin CLI for local Ironweaver DB directories (step 7). It works on directories that no store has open; a running store is backed up from the program that runs it (`Store::backup`, or `store.backup()` in Python). `iwctl shell` is an interactive client of a server ([below](#query-shell), step 14a). `iwctl user` and `iwctl token` manage users, grants and API tokens on a directory or a server ([Users and tokens](#users-and-tokens), step 15a). Decisions: [ADR 0012](adr/0012-iwctl.md), [ADR 0036](adr/0036-query-shell.md).

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
| `import <dir> <name> <file> [--format json\|binary\|lgf] [--merge]` | With `--merge`, upsert the file's nodes and edges into the existing namespace `<name>` (`default` too) through commits, in batches. Without it: create the namespace `<name>` from a graph file: a core JSON or binary file (as the Ironweaver library writes them) or an LGF file, detected from its first bytes unless `--format` names it ([api/import-export.md](api/import-export.md)). One checkpoint, all or nothing; prints the counts, the indexes and what the file held that a namespace has no place for. With `--archive <archive>` (the store's archive) the import's checkpoint is archived at once; otherwise the store's next open with its archive archives it. Progress goes to stderr on a terminal. |
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
| `--merge` | `import` into an existing namespace through commits. |
| `--format <format>` | The file format of `import` (`json`, `binary`, `lgf`) or `export` (`json`, `binary`). |

## Users and tokens

```
iwctl user create <dir> <name> [--admin]          # prompts for the password
iwctl user passwd <dir> <name>                    # ends the user's sessions
iwctl user delete <dir> <name>
iwctl user admin <dir> <name> on|off              # server-wide admin or not
iwctl user grant <dir> <name> <namespace> read|write|admin
iwctl user revoke <dir> <name> <namespace>
iwctl user list <dir>
iwctl token create <dir> <user> <name> [--expires <seconds>]   # prints the token once
iwctl token revoke <dir> <user> <name>
iwctl token list <dir> <user>
```

On a **data directory** (the server stopped: a store holds the directory's lock) they need no credentials: whoever can open the directory owns the store, as for every other command here. That is how the first admin of a store is made without a default password (or with `IWDB_AUTH_BOOTSTRAP_PASSWORD` on the server's first start, [config.md](api/config.md#authentication-and-the-first-admin)).

On a **server**, put `--server <endpoint>` where the directory goes, with credentials: `--token <token>` (or `IWDB_TOKEN`), or `--user <name>` (a password prompt, then a login). The server checks the caller's roles: managing users and grants needs the server-wide admin role; users can change their own password and manage their own tokens.

Passwords are read without echo from a terminal (a new one twice), or one line each from stdin when it isn't a terminal (with `--user`, the login's password first). They never go on the command line, in the output or in an error. Roles: `read` (every read, the change stream, the catalog, the status), `write` (and commits), `admin` (and catalog changes and dropping the namespace); [ADR 0043](adr/0043-users-and-roles-in-the-system-namespace.md).

```
$ iwctl user create /var/lib/iwdb root --admin
password for the new user root:
again:
created user root (admin): no grants
$ export IWDB_TOKEN=$(iwctl --json token create --server http://127.0.0.1:7600 --user root root cli | jq -r .token)
$ iwctl user create --server http://127.0.0.1:7600 ann
$ iwctl user grant --server http://127.0.0.1:7600 ann social write
ann: social=write
```

## Query shell

```
iwctl shell <endpoint> [-n <namespace>] [--json] [--token <token> | --user <name>]
```

Against a server with authentication on, log in with `--token` (or `IWDB_TOKEN`), with `--user` (a password prompt), or with `\login <user>` in the session; when stdin isn't a terminal, the password is the next line of input. `\logout` ends the session, `\whoami` shows the user and its roles.

Connects to the `iwdb-server` at `<endpoint>` (`http://host:port`) and reads one command per line from stdin, so it works interactively and piped from a script. The prompt (`social> `) goes to stderr, and only when stdin is a terminal. For line editing and history, run it under `rlwrap`. Each command is one call of the `Database` trait over gRPC, bounded like any other read.

| Command | What it does |
|---|---|
| `match <pattern>` | every match of a pattern, in the core's text: `(a:Person {age: 30})-[k:KNOWS*1..2]->(b)`. One column per node variable, then one per edge variable (edge ids; a path for a variable-length edge); anonymous ones are `_0`, `_e0`, … |
| `find <filter>` | nodes matching a filter, in the core's JSON form (as over REST): `{"Label": "Person"}`, `{"Compare": {"path": ["age"], "op": "Ge", "value": {"Int": 18}}}` |
| `explain <filter>` | how `find` would read the filter: the plan, estimated and exact candidates |
| `node <id>...`, `edge <id>...` | lookups; missing ids are listed as not found |
| `upsert-node <id> [:Label]... [{json}]` | create a node or replace its attributes (plain JSON: integers are `Int`, other numbers `Float`, objects dicts); one commit |
| `add-edge <from> <to> [:type] [{json}]`, `delete-node <id>`, `delete-edge <id>` | one commit each |
| `namespaces`, `use <name>`, `create-namespace <name>`, `drop-namespace <name>` | namespaces; `use` switches (and checks that it exists) |
| `status`, `indexes` | the namespace's state; its indexes and constraints |
| `create-index`, `drop-index`, `add-constraint`, `drop-constraint` | as the commands above, with the same arguments minus the directory |
| `\next` | the next page of the last `find` or `match` (shown as `(more: \next)`) |
| `\limit <n>\|off`, `\partial on\|off`, `\timeout <s>\|off` | read options of the following commands |
| `\json`, `\table` | output: one JSON object per answer (with `seq`, `cursor`, `truncated`, `work`), or aligned tables |
| `\login <user>`, `\logout`, `\whoami` | log in (password prompted for, or the next line when piped), end the session, show who the server takes you for |
| `\help`, `\quit` | also `quit`, `exit`, or the end of input. `--` starts a comment |

An error prints `error (<code>): <message>` ([codes](api/errors.md)) to stderr (in JSON mode `{"error": {"code", "message"}}` to stdout) and the shell goes on. The exit code is 0 if every command succeeded, 4 if one failed, 2 for an invalid endpoint.

```
$ iwctl shell http://127.0.0.1:7600 -n social
social> match (a:Person)-[k:KNOWS]->(b)
a     | b     | k
------+-------+--
alice | bob   | 0
bob   | carol | 1
(2 rows)
```

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
