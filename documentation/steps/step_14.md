# Step 14: Python query methods and the remote client

Status: done
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
- [x] `find`, `explain`, `neighbourhood`, `traverse`, `shortest_path`, `random_walks`, `subgraph`, `match`, `analyze` on `Store` and `Namespace` (moved here from step 10), through the `Database` trait, with the limits (`max_results`, `max_visited`, `max_edges`, `timeout`), `partial`, cursors and `min_seq`. Answers are plain dicts.
- [x] Filters from Python: a small builder (`iwdb.attr("age") >= 18`, `iwdb.label("Person")`, `&`, `|`, `~`) that becomes the core's `Expr`, with values converted as in the Values table and the core's depth limit. Patterns are the core's text.
- [x] Exception classes for `budget_exceeded`, `cursor_expired`, `not_retained` and `unavailable` ([errors.md](../api/errors.md)).

### Remote client
- [x] `iwdb.connect(endpoint)`: a `Store` served by `Remote` over gRPC, with the API of [python-api.md](../python-api.md) except the calls that need the store's directory (they raise `InvalidError`; the list is in python-api.md).
- [x] Read-your-writes: the client tracks the seq of its last commit per namespace and sends it as `min_seq` when a read gives none.
- [x] `iwdb.aio`: `AsyncStore`, `AsyncNamespace`, `AsyncTransaction` over the sync API with `asyncio.to_thread`.

### Tests and CI
- [x] The `store` fixture of `crates/iwdb-python/tests/conftest.py` runs every test that uses it against an embedded store and against a running `iwdb-server`; tests of local-only calls are marked `embedded`.
- [x] Remote-only tests: no server (`UnavailableError`), read-your-writes between two clients, closing, the local-only calls.
- [x] CI's `python` job builds `iwdb-server` and runs the suite with the remote half required.

### Docs
- [x] ADR 0035; python-api.md (queries, filters, `connect`, `aio`, the new exceptions); errors.md's Python column; AGENTS.md's layout.

## Notes

- **Filter depth.** The embedded store doesn't check how deep a filter nests; only the core's serde does, on the way to a server. The bindings check it themselves, with the core's rule (a leaf counts as a level: 99 operators around a leaf pass, 100 fail), so a filter fails with `ValueError` the same way on both.
- **The sdist.** It now has to carry `iwdb-server` and the protos. maturin refuses include patterns outside the Python crate, so `crates/iwdb-server/proto` is a symlink to `proto/`, and the server's build script reads the protos through it. The release workflow builds a wheel from the sdist.
- **Stubs.** `_iwdb.pyi` had fallen behind since step 9 (namespaces, `checkpoint_all`); it now covers the whole API, with the per-namespace methods in one `_Graph` base.
- **Label propagation** has no default `max_iter` in the core; the Python API uses 100.

## Acceptance criteria

- The same Python tests are green for embedded and remote (apart from the tests marked `embedded`).
- Every query method works through both, and its errors raise the documented classes.
- `iwdb.aio` works against both.

## Non-goals

- A pure-Python client (no Rust in the wheel); native awaitables. Possible later; see ADR 0035.
- The query shell (step 14a) and benchmarks (step 14b).
- Authentication and TLS (step 15): `connect` speaks plain gRPC.
- Windows wheels (ADR 0013).
