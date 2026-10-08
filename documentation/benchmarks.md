# Benchmarks

Status: step 14b, measured 2026-10-04 at the commit that adds this file. These are numbers for one machine and one synthetic graph. They show the order of magnitude and catch regressions; they are not a promise for your workload.

## How to run them

| Tool | What it measures | Command |
|---|---|---|
| Criterion benches (`crates/iwdb-server/benches/graph.rs`) | single-call latency of each read, embedded and over gRPC; PageRank; one commit per fsync policy (one writer) | `cargo bench -p iwdb-server --bench graph` (size: `IWDB_BENCH_NODES`, default 100 000) |
| Load generator (`crates/iwdb-server/examples/loadgen.rs`) | loading, memory, export and import, then throughput and latency under concurrent gRPC clients, and commits per fsync policy | `cargo run --release -p iwdb-server --example loadgen -- --nodes 1000000` (`--degree`, `--clients`, `--seconds`, `--skip-import`, `--json`, `--check`) |
| CI gate (`bench` job) | the load generator on 100k nodes against [`targets-100k.json`](../crates/iwdb-server/benches/targets-100k.json) | `loadgen --nodes 100000 --clients 4 --seconds 3 --check crates/iwdb-server/benches/targets-100k.json` |

**The graph** (`benches/support`): `N` nodes `n0` .. `n{N-1}` with the label `Person` and four attributes (`age` = `i % 80`, indexed; `group` = `i % 1000`; `x` and `y`), and `degree` outgoing `KNOWS` edges per node to pseudo-random nodes, each with a weight `w`. It is deterministic for a given `N` and `degree`. It is loaded through commits of 10 000 mutations under the `group` fsync policy: nodes first, then edges.

**The operations:**

| Name | Request |
|---|---|
| `get_node` | one node by id |
| `neighbourhood_depth_2` | the nodes within 2 outgoing edges of a node (about 21 with degree 4) |
| `shortest_path` | BFS between two random nodes (bidirectional, unweighted) |
| `find_indexed_100` | `age == k` (one value of 80, through the index), first page of 100 |
| `match_two_hops_100` (criterion) | `(a:Person {group: k})-[:KNOWS]->(b)-[:KNOWS]->(c)`, first 100 rows. `group` isn't indexed, so this scans every node |
| `match_two_hops_from_id_100` (load generator) | the same pattern from one bound node id, first 100 rows. Bound ids can't be written in the pattern text, so this pattern crosses the wire as postcard (ADR 0023) |
| `pagerank` | PageRank over every node and outgoing edge (defaults: α 0.85, 100 iterations, tolerance 1e-6), top 10 |
| `commit.<policy>` | one `upsert_node` per commit, under `always`, `group` (10 ms or 64 commits) or `off` |

Reads ask for `partial` answers, so a read that reaches a limit is measured as truncated rather than failing.

**The machine:** Apple M4 Pro (12 cores), 24 GB, internal SSD, macOS; Rust 1.99, release build; server and clients in one process, over loopback. On macOS, `fsync` (as used by `always`) is the full-flush kind, which makes `always` slower than on most Linux disks.

## Results

### Single calls (criterion, 100k nodes, degree 4)

Median time of one call from one thread.

| Operation | Embedded | gRPC |
|---|---:|---:|
| `get_node` | 9.5 µs | 63 µs |
| `neighbourhood_depth_2` | 23 µs | 120 µs |
| `shortest_path` | 73 µs | 128 µs |
| `find_indexed_100` | 515 µs | 800 µs |
| `match_two_hops_100` (a scan) | 14.2 ms | 14.4 ms |
| `pagerank` | 14 ms | |
| commit, `off` | 4.0 µs | |
| commit, `group` | 91 µs | |
| commit, `always` | 3.9 ms | |

A call over gRPC costs about 50 to 100 µs more than embedded, which is mostly the loopback round trip and protobuf.

### Under load (load generator, 8 gRPC clients)

| | 100k nodes, 400k edges | 1M nodes, 4M edges | 10M nodes, 10M edges ¹ |
|---|---:|---:|---:|
| Load: nodes/s | 451 000 | 452 000 | 355 000 |
| Load: edges/s | 356 000 | 301 000 | 96 000 |
| Load: total | 1.3 s | 15.5 s | 133 s |
| Import (export, then import as a new namespace) | 1.0 s, 26 MiB | 14.9 s, 289 MiB | not run ² |
| Graph memory (the graph's own estimate, indexes and payloads included ⁵) | 258 MiB | 2.45 GiB | 9.3 GiB (payloads not) |
| Resident memory per node / per edge | 1.6 KB / 0.59 KB ³ | 1.36 KB / 0.44 KB | ³ |
| `get_node` | 40 200/s, p99 0.30 ms | 39 200/s, p99 0.32 ms | 39 300/s, p99 0.36 ms |
| `neighbourhood_depth_2` | 28 600/s, p99 0.42 ms | 28 100/s, p99 0.43 ms | 30 400/s, p99 0.54 ms |
| `shortest_path` | 34 200/s, p99 0.37 ms | 19 000/s, p99 0.84 ms | 6 850/s, p99 3.2 ms |
| `find_indexed_100` | 7 760/s, p99 1.5 ms | 1 120/s, p99 7.9 ms | 11/s, p99 822 ms ⁴ |
| `match_two_hops_from_id_100` | 35 100/s, p99 0.33 ms | 34 800/s, p99 0.34 ms | 38 800/s, p99 0.36 ms |
| `pagerank` (one run) | 0.01 s | 0.17 s | 1.8 s |
| Commits, `always` | 263/s, p99 345 ms | 252/s, p99 382 ms | 266/s, p99 401 ms |
| Commits, `group` | 10 700/s, p99 4.6 ms | 10 700/s, p99 4.5 ms | 12 000/s, p99 4.3 ms |
| Commits, `off` | 41 400/s, p99 0.32 ms | 41 500/s, p99 0.32 ms | 42 400/s, p99 0.31 ms |

1. Degree 1 instead of 4: 10M nodes with 40M edges need about 30 GB of resident memory, more than this machine has. Even this run went past physical memory: macOS compressed it (peak footprint 20.7 GB, resident at most 8.2 GB, no swapping). Reads that touch random memory (`shortest_path`, `find_indexed_100`) and the load of edges are slower partly for that reason.
2. Skipped at 10M (`--skip-import`): the imported namespace is a second copy of the graph in memory.
3. Resident memory is the process's (`ps`): the WAL's buffers and the allocator's slack on top of the graph, so it is more than the graph's estimate. Under memory compression (the 10M run) it isn't meaningful.
4. See the first finding below.
5. Since core `c69ef51` (upstream #61) the graph's estimate counts attribute payloads too, so the 100k and 1M rows were measured again (2026-10-08, same machine): 542 bytes per entity at 100k, about twice the earlier 278. The 10M row is from before and doesn't include them. Nothing else changed.

## Findings

- **A first page from an index costs O(candidates), not O(page).** `find` returns results sorted by id with a keyset cursor (ADR 0021), so it checks every candidate the index gives and keeps the smallest 101 ids. With 1 of 80 values per node that is 1 250 candidates at 100k, 12 500 at 1M and 125 000 at 10M. At 10M a page of 100 costs 0.7 s and is truncated at the default `max_visited` of 100 000. This is how pagination was designed, not a bug, but it makes low-selectivity index lookups expensive on large graphs. Options for a later step: iterate the index in id order (needs the core's index to keep ids sorted per key, an upstream request), or offer an unordered first page without a cursor.
- **`always` serializes on fsync.** Each commit under `always` waits for its own fsync, about 4 ms here. Eight concurrent clients get no more throughput than one, and wait in line (p99 around 400 ms). `group` gives 40 times the throughput with a bounded loss window ([guarantees.md](guarantees.md)). A group commit for `always` (one fsync for the commits waiting at that moment, each acknowledged only after it) would keep the guarantee and raise throughput. A candidate for step 16.
- **Pattern filters on properties scan without an index.** `(a:Person {group: k})` checks every `Person`. The criterion benchmark shows it (14 ms at 100k). Patterns that start from an indexed property or a bound id are fast.
- **The gRPC overhead is small and flat:** about 50 to 100 µs per call, independent of the graph's size.

## Targets

The CI gate (`bench` job) checks the 100k set against [`crates/iwdb-server/benches/targets-100k.json`](../crates/iwdb-server/benches/targets-100k.json) on every push. The targets come from two runs on the machine above with the gate's flags (4 clients, 3 s), and leave room for shared runners: throughput at least a fifth of it, p99 latency at most ten times, the graph's memory estimate at most about 40% above it. `commit.always` (disk-dependent) and resident memory (noisy) aren't gated. A target that fails after a deliberate change is updated in the same change, with the reason.

The table above, on this machine, meets every target: the 100k numbers are well within them, by design.
