# Step 14a: Query shell

Status: todo
Milestone: M3 Network access
Depends on: step 14

Split out of step 14 on 2026-10-04 (see its "Plan change").

## Goal

`iwctl shell`, an interactive client like `psql` or `redis-cli`: connect to a server, run `match` patterns, lookups and admin commands, and print tables or JSON.

## Tasks

- [ ] `iwctl shell <endpoint>` over `iwdb_server::client::Remote` (feature `client`), through the `Database` trait only.
- [ ] Commands: `match` patterns (the core's text), `node` / `edge` lookups, `find` with a filter, the catalog and namespace commands of `iwctl`, switching the namespace; limits and `partial` as options.
- [ ] Output as tables (default) or JSON (`\json`), with `cursor` paging (`\next`).
- [ ] Errors print their code and message and don't end the shell.
- [ ] Tests against a server started by the test.

## Acceptance criteria

- A session can create a namespace, commit data (through an admin command or an import), run a `match` and page through a `find`.

## Non-goals

- A query language beyond the core's pattern text and filters.
- Line editing beyond what a well-known crate gives cheaply (decide in the step, with a one-line justification for the dependency).
