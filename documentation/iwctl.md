# iwctl

`iwctl` is the admin CLI for local Ironweaver DB directories (step 7) and for running servers (step 16e). On a directory it works only if no store has it open; a running server is checkpointed, backed up, verified and pruned with `--server` ([Against a running server](#against-a-running-server)), and an embedded store from the program that runs it (`Store::backup`, or `store.backup()` in Python). `iwctl shell` is an interactive client of a server ([below](#query-shell), step 14a). `iwctl user` and `iwctl token` manage users, grants and API tokens on a directory or a server ([Users and tokens](#users-and-tokens), step 15a). Decisions: [ADR 0012](adr/0012-iwctl.md), [ADR 0036](adr/0036-query-shell.md), [ADR 0055](adr/0055-admin-writes-and-iwctl-against-a-server.md).

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
| `backup <dir> <dest> [--no-verify] [--max-bytes-per-second <n>]` | Open the store, back it up into `<dest>` (missing or empty) up to its last commit, close it, and verify the backup ([backup.md](formats/backup.md)). With `--max-bytes-per-second`, the copy is throttled. |
| `restore <dest> [--backup <dir>] [--archive <archive>] [--seq <n> \| --time <time>] [--no-verify]` | Restore into `<dest>` (missing or empty) from a backup (or a data directory no store has open), an archive, or both, to seq `n`, to the last commit at or before `time`, or to the latest seq they reach; then verify the result. `time` is RFC 3339 with a UTC offset: `2026-10-01T12:30:00Z`, `2026-10-01T14:30:00+02:00`. The restored store has a new history: give it a new archive. |
| `verify <dir>` | Check every file and invariant of a data directory, a backup or an archive, and change nothing ([ADR 0011](adr/0011-verify.md)). Problems are damage; notes are what a crash leaves (a torn tail, temporary files, an interrupted cleanup). |
| `archive prune <archive> --before <backup> [--dry-run]` | Remove from a WAL archive what no restore from `<backup>` (the oldest backup you keep) or a later one can need: per namespace the backup holds, the segments wholly before its oldest checkpoint in the backup (never the last one), and imports' checkpoints below it. Namespaces the backup doesn't hold are left alone. `--dry-run` only reports. A store may keep archiving into the archive meanwhile; don't restore from it while it is pruned ([ADR 0055](adr/0055-admin-writes-and-iwctl-against-a-server.md)). |
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
| `--max-bytes-per-second <n>` | `backup`: copy at most this fast. Checkpoints wait for a backup's whole copy, so a slower one holds them back longer. |
| `--before <backup>`, `--dry-run` | `archive prune`: the oldest backup to keep (with `--server`, its name in the backup directory); report only. |
| `--merge` | `import` into an existing namespace through commits. |
| `--format <format>` | The file format of `import` (`json`, `binary`, `lgf`) or `export` (`json`, `binary`). |

## Against a running server

```
iwctl --server <endpoint> <command> [options]     # credentials as for user and token, below
```

Put `--server <endpoint>` (`https://host:port`, or `http://` for a server whose TLS is off) where the directory goes, with credentials: `--token <token>` (or `IWDB_TOKEN`), `--user <name>` (a password prompt), or a client certificate (`--tls-cert`, `--tls-key`; `--tls-ca` for the CA). Each command is one call of the server's `Database` or `Admin` trait (`AdminService` over gRPC), printed like its local form, text or `--json`. The admin writes need a server-wide admin, and every call is audited ([ADR 0055](adr/0055-admin-writes-and-iwctl-against-a-server.md)).

| Command | What it does |
|---|---|
| `status` | The server: version, start, readiness, fsync policy, memory against its limit, disk, request counts, and every namespace you have a role on. |
| `checkpoint [-n <ns>]` | Checkpoint one namespace, or all. No `--archive`: the server's store archives what it removes if it has `[store] archive`. Waits for a running backup. |
| `backup <name> [--max-bytes-per-second <n>] [--no-verify]` | Back up into the server's backup directory (`[backup] dir`) as `<name>` (1 to 128 ASCII letters, digits, `.`, `_` or `-`), then verify it. Nothing may exist under the name. The rate defaults to the server's `[backup] max_bytes_per_second` (0: unthrottled). |
| `verify [store \| archive \| backup <name>]` | Verify the running store (by default: its checkpoints, its WAL up to each namespace's synced seq, and its live state), the server's WAL archive, or a backup by name. Refused while the server refuses writes for memory. |
| `archive prune --before <name> [--dry-run]` | `archive prune` on the server's archive, before its backup `<name>`. |
| `namespaces`, `create-namespace <name> [--key <k>]`, `drop-namespace <name> [--key <k>]` | As locally, without `<dir>` (and without `--archive`). |
| `indexes`, `create-index`, `drop-index`, `add-constraint`, `drop-constraint` | As locally, without `<dir>`; `-n` names the namespace. |
| `requests [<user>]` | The running requests, oldest first (a non-admin sees only its own). |
| `cancel <id>` | Cancel a running read: its caller gets `cancelled`. Commits and admin writes can't be cancelled. A queued or running job (listed as `StartJob`) is cancelled like `jobs cancel`. |
| `jobs list [<user>]` | The managed analytics jobs the server keeps, newest first: id, kind, namespace, owner, state, how long it ran, how far its algorithm has got (`pagerank 37/100`: the core's phase, units done and their total), the projection's size and rows once known (a non-admin sees only its own). |
| `jobs show <id>` | One job's state and progress, its error if it failed or was cancelled, and how long it is kept. |
| `jobs cancel <id>` | Cancel a queued or running job; one that has ended is shown as it is. |
| `jobs result <id> [<offset> [<limit>]]` | A page of a done job's rows (tab-separated id and score or count, or a group's ids per line), at most 10 000 and about 4 MiB; while more are left the last line names the next command. |

Jobs are started through the API (gRPC `StartJob`, REST `POST /v1/namespaces/{ns}/jobs`) or the Rust clients, not by `iwctl` ([ADR 0056](adr/0056-managed-analytics-jobs.md)). They live in the server's memory, so `jobs` needs `--server` (exit 2 without it); anyone may list, show, cancel and fetch their own, a server-wide admin anyone's. `jobs result` of a job that isn't done fails (exit 4) with `invalid_argument`, or the job's error.

`restore`, `import` and `export` are offline: with `--server` they are refused (exit 2). A restore writes a new data directory and doesn't touch a running store: run `iwctl restore` on the server's host, then start a server on the restored directory.

Exit codes with `--server`: 0 done, 1 damage (`verify` found a problem, or `corrupt`), 2 usage, 3 the server is unavailable (busy, draining, or not answering), 4 any other error, printed as `iwctl: <code>: <message>` (with `--json` also `{"error": {"code", "message"}}` on stdout).

```
$ S="--server https://127.0.0.1:7600 --tls-ca docker/tls/ca.pem"
$ iwctl $S backup nightly-2026-10-06 --max-bytes-per-second 52428800
backed up 2 namespaces into /var/backups/iwdb/nightly-2026-10-06, 7340032 bytes
  default at seq 58 (2026-10-06T01:53:04.882200+00:00): checkpoints 29, 58, 8 WAL segments
  social at seq 12: checkpoints 12, 0 WAL segments
verify /var/backups/iwdb/nightly-2026-10-06 (backup): ok
$ iwctl $S archive prune --before nightly-2026-09-29 --dry-run
would remove 41 archived segments (43008 bytes) of /var/lib/iwdb-archive before the backup /var/backups/iwdb/nightly-2026-09-29
$ iwctl $S requests
request 812: Analyze on social by ann from 10.0.0.7, running 41.3 s
$ iwctl $S cancel 812
$ iwctl $S jobs list
job 815: leiden on social by ann, running for 912.4 s, leiden 1/3, 2000000 nodes and 9000000 edges
job 790: page_rank on social by ann, done for 431.0 s, pagerank 37/100, 1000 rows (cut)
$ iwctl $S jobs result 790 0 3
p-1003	0.0021
p-77	0.0019
p-4	0.0017
(more: jobs result 790 3)
```

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

On a **server**, put `--server <endpoint>` where the directory goes (`https://host:port`, step 15b), with credentials: `--token <token>` (or `IWDB_TOKEN`), or `--user <name>` (a password prompt, then a login), or a client certificate (below). The server checks the caller's roles: managing users and grants needs the server-wide admin role; users can change their own password and manage their own tokens.

**TLS** (step 15b, [ADR 0048](adr/0048-tls-and-mtls.md)), for `--server` and `shell`: `--tls-ca <file>` is the CA (PEM) the server's certificate is verified against (default: the system's trust store); `--tls-cert <file>` and `--tls-key <file>` are a client certificate and its key (PEM), which authenticate as the user the certificate names on a server with `[tls] client_ca`, without a token or password. An `http://` endpoint reaches a server whose TLS is off; `--tls-*` with one is a usage error (exit 2).

Passwords are read without echo from a terminal (a new one twice), or one line each from stdin when it isn't a terminal (with `--user`, the login's password first). They never go on the command line, in the output or in an error. Roles: `read` (every read, the change stream, the catalog, the status), `write` (and commits), `admin` (and catalog changes and dropping the namespace); [ADR 0043](adr/0043-users-and-roles-in-the-system-namespace.md).

```
$ iwctl user create /var/lib/iwdb root --admin
password for the new user root:
again:
created user root (admin): no grants
$ S="--server https://127.0.0.1:7600 --tls-ca docker/tls/ca.pem"
$ export IWDB_TOKEN=$(iwctl --json token create $S --user root root cli | jq -r .token)
$ iwctl user create $S ann
$ iwctl user grant $S ann social write
ann: social=write
$ sh docker/dev-cert.sh --client root      # a client certificate for root: no token needed
$ iwctl user list $S --tls-cert docker/tls/client-root.pem --tls-key docker/tls/client-root.key
```

## Query shell

```
iwctl shell <endpoint> [-n <namespace>] [--json] [--token <token> | --user <name>] [--tls-ca <file>] [--tls-cert <file> --tls-key <file>]
```

Against a server with authentication on, log in with `--token` (or `IWDB_TOKEN`), with `--user` (a password prompt), or with `\login <user>` in the session; when stdin isn't a terminal, the password is the next line of input. `\logout` ends the session, `\whoami` shows the user and its roles.

Connects to the `iwdb-server` at `<endpoint>` (`https://host:port`; `http://` for a server whose TLS is off; `--tls-*` as above) and reads one command per line from stdin, so it works interactively and piped from a script. The prompt (`social> `) goes to stderr, and only when stdin is a terminal. For line editing and history, run it under `rlwrap`. Each command is one call of the `Database` trait over gRPC, bounded like any other read.

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
$ iwctl shell https://127.0.0.1:7600 --tls-ca docker/tls/ca.pem -n social
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
| 3 | Locked: a store has the directory open (for `status`, after printing what the files say); with `--server`, the server is unavailable |
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
