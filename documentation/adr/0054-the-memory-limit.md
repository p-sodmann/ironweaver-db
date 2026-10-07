# ADR 0054: The memory limit

Status: accepted
Date: 2026-10-06

## Context

Step 16d asks the server to warn, then refuse writes, before the operating system kills it for memory, and to keep serving reads. Step 16c reports memory per namespace as the core's `Graph::memory_usage` (ADR 0050), which is O(1) but leaves out the payloads' heap: attribute maps, their keys, strings and lists.

Points to decide: what the limit counts and how well, the defaults, where the default limit comes from, how the state changes back, which writes are refused and with which error, and where the check sits.

**What the pinned core (`7e7b7fa`) reports.**

- `Graph::memory_usage`: slots (the payload's inline size included), ids, adjacency lists, labels, the id, edge-id and label indexes, and the property indexes. Not the heap a payload owns. That is documented, and pinned by `memory_usage_leaves_out_payloads` (`core_smoke.rs`).
- `Projection::memory_usage` for a finished projection. Nothing for the raw one that `Projection::collect` returns, and nothing for an `IndexBuild` in progress.
- No hook to report a payload's heap: `Graph<N, E>` knows nothing about `N` beyond `Attributes` and `AttrPatch`.

**Measured** under a counting allocator (a scratch binary, not in the repo, since `unsafe_code` is forbidden here). It builds each namespace through `Namespace::commit` and compares the allocator's live bytes with the estimates. The seed is the console's sample data (`console/tools/seed.mjs`'s namespaces), and the generated graphs are the load generator's numeric one and a text-heavy one with names, emails, a 200-byte description and tags per node, each with 100 000 nodes and 400 000 edges.

| Namespace | Heap | `memory_usage` | `memory_usage` + payload estimate |
|---|---|---|---|
| seed: `social` (82 nodes, 272 edges) | 0.25 MiB | 47 % | 104 % |
| seed: `orders` (420, 390) | 0.49 MiB | 50 % | 105 % |
| seed: `inventory` (122, 253) | 0.24 MiB | 39 % | 104 % |
| numeric (100k, 400k) | 306 MiB | 39 % | 103 % |
| text (100k, 400k) | 353 MiB | 35 % | 103 % |

- So the core sees a third to a half of the heap, and the payloads are the rest.
- An analytics projection of the 100k graphs peaks at 10.5–11.4 MiB while it is collected, and holds 5.9–6.8 MiB once finished, which its `memory_usage` reports exactly.
- An index build in progress holds 7.5 MiB (an integer key) to 21.6 MiB (an email) for 100k nodes. Nothing reports it.
- The checkpointer keeps a second copy of every namespace it has checkpointed (step 5), so the same again.

## Decision

**What `[memory] limit_bytes` counts** (`used`), in four parts, each reported:

- **`graph`**: every live namespace's `Graph::memory_usage`.
- **`payload`**: every live namespace's payload estimate, `Namespace::payload_bytes`. `DbRecord::heap_bytes` estimates a record's maps from their lengths: std's hash-table layout, keys, and the strings, lists, maps and bytes of the values. Lengths, not capacities, so a replayed copy has the same estimate as the live one. Every apply and replay keeps it up to date in O(entities the record touches): the payloads of the nodes and edges its ops name, plus the edges of removed or renamed nodes, summed before and after. Loading a checkpoint computes it in O(graph). **This estimate is ours until the core reports payloads** ([upstream draft 25](../upstream-issues.md#25-memory_usage-cant-count-payloads-and-an-index-build-reports-no-memory)).
- **`checkpoint`**: every checkpointer's copy, graph and payloads alike, once loaded.
- **`working`**: analytics projections and index builds while they run, with these estimates:
  - A projection is charged `16 bytes × edges + 48 bytes × nodes + 4 bytes × node slots` while it is collected (within 6 % of the measured peaks), then its own `memory_usage` until the job ends.
  - An index build is charged `8 bytes × nodes` for its handles plus `96 bytes × nodes scanned`. That is a marked estimate: it is between the measured 75 bytes (integer key) and 216 bytes (email) per entry. Draft 25 asks the core for the real figure.

*Update, upstream check of 2026-10-07: #61 was fixed in core `c69ef51`.* The parts are now three:
- **`graph`** is every live namespace's `Graph::memory_usage` with payloads counted. Every namespace's graph turns that on with `count_payloads()` (`Namespace::new`, `from_loaded`), and `DbRecord` implements the core's `HeapSize` (lengths, not capacities, as before).
- `payload`, `Namespace::payload_bytes`, `DbRecord::heap_bytes` and the per-apply `Touched` sums are gone.
- `MemoryStatus.payload_bytes` (proto field 3) is reserved, and the metric's `part` label has no `payload` value. That is a breaking change to the status and the metrics, made before 1.0.
- An index build is charged its handles plus the core's `IndexBuild::memory_usage`.
- A projection keeps the formula while it is collected, because it is charged before it allocates. Once collected it is charged `RawProjection::memory_usage`, then the sorted projection's `memory_usage`.

**Not counted:** request and response buffers, the WAL's write buffer, the change stream's reads, idempotency key tables, the runtime, and the allocator's slack. The defaults leave room for them.

**Thresholds** are fractions of the limit: `warn_at` (default 0.80) and `refuse_writes_at` (default 0.90), with `0 < warn_at ≤ refuse_writes_at ≤ 1`. The estimate is 3–8 % above the heap in the measurements, so the 10 % between refusal and the limit is for what isn't counted.

**Hysteresis.** A fixed band of 5 % of the limit, no setting. The state rises as soon as `used` reaches a threshold, and falls only once `used` is below that threshold minus 0.05:

- `normal` → `warn` at 80 %, back below 75 %.
- `warn` → `refusing writes` at 90 %, back to `warn` below 85 %.

So a namespace hovering at the line doesn't flip between accepting and refusing with every commit. The state is re-evaluated whenever a part changes, not by a timer.

**The default limit.** An unset `limit_bytes` reads the cgroup, on Linux only:

- **cgroup v2**: the `0::<path>` line of `/proc/self/cgroup` gives `/sys/fs/cgroup<path>/memory.max`, falling back to `/sys/fs/cgroup/memory.max`. `max` means no limit.
- **cgroup v1**: the line naming the `memory` controller gives `/sys/fs/cgroup/memory<path>/memory.limit_in_bytes`, falling back to `/sys/fs/cgroup/memory/memory.limit_in_bytes`. Values of 2^60 or more mean no limit (v1 reports "unlimited" as a page-aligned `i64::MAX`).
- Elsewhere, or if none is found, there is no limit. `limit_bytes = 0` turns the limit off explicitly.

The status reports where the limit came from: `config`, `cgroup v2`, `cgroup v1`, or nothing. An embedded store (Rust or Python) has no limit unless its `StoreOptions` set one: it shares its process with an application whose memory it can't see.

**Which writes are refused, while refusing:**

- data commits, marks included (projections);
- catalog changes that add an index or a constraint, refused before an index build starts;
- creating a namespace, and imports.

**Still accepted:**

- commits whose every op only removes something: nodes, edges, labels, attributes (with the version bumps the pipeline adds), so that an application can free memory;
- dropping an index, a constraint or a namespace;
- the system namespace: users, grants, and the session tokens that logins write, so that an operator can still log in and act;
- a commit repeated with its idempotency key, which answers its original result as it does on a read-only namespace;
- every read, `analyze` included (it is counted, not refused).

**The error is a new code, `resource_exhausted`**, not `read_only` with a reason:

- `read_only` means a namespace failed (a WAL write, an fsync, an apply), stays so until the store is reopened, and its advice is "after reopening". Clients and the console treat it as broken.
- A memory refusal is server-wide, clears by itself once memory falls, and its advice is to retry with backoff.
- Merging them would make the code's advice wrong for one of the two, and clients branch on codes.

Its mappings: gRPC `RESOURCE_EXHAUSTED` (which `budget_exceeded` also uses; `iwdb-code` tells them apart), HTTP 503 (the server can't take it now, like `unavailable`), and Python's `ResourceExhaustedError`. Nothing changed, and retrying with backoff is safe; with an idempotency key it is safe in any case. The message names the used and limit bytes and says that deletes and drops are accepted.

**One check, before the log.**

- `iwdb_storage::memory::Memory` holds the parts (atomics), the limit and the state. The store owns one and gives every `LoggedNamespace` and `Checkpointer` a handle to it.
- `LoggedNamespace::commit_now` asks it after preparing a commit and after the idempotency lookup, before the WAL append. That is the same place the read-only check is, in the one commit pipeline (design rule 2).
- `commit_catalog_keyed` asks before an index build starts.
- The namespace log asks before a create or an import is logged.
- No adapter checks anything: gRPC, REST, Python and `iwctl` only map the error (design rule 8).

**A commit is admitted on the state before it.** One commit can carry `used` past the line by its own size. A WAL record is at most 64 MiB, so with its apply that is at most a few hundred MiB on a single commit, and the next commit is refused.

**Visible:**

- `MemoryStatus` (`GetServerStatus`) gains the parts, `used_bytes`, the thresholds in bytes, the state and the limit's source; `limit_bytes` is filled.
- Metrics: `iwdb_memory_used_bytes{part}`, `iwdb_memory_limit_bytes`, `iwdb_memory_warn_bytes`, `iwdb_memory_refuse_writes_bytes` and `iwdb_memory_state` (0 normal, 1 warn, 2 refusing writes). `part` is one of four fixed values, so the labels stay bounded (ADR 0050).
- Each change of state is logged once: `warn` when entering warn, `error` when writes start being refused, `info` when the state falls.
- The console reads the thresholds from the status. `U.MEMORY_WARN` is gone.

## Consequences

- The accounting is good to a few percent for the graph and its payloads, and an estimate for builds. RSS can still be higher than `used` by the allocator's slack and the uncounted buffers. An operator who sees the process killed below the limit lowers `refuse_writes_at` (or `limit_bytes`).
- Under pressure an application can still delete and drop to get below the line, and operators can still log in.
- Memory freed by a checkpointer is counted when its copy is dropped. A checkpointer's copy is kept between runs (step 5), so it stays in `checkpoint` until the namespace is dropped or the store closes.
- Per-namespace and per-client limits (step 15d) can build on the per-namespace estimate, `Namespace::memory_bytes` (the core's figure, payloads included).
- ~~When the core counts payloads (draft 25), `payload` becomes part of `graph` and `DbRecord::heap_bytes` goes.~~ Done in the upstream check of 2026-10-07 (core `c69ef51`).
