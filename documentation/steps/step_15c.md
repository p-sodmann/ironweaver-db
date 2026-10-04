# Step 15c: Audit log and `SECURITY.md`

Status: todo
Milestone: M4 Production 1.0
Depends on: step 15a (the principal every audit entry names)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

Every admin and catalog operation leaves a record of who did what, when and with what outcome; and the project tells researchers how to report a vulnerability.

## Tasks

- [ ] Audit events for logins (success and failure), logout, user, grant and token changes, namespace create and drop, catalog changes, and (after 16e) checkpoint, backup, restore and cancel: time, principal, client address, operation, namespace, outcome and error code. Never a secret or a value of the data.
- [ ] Where they go, decided in an ADR: a separate `tracing` target (`iwdb::audit`) to the log output or a file of its own, and whether it is durable with the change it records (for catalog and user changes, the WAL record already is; the audit entry names its seq).
- [ ] Emitted once, at the authorisation point of 15a (design rule 8), not in the adapters.
- [ ] `SECURITY.md`: supported versions, how to report (private advisory on GitHub, an address), what to expect and when, and the statement that the database sends no telemetry. A test or check that the binaries open no outbound connection they weren't configured for (projections only).
- [ ] Tests: each audited operation produces one entry with the right principal and outcome over gRPC and REST; no secret in any entry.
- [ ] ADR (audit log); config.md for its settings.

## Acceptance criteria

- Every admin and catalog operation is audited with principal and outcome (test per operation).
- `SECURITY.md` exists and is linked from the README.

## Non-goals

- Auditing reads or data commits (that is a change stream consumer's job).
- Shipping audit events to a SIEM (the log format is the interface).
