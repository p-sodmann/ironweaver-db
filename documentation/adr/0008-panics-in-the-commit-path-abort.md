# ADR 0008: A panic in the commit path aborts the process

Status: accepted
Date: 2026-09-30

## Context

Step 6 requires that a panic inside the commit path is treated as a crash: the process exits, and recovery restores the state. In step 5 the store handled such a panic like a WAL failure. The panic poisoned the mutex around the live namespace, and the store then reported itself read-only until it was reopened.

That is not safe for readers. The store's reads lock the same mutex and ignore the poison, and that is deliberate: a panic in a read closure must not break the store. So after a panic in the middle of `Namespace::apply` (the core's `apply_all`), readers would see whatever part of the transaction had been applied. The core has an `expect` on that path (upstream [#28](https://github.com/p-sodmann/Ironweaver/issues/28)), so this is a real risk. It breaks the promise that no partial transaction is ever visible. A panic in the middle of a WAL append is less visible but just as wrong. The writer's position (`next_seq`, `segment_len`, `synced_seq`) may no longer match the file. Anything that carried on after it, as ignoring the poison for writes would, could append after a torn frame or report records as synced on the strength of that state.

Where a panic can happen while the store holds the live namespace for writing:

- `LoggedNamespace::commit` / `commit_catalog`: resolve and validate, WAL append and fsync (and rotation), apply;
- `Store::sync`, the fsync in `Store::checkpoint` and `close`, and the group commit timer's `sync_due`.

Options considered:

1. **`panic = "abort"`** for the harness child. This is a profile setting, so it applies to the whole build and can't be set for one binary. It also only covers our own binaries, not embedded users.
2. **A process-wide panic hook** that aborts. This would take over the host application's hook and abort on panics that have nothing to do with the store.
3. **Make the store unusable after such a panic**, reads included. Every read would need an error channel (`node`, `edge`, `seq` and `read` return plain values today). The host would still have to reopen the store, and until it did, a half-applied graph would stay in memory.
4. **Abort the process from the store**: catch the panic where the store changes its namespace or WAL, and call `std::process::abort()`.

## Decision

Option 4. `Store` runs every change to the live namespace or its WAL inside `catch_unwind`: commits, `sync`, the fsyncs of `checkpoint` and `close`, and the group commit timer. On a panic it logs the message (`log::error!` and stderr) and calls `std::process::abort()`. No destructor runs, and nothing more is written, just as with `kill -9`. The next `Store::open` recovers every commit in the log (ADR 0006). What that means for the commit in flight:

- a panic before its record was written: it is not in the log;
- a panic halfway through the write: a torn tail, which recovery cuts;
- a panic after the write (in the fsync or in apply): the record is complete in the page cache, and recovery replays it. As with a failed fsync (ADR 0005), its outcome is unknown to the client, which never got an acknowledgement.

Panics elsewhere are not crashes:

- **A panic in a `read` closure** (user code) unwinds to the caller and changes nothing. Such a panic can poison the mutex, and a poisoned mutex is now always harmless, because every panic that could have left a partial change aborts first. So the store ignores the poison for writes as well as reads. A read panic no longer makes the store read-only (a step 5 bug, fixed in step 6).
- **A panic in the checkpointer** hits its own copy of the namespace, which no reader sees. If it happens while replaying, the copy is dropped and loaded again at the next run. The background thread ends, and later checkpoints come only from `Store::checkpoint` and `close`. On disk it leaves nothing that recovery doesn't handle: at worst a temporary file (removed on open), or a new checkpoint whose old files weren't removed yet.
- **A panic during recovery** (`Store::open`): nothing has changed yet except temporary files and a torn tail. It unwinds to the caller of `open`, and the next open starts over.

## Consequences

- No reader ever sees a partial transaction, not even after a bug in the apply path. The guarantee in [guarantees.md](../guarantees.md) holds without exceptions.
- An embedded application, and the Python interpreter from step 7, dies on such a bug instead of getting an error. That is the same trade-off PostgreSQL makes when it PANICs, and it happens only on a bug, never on user input. If embedded users need to survive it, a later step can add an option that keeps the process alive and closes the store for reads as well (option 3). Until then, the abort is documented on `Store`.
- In the server (step 11), a panic in one namespace's commit path takes down the whole process and every namespace with it. Upstream #28 (make the apply path panic-free) matters more then; the row in [upstream-check.md](../steps/upstream-check.md) says so.
- The test in `crates/iwdb/tests/panics.rs` and the crash harness (step 6) cover it. A failpoint panics in the WAL write, halfway through it, in the fsync and in the group commit timer. The child process must die with `SIGABRT`, and recovery must restore the reference state.

## Update (step 11)

Upstream #28 is fixed: the core's apply path returns `GraphError::Internal` instead of panicking. [ADR 0028](0028-internal-apply-errors-abort.md) keeps this ADR's abort for the server and extends it: an apply that fails with `GraphError::Internal` (the graph may hold part of the transaction) aborts like a panic.
