# Step 16e: `iwctl` against a running server; archive pruning and backup throttling

Status: todo
Milestone: M4 Production 1.0
Depends on: step 16c (the admin reads and cancel)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

Every admin task can be done with `iwctl` against a running server, without its data directory; and the WAL archive and online backups are manageable at scale.

## Tasks

- [ ] Admin writes behind the trait (or the `Admin` trait of 16c): `checkpoint`, `backup` (to a path on the server), `verify`, with the proto, REST and OpenAPI as in 16c.
- [ ] `iwctl --server <endpoint>`: `status`, `checkpoint`, `backup`, `restore` (offline: say so and refuse against a running server, or define an online form in the ADR), `verify`, index and constraint management, namespaces, `requests` and `cancel`. Through `client::Remote` only; `iwctl` holds no logic (design rule 8).
- [ ] `iwctl archive prune --before <backup>`, and optionally recording the store's archive in the data directory so `iwctl checkpoint` needn't be told (ADR 0012).
- [ ] Throttling for online backups of large stores (checkpoints wait for a backup's copy, ADR 0009): a bytes-per-second limit, and a test that a checkpoint during a throttled backup still completes and both are consistent (design rule 3).

## Acceptance criteria

- All admin tasks can be done with `iwctl` (a test per command against a server started by the test).

## Non-goals

- Managed jobs' commands (step 16f adds `iwctl jobs`).
