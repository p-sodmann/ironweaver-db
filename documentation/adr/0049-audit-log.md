# ADR 0049: The audit log

Status: accepted
Date: 2026-10-05

## Context

Step 15c asks for a record of who did what, when, and with what outcome, for every admin and catalog operation, without ever recording a secret or a value of the data. Design rule 8 puts every decision behind one service trait: the adapters (gRPC, REST, and through them Python, `iwctl --server` and the console) only translate. Points to decide: what is audited, where entries are made, what they contain, where they go and for how long, and whether they are durable with the change they record.

## Decision

**What is audited.** Every call of a login (success or failure), a logout, a user, grant, token, namespace or catalog change; and every refusal (`unauthenticated`, `permission_denied`) of any operation, reads included. A refused read is an access attempt, not a read. Successful reads and data commits aren't audited (step 15c's non-goal; the change stream records commits). The operation table of ADR 0045 gets a column for this (`Operation::audited`: `Always` or `Refusals`), and `Login` joins the table as an operation that needs nothing (`Requirement::Open`). The role test keeps its own list of the operations that may go unaudited, so a new operation is audited until someone lists it there.

**Where entries are made: at the authorisation point, once.**

- `iwdb_query::Authorized<D>` records a refusal when its check fails, and records the outcome of an `Always` operation once the inner call returns. `Logout` now runs through it too (before, the adapters called the store's `logout` directly).
- `iwdb_query::auth::login` wraps `Authenticate::login` and records the login. It is the only way the adapters log in.
- The server's gate (`iwdb_server::auth::credentials` and `authenticate`) records the requests it refuses before an operation runs: no credentials, a token that is unknown or expired, a certificate that names no user, or a missing CSRF header. It names the operation from the gRPC method or the REST route.

No adapter makes an entry. Where entries go is an `AuditSink` (in `iwdb_query::audit`, so the pure-Rust crates stay free of `tracing`, ADR 0042). The server's `LogAudit` is the default of `Server::new`, and tests swap in their own with `Server::audit`.

**What an entry holds.** `operation` (the RPC's name), `outcome` (`success` or `failure`), `code` (the error code, never the message: a catalog error can quote data), `user`, `auth` (`session`, `api_token`, `certificate`, `off`; `Principal` now records how it authenticated), `client` (the IP address), and where they apply `namespace`, `subject` (the user an account change is about), `token_name`, `role`, `admin`, `seq` (a catalog change's commit) and `namespace_event` (a namespace's creation or drop).

Some things are never recorded: a password, a token, a token's hash, a certificate (not even its name when it names no user), an error message, or a value of the data.

A login's `user` is the name tried only if it is a valid user name: a password typed into the name field is then usually left out, and no control character reaches a log line.

**Where entries go: the log, and files if asked.**

- Every entry is an `info` event of target `iwdb::audit` in the normal log (JSON or text, ADR 0042). The log filter lets that target through whatever `[log] level` says, unless the level names `iwdb::audit` itself. A `warn` level doesn't silently drop the audit trail.
- `[audit] dir` also writes the entries as JSON lines to a file per UTC day (`audit-YYYY-MM-DD.jsonl`, mode 0600, directory 0700), through a second `tracing-subscriber` layer filtered on the target. No new crate is needed.
- **Retention.** `[audit] retention_days` (default 30; 0 keeps everything) deletes older files at start and at each new day, so the files don't grow without bound. Daily files need no logrotate and no SIGHUP. Entries in stderr are kept as long as the log collector keeps them (Docker's log driver, journald), and config.md says how to bound that.

There is no switch to turn auditing off. The log level can still hide it, by naming the target.

**Not durable with the change.** An entry is written after the outcome is known, without fsync, so a crash can lose the last entries. User, grant, token, namespace and catalog changes are durable in the WAL on their own, and the entry of a catalog change or namespace event names its seq.

User, grant and token changes carry no seq: `Accounts` doesn't return one, and changing its ten methods and every client for it wasn't worth it while the system namespace's WAL is their durable record. A call whose future is dropped before it finishes (the client went away) leaves no entry, even if the change was made.

**Not audited at all.** Failed TLS handshakes stay at `debug`: there is no request yet, and auditing them would let any port scanner fill the trail. In-process access (the embedded store, `iwctl` on a data directory, Python's `Store.open`) isn't audited either: the file permissions are the boundary (ADR 0045), and nothing can be refused there.

**No telemetry.** The project collects no data. The server opens no connection apart from the projection sources it is configured with; a binary test lists its sockets through a session of every audited kind. The console serves its own fonts instead of loading them from Google Fonts. `SECURITY.md` states this, and says how to report a vulnerability: GitHub's private vulnerability reporting, turned on for the repository.

## Consequences

- One audit entry per audited operation is tested for every role × operation cell over gRPC and REST (`crates/iwdb-server/tests/roles.rs`), with certificate principals in `tls.rs`. The binary test of 15a greps the audit lines and the audit file for passwords, tokens, keys and a value of the data.
- `Authorized::new` takes the request's `Audit` (sink and client address), and `Principal` has a `via` field. Both are crate-internal for the adapters, but they are public API of `iwdb-query`.
- A deployment that wants a SIEM ships the `iwdb::audit` lines or the audit files; the format is the interface (step 15c's non-goal).
- Step 16e's checkpoint, backup, restore and cancel operations add their rows to the operation table as `Always` when they arrive.
