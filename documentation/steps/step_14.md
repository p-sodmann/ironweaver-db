# Step 14: Clients, query shell and benchmarks

Status: todo
Milestone: M3 Network access
Depends on: step 13

## Goal

Complete M3: a typed Python client with the same API as the embedded bindings, an interactive shell, and published performance numbers.

## Tasks

- [ ] `clients/python/`: sync and async client over gRPC, typed wrappers, same API shape as `iwdb-python`; tracks the last commit `seq` and sends `min_seq` for read-your-writes.
- [ ] The Python test suite runs unchanged against the embedded store and a remote server.
- [ ] `iwctl shell` (like `psql` / `redis-cli`): connect to a server, run `match` patterns, lookups and admin commands, table/JSON output.
- [ ] Benchmarks (criterion + load generator): 100k / 1M / 10M nodes; load, commit throughput per fsync policy, neighbourhood depth 2, shortest path, `match`, PageRank, memory per node/edge. Regression gate on the 100k set; results in `documentation/benchmarks.md`.

## Acceptance criteria

- Same Python tests green for embedded and remote.
- Benchmark targets set and met.
- M3 is done: update [README.md](README.md).
