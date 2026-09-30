# Ironweaver DB: a production-grade database around `ironweaver-core`

Status: design proposal (updated 2026-09-30 for the `ironweaver-core` 0.2 rewrite). No code yet. Implementation is tracked in [steps/](steps/README.md).

## Context

[Ironweaver](https://github.com/p-sodmann/Ironweaver) 0.2 split its engine into a pure-Rust crate, `ironweaver-core`: a directed property multigraph with persistent edge ids, labels, typed values, an op log with undo, property indexes, filter expressions, pattern matching, analytics and a checksummed file format. It is still an in-process library: no WAL, no crash recovery, no concurrency control, no network access.

Ironweaver DB is a **separate project** that turns `ironweaver-core` into a durable, concurrent graph database that can be embedded or run as a server. What the core already provides, and which small upstream changes would help, is in [ironweaver-core-review.md](ironweaver-core-review.md).

## Guiding decisions

1. **Build on `ironweaver-core`, don't fork it.** Use its `Graph`, `Op`, `Expr`, `Pattern`, `Projection`, algorithms and file format. Anything that belongs in the engine goes upstream as a PR.
2. **Rust server, Python as a client.** The server embeds the core directly. Python talks to it over the network, or uses the embedded bindings.
3. **Two deployment modes, one engine.** Embedded durable mode ("SQLite for graphs") and server mode share the storage and transaction layers.
4. **Single writer, many readers.** Every change goes through one commit pipeline and the WAL. Readers never see partial transactions.
5. **One service interface, many access methods.** Embedded Rust, embedded Python, gRPC, REST/JSON and the CLI are thin adapters over the same `Database` service, so behaviour, limits and errors are identical everywhere.
6. **Every read is bounded.** Result limits, visit budgets and timeouts on everything that goes over a boundary.
7. **Stay a graph database.** No full-text search, vector search, SQL or job queues. Integrate with other systems via the change stream and projection mode.

## Architecture

```
 Access     embedded Rust │ Python (PyO3) │ gRPC (tonic) │ REST/JSON (axum) │ change stream │ iwctl
            ─────────────────────────────────────────────────────────────────────────────────────
 Service    Database trait: sessions, auth context, limits, error model
 Query      get / neighbourhood / paths / match / filter / analytics jobs, EXPLAIN, pagination
 Engine     commit pipeline (seq, OCC, idempotency, constraints) │ catalog (namespaces, indexes, roles)
 Storage    WAL segments │ checkpoints │ recovery │ backup / PITR │ verify
 Core       ironweaver-core: Graph, Op, Expr, Pattern, Projection, algorithms, format v2
```

| Layer | Crate | Borrowed from |
|---|---|---|
| Storage | `iwdb-storage` | Postgres WAL, `pg_basebackup` and PITR; Redis AOF fsync modes (`always` / `everysec` / `no`) and RDB snapshots; SQLite file locking and `PRAGMA integrity_check` |
| Transactions | `iwdb-engine` | etcd revisions and compare-and-set transactions; SQLite's single writer |
| Catalog | `iwdb-engine` | Postgres `pg_catalog`: databases (namespaces), indexes, constraints, roles, all changed through logged operations |
| Query | `iwdb-query` | Postgres `EXPLAIN` and `statement_timeout`; Neo4j Cypher (via the core's pattern language) |
| Service and access | `iwdb` (embedded facade), `iwdb-python`, `iwdb-server`, `iwctl` | SQLite (embedded), etcd (gRPC and Watch from a revision), Neo4j HTTP API, CouchDB `_changes`, `psql` / `redis-cli` |
| Operations | `iwdb-server`, `iwctl` | Postgres `pg_stat_*` views and roles/GRANT; Prometheus conventions |

## Access methods

| Method | For | Notes |
|---|---|---|
| Embedded Rust (`iwdb::Store`) | Rust applications, tests | Opens a data directory; full durability; no network |
| Embedded Python (`iwdb-python`) | Python apps and tests | Same API shape as the remote client, so code can switch |
| gRPC | Services, other languages | Canonical contract in `proto/`; server streaming for large results and the change stream |
| REST/JSON | Browsers, scripts, curl | Same message shapes as the protos (via `pbjson`); OpenAPI generated; SSE for streams |
| Change stream | Caches, indexers, replicas | Resume from any retained `seq` |
| `iwctl` | Operators | Admin commands plus an interactive query shell |

## Workstreams

### 1. Foundation on `ironweaver-core`
- Depend on a pinned revision; get the upstream changes from the review in (done: PR #25, `a14149e`).
- DB payload `DbRecord { attr, meta, version }` implementing `Attributes`, `AttrPatch` and a `Codec`.
- Canonical-state comparison for tests (order-independent).

### 2. Transactions and commit pipeline
- A transaction is a list of high-level mutations (upsert, delete, set/remove/append attribute, label and type changes; a merge is several attribute sets in one transaction) with optional `expected_version` per entity.
- The single writer resolves them into plain `Op`s with explicit edge ids and the new versions ([ADR 0004](adr/0004-version-ops.md)), checks constraints on the state after the whole transaction, then applies them with `apply_all`, flushes indexes and assigns the next `seq`.
- Idempotency keys for retried writes. Read-your-writes via `min_seq`.

### 3. Storage: durability and recovery
- WAL: segmented, append-only, CRC32C per record, records = resolved ops (or a catalog change) + `seq` ([format](formats/wal.md)).
- fsync policies: `always`, `group` (every N ms or N records), `off` (tests), each with a documented guarantee ([ADR 0005](adr/0005-wal-fsync-and-failures.md), [guarantees](guarantees.md)).
- Checkpoints by a background checkpointer (load checkpoint, replay WAL, `write_atomic` the new one with `seq` in meta), then WAL truncation.
- Recovery: newest valid checkpoint, replay WAL, stop at a torn tail.
- Data-directory lock, online backup, point-in-time restore, `verify`.

### 4. Catalog
- Namespaces (one graph each), index definitions, constraints (unique, required, endpoint existence), roles. Catalog changes are WAL records too.

### 5. Query layer
- Operations: get/multi-get, neighbourhood, BFS/DFS, shortest paths, random walks, subgraph extraction, `match` patterns with `Expr` filters, analytics jobs on a `Projection`.
- Every read: max results, max visited, timeout (cancel token), cursor pagination. `EXPLAIN` shows index use.
- Error model shared by all access methods.

### 6. Access methods
- `Database` service trait; embedded facade; Python bindings; gRPC server; REST gateway; `iwctl` shell.

### 7. Change data capture and integration
- Change stream from any retained `seq`.
- Projection mode: follow an external ordered event log (e.g. a Postgres outbox table), storing the high-water mark in the same transaction.
- Bulk import/export: ironweaver JSON/binary, LGF, CSV/Parquet edge lists, GraphML.

### 8. Security
- TLS by default, API tokens and mTLS (OIDC later), roles per namespace, resource limits per client and namespace, audit log, `SECURITY.md`.

### 9. Operability
- Config file + environment, health/readiness, Prometheus metrics, OpenTelemetry traces, JSON logs, `status` views, defined memory-limit behaviour, graceful shutdown.

### 10. Quality and release
- Model-based property tests, crash and fault injection, fuzzing (WAL reader, checkpoint loader, protocol decoding), format compatibility fixtures, benchmarks with CI gates, 24h soak.
- Semver releases: crates.io, PyPI wheels, multi-arch Docker image, docs site.

### 11. Replication and HA (after 1.0)
- WAL shipping to read replicas, manual failover first, Raft only if needed, Jepsen-style tests.

## Milestones

| Milestone | Steps | Done when |
|---|---|---|
| **M0 Foundation** | 1–3 | Transactions commit in memory with versions, resolved ops and `seq`; upstream changes landed |
| **M1 Embedded durable** | 4–7 | Kill -9 suite green; PITR works; Python embedded wheels on PyPI |
| **M2 Service** | 8–10 | Concurrent reads, catalog, bounded query layer behind the `Database` trait |
| **M3 Network access** | 11–14 | gRPC, REST and change stream served; Python remote client passes the same tests as embedded; benchmarks published |
| **M4 Production 1.0** | 15–17 | Security, operability, full verification suite; 1.0 released |
| **M5 HA** | 18 | Replicas, failover, Jepsen-style tests |
