# Step 14: Python query methods and the remote client

Status: in progress
Milestone: M3 Network access
Depends on: step 13

## Goal

One Python API for both deployment modes: the query methods in the embedded bindings, and `iwdb.connect(...)`, a remote client with the same API, the same result dicts and the same exceptions. The Python test suite runs unchanged against both. Design: [ADR 0035](../adr/0035-python-remote-client.md).

## Plan change (2026-10-04)

The original step 14 also held the query shell and the benchmarks, and planned the remote client as a separate package in `clients/python/`. When the step was refined:

- **The shell moved to [step 14a](step_14a.md), the benchmarks to [step 14b](step_14b.md).** They are separable and each is a PR's worth of work. M3 is done after step 14b.
- **The remote client is native, in the `ironweaver-db` wheel, not a pure-Python package.** Values, filters and patterns cross gRPC as postcard bytes in the core's serde layout ([ADR 0023](../adr/0023-wire-encoding-of-values-filters-and-patterns.md)), so a pure-Python client would need a second postcard codec and a second copy of the conversions, exceptions and API. The bindings instead serve a `Store` from `iwdb_server::client::Remote` ([ADR 0024](../adr/0024-the-rust-grpc-client.md)) as well as from `iwdb::Embedded`: one conversion layer and one error mapping (design rules 8 and 9). `clients/python/` isn't created. ADR 0035 records the decision and its costs.
- **Async is a thin `asyncio` wrapper** over the sync calls (`iwdb.aio`); native awaitables are a non-goal.

## Tasks

### Query methods (embedded and remote)
- [ ] `find`, `explain`, `neighbourhood`, `traverse`, `shortest_path`, `random_walks`, `subgraph`, `match`, `analyze` on `Store` and `Namespace` (moved here from step 10), through the `Database` trait, with the limits (`max_results`, `max_visited`, `max_edges`, `timeout`), `partial`, cursors and `min_seq`. Answers are plain dicts.
- [ ] Filters from Python: a small builder (`iwdb.attr("age") >= 18`, `iwdb.label("Person")`, `&`, `|`, `~`) that becomes the core's `Expr`, with values converted as in the Values table and the core's depth limit. Patterns are the core's text.
- [ ] Exception classes for `budget_exceeded`, `cursor_expired`, `not_retained` and `unavailable` ([errors.md](../api/errors.md)).

### Remote client
- [ ] `iwdb.connect(endpoint)`: a `Store` served by `Remote` over gRPC, with the API of [python-api.md](../python-api.md) except the calls that need the store's directory (they raise `InvalidError`; the list is in python-api.md).
- [ ] Read-your-writes: the client tracks the seq of its last commit per namespace and sends it as `min_seq` when a read gives none.
- [ ] `iwdb.aio`: `AsyncStore`, `AsyncNamespace`, `AsyncTransaction` over the sync API with `asyncio.to_thread`.

### Tests and CI
- [ ] The `store` fixture of `crates/iwdb-python/tests/conftest.py` runs every test that uses it against an embedded store and against a running `iwdb-server`; tests of local-only calls are marked `embedded`.
- [ ] Remote-only tests: no server (`UnavailableError`), read-your-writes between two clients, closing, the local-only calls.
- [ ] CI's `python` job builds `iwdb-server` and runs the suite with the remote half required.

### Docs
- [ ] ADR 0035; python-api.md (queries, filters, `connect`, `aio`, the new exceptions); errors.md's Python column; AGENTS.md's layout.

## Acceptance criteria

- The same Python tests are green for embedded and remote (apart from the tests marked `embedded`).
- Every query method works through both, and its errors raise the documented classes.
- `iwdb.aio` works against both.

## Non-goals

- A pure-Python client (no Rust in the wheel); native awaitables. Possible later; see ADR 0035.
- The query shell (step 14a) and benchmarks (step 14b).
- Authentication and TLS (step 15): `connect` speaks plain gRPC.
- Windows wheels (ADR 0013).
