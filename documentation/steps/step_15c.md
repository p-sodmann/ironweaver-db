# Step 15c: Audit log and `SECURITY.md`

Status: done
Milestone: M4 Production 1.0
Depends on: step 15a (the principal every audit entry names)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

Every admin and catalog operation leaves a record of who did what, when and with what outcome; and the project tells researchers how to report a vulnerability.

## Tasks

- [x] Audit events for logins (success and failure), logout, user, grant and token changes, namespace create and drop, catalog changes, and (after 16e) checkpoint, backup, restore and cancel: time, principal, client address, operation, namespace, outcome and error code. Never a secret or a value of the data. (Checkpoint, backup, restore and cancel are left out: step 16e, which adds them to the API, isn't done. When it is, they get rows in the operation table marked `Always`, and the role test requires their entries.)
- [x] Where they go, decided in an ADR: a separate `tracing` target (`iwdb::audit`) to the log output or a file of its own, and whether it is durable with the change it records (for catalog and user changes, the WAL record already is; the audit entry names its seq).
- [x] Emitted once, at the authorisation point of 15a (design rule 8), not in the adapters.
- [x] `SECURITY.md`: supported versions, how to report (private advisory on GitHub, an address), what to expect and when, and the statement that the database sends no telemetry. A test or check that the binaries open no outbound connection they weren't configured for (projections only).
- [x] Tests: each audited operation produces one entry with the right principal and outcome over gRPC and REST; no secret in any entry.
- [x] ADR (audit log); config.md for its settings.

## Acceptance criteria

- Every admin and catalog operation is audited with principal and outcome (test per operation).
- `SECURITY.md` exists and is linked from the README.

## Non-goals

- Auditing reads or data commits (that is a change stream consumer's job).
- Shipping audit events to a SIEM (the log format is the interface).

## Outcome

[ADR 0049](../adr/0049-audit-log.md); [SECURITY.md](../../SECURITY.md); [config.md](../api/config.md#audit-log); [guarantees.md](../guarantees.md#audit-log-step-15c).

- Entries are made at the authorisation point: `Authorized<D>`, `iwdb_query::auth::login`, and the gate for the requests it refuses. They go to the log (target `iwdb::audit`) and, with `[audit] dir`, to daily files kept for `[audit] retention_days` (default 30). Retention was added at the owner's request: the files must not grow indefinitely.
- Refusals of every operation are audited, reads included. Successful reads and data commits aren't.
- Entries aren't durable with the change. Catalog changes and namespace events name their seq; user, grant and token changes don't (`Accounts` returns none).
- The no-telemetry check is a binary test that lists the server's sockets with `lsof`. The console's fonts are served by the server instead of Google Fonts (the owner's choice, found while writing `SECURITY.md`).
- Vulnerability reports go through GitHub's private vulnerability reporting, which is now turned on for the repository; there is no contact address.
