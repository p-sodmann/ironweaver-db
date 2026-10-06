# Step 16e: `iwctl` against a running server; archive pruning and backup throttling

Status: done
Milestone: M4 Production 1.0
Depends on: step 16c (the admin reads and cancel)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

Every admin task can be done with `iwctl` against a running server, without its data directory; and the WAL archive and online backups are manageable at scale.

## Tasks

- [x] Admin writes behind the trait (or the `Admin` trait of 16c): `checkpoint`, `backup` (to a path on the server), `verify`, with the proto, REST and OpenAPI as in 16c. Also `prune_archive`, so the archive can be pruned without the server's host; backups go only into `[backup] dir`, by name ([ADR 0055](../adr/0055-admin-writes-and-iwctl-against-a-server.md)).
- [x] `iwctl --server <endpoint>`: `status`, `checkpoint`, `backup`, `restore` (offline: say so and refuse against a running server, or define an online form in the ADR), `verify`, index and constraint management, namespaces, `requests` and `cancel`. Through `client::Remote` only; `iwctl` holds no logic (design rule 8). `restore` (and `import`, `export`) is refused with `--server` (ADR 0055).
- [x] `iwctl archive prune --before <backup>`, and optionally recording the store's archive in the data directory so `iwctl checkpoint` needn't be told (ADR 0012). Not recorded: it would be a layout change for the offline case only, since `iwctl --server ... checkpoint` asks the store, which knows its archive. The server gets `[store] archive` instead (ADR 0055).
- [x] Throttling for online backups of large stores (checkpoints wait for a backup's copy, ADR 0009): a bytes-per-second limit, and a test that a checkpoint during a throttled backup still completes and both are consistent (design rule 3). `[backup] max_bytes_per_second`, per request too; `crates/iwdb/tests/admin_writes.rs`.

## Acceptance criteria

- [x] All admin tasks can be done with `iwctl` (a test per command against a server started by the test): `crates/iwctl/tests/server.rs`, one test per command (`status`, `checkpoint`, `backup`, `verify`, `archive prune`, namespaces, indexes and constraints, `requests` and `cancel`, `restore` refused), with authentication on.

## Non-goals

- Managed jobs' commands (step 16f adds `iwctl jobs`).
