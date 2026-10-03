# Error codes

Every error of the `Database` trait (`iwdb_query::Error`) has a **code** and a message. The codes below are a contract shared by every access method: the embedded store, Python, gRPC (step 11) and REST (step 12) report the same code for the same failure, and clients branch on the code, never on the message. Adding a code is a minor change; renaming or removing one breaks clients.

The engine's and the storage layer's errors are mapped to codes in one place (`crates/iwdb-query/src/error.rs`). The gRPC and HTTP status columns are the mapping steps 11 and 12 will implement.

| Code | Meaning | Retry? | Python exception | gRPC (step 11) | HTTP (step 12) |
|---|---|---|---|---|---|
| `invalid_argument` | The request is invalid: a bad argument or name, a limit of 0, a filter or pattern the core rejects, an invalid transaction (reserved key, empty, value too deep, ambiguous edge, record too large), a seq of another history, a cursor that isn't one or belongs to another request | no, fix the request | `InvalidError` | `INVALID_ARGUMENT` | 400 |
| `not_found` | A namespace doesn't exist (or was dropped); in a mutation, a node or edge; an index or constraint to drop; the start or end of a path | no | `NotFoundError` | `NOT_FOUND` | 404 |
| `conflict` | A version conflict (`expected_version`), a namespace, index or constraint that exists already, an idempotency key reused for another request. Nothing changed | after re-reading | `ConflictError` | `ABORTED` | 409 |
| `constraint_violation` | A commit would violate a unique or required constraint. Nothing changed | no | `ConstraintError` | `FAILED_PRECONDITION` | 409 |
| `budget_exceeded` | A read reached `max_results`, `max_visited` or `max_edges` and didn't ask for a partial answer ([ADR 0021](../adr/0021-bounded-reads-and-cursors.md)) | with other limits, `partial`, or a narrower request | `iwdb.Error` (step 14 adds a class) | `RESOURCE_EXHAUSTED` | 422 |
| `timeout` | The request didn't finish within its timeout (the `min_seq` wait included). Nothing changed | yes | `TimeoutError` | `DEADLINE_EXCEEDED` | 504 |
| `cancelled` | The caller cancelled the request | – | `iwdb.Error` | `CANCELLED` | 499 |
| `cursor_expired` | A paginated read's namespace changed since its first page (cursors are valid at one seq) | start again without the cursor | `iwdb.Error` (step 14 adds a class) | `FAILED_PRECONDITION` | 410 |
| `read_only` | The namespace is read-only after a failed WAL write or fsync, or a failed apply, until the store is reopened | after reopening | `ReadOnlyError` | `UNAVAILABLE` | 503 |
| `unavailable` | The store can't take the request now: shutting down, too many queued requests, the data directory locked by another store | yes, with backoff | `iwdb.Error` | `UNAVAILABLE` | 503 |
| `io` | A file operation failed. **A commit's outcome is unknown**: retry with the same idempotency key ([ADR 0015](../adr/0015-idempotency-keys.md)) | with the same idempotency key | `IoError` | `UNAVAILABLE` | 503 |
| `corrupt` | Damaged data: the WAL, a checkpoint, the namespace log, a namespace directory | no, see `iwctl verify` | `CorruptError` | `DATA_LOSS` | 500 |
| `internal` | A bug: a panic outside the commit path, a broken invariant | report it | `InternalError` | `INTERNAL` | 500 |

Notes:

- The message is for people. It is the Rust error's text, for example `version conflict on node 'ann': expected version 7, found 1`, or for a budget: `budget exceeded after visiting 100 nodes, examining 512 edges and producing 0 results`.
- A partial answer is not an error: with `QueryOptions::partial`, a read that reaches a limit answers with `truncated: true` instead of `budget_exceeded`.
- Errors of the operations outside the trait (opening a store, backup, restore, verify) are the storage layer's (`iwdb::Error`); `iwdb_query::Error::from` maps them to the codes above where an adapter needs one (a locked directory is `unavailable`).
- `cancelled` is rarely seen: the caller that cancelled has usually gone away.
