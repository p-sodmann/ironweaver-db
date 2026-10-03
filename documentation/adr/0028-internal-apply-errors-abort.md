# ADR 0028: The server keeps the abort, and an internal apply error aborts too

Status: accepted
Date: 2026-10-03

## Context

ADR 0008 turns a panic in the commit path into a process abort, so that no reader ever sees part of a transaction and no writer continues from a WAL position that may disagree with the file. Its consequences note that in the server (step 11) such a panic takes down every namespace, and that upstream #28 (make the core's apply path panic-free) would matter more then. #28 is fixed: since `3b15149`, `RemoveNode`, `RenameNode` and `RemoveEdge` check consistency first and return `GraphError::Internal` instead of panicking, and since `a14149e` a failing undo in `apply_all`'s rollback returns `GraphError::Internal` too.

Two questions for the server:

1. Should a panic in one namespace's commit path still abort the whole process?
2. What happens now on the paths that used to panic?

On the second: `Namespace::apply` turns any error of `apply_all` into `ApplyFailed` and poisons the namespace; the store then refuses commits (`read_only`) but **keeps serving reads**. That is right when `apply_all` rolled back cleanly: the graph is as before the commit. It is not right for `GraphError::Internal`. The core says the graph "may then be inconsistent and should be reloaded": the rollback failed halfway (part of the transaction stays applied), or an op found the graph already inconsistent. Before #28 those cases panicked and ADR 0008 aborted; after it, readers would go on reading a graph that may hold part of a transaction. The upstream fix quietly weakened our guarantee ("no partial transaction is ever visible").

## Decision

1. **Panics still abort, in the server too.** #28 removed the known panics from the core's apply path, so what is left is a bug in our code, in the core outside the known paths, or in std. For those, nothing short of recovery from checkpoint and WAL restores a state we can vouch for. Isolating the namespace instead (refuse its reads and writes, keep the others) would need reopening one namespace inside a running store, which the store can't do, and a namespace whose WAL writer position is unknown can't safely be written by anything until then. A server runs under a supervisor (systemd, Kubernetes) that restarts it; recovery restores every logged commit; clients retry with idempotency keys. That is the PostgreSQL PANIC trade-off ADR 0008 already chose. `documentation/api/grpc.md` says the server must run under a supervisor.
2. **An apply that fails with `GraphError::Internal` aborts like a panic.** The store checks the result of every commit (data and catalog) inside the code that `or_abort` guards: `ApplyFailed` with an `Internal` error logs the message and aborts. Every other `ApplyFailed` keeps today's behaviour (the namespace becomes read-only, reads go on), because the core guarantees the rollback restored the graph. The record was in the WAL before apply, so recovery replays it; as with a panic after the write, the client never got an answer and its outcome is unknown.

Tested with a failpoint in the engine (`iwdb-engine` feature `failpoints`, `failpoint::fail_next_apply`) that makes the next apply return `GraphError::Internal`: `crates/iwdb/tests/apply_failures.rs` runs a child that commits until it hits it, checks that the child died with SIGABRT, and that recovery restores the reference state including the commit in flight. A clean rollback (`fail_next_apply` with a non-internal error) leaves the namespace read-only and readable, as before.

## Consequences

- The guarantee in `guarantees.md` holds again without exceptions: no reader sees a partial transaction, whether the bug shows as a panic or as `Internal`.
- One bug in one namespace's apply path takes down the whole server and all its namespaces. Upstream #28 makes that rare; if it shows in practice, a later step can add namespace isolation (reopen one namespace in a running store).
- If the bug is deterministic for that record, replaying it at the next open fails the same way: open reports `ReplayFailed` and changes nothing on disk. The store then needs a core fix, or a restore to the seq before the record (ADR 0009). That is no different from a panic in apply after the write; aborting doesn't create the problem, it stops readers from seeing it.
- ADR 0008 stands; this ADR extends its trigger from panics to internal apply errors.
