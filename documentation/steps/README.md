# Implementation steps

Ordered task list for [Ironweaver DB](../ironweaver-db.md), built on `ironweaver-core` (see the [core review](../ironweaver-core-review.md)). Work on one step at a time (see [AGENTS.md](../../AGENTS.md)). Each step file lists its goal, tasks, acceptance criteria and non-goals. Later steps are intentionally less detailed; refine a step when it becomes the next one.

| Step | Title | Milestone | Status |
|---|---|---|---|
| [1](step_1.md) | Bootstrap on `ironweaver-core` | M0 | done |
| [2](step_2.md) | DB payload and catalog model | M0 | done |
| [3](step_3.md) | Commit pipeline (in memory) | M0 | done |
| [4](step_4.md) | Write-ahead log | M1 | done |
| [5](step_5.md) | Checkpoints, recovery and `Store::open` | M1 | done |
| [6](step_6.md) | Crash and fault-injection suite | M1 | done |
| [7](step_7.md) | Backup, PITR, `verify` and Python embedded bindings | M1 | done (PyPI publish of 0.1.0 pending: [releasing.md](../releasing.md)) |
| [8](step_8.md) | Concurrency, idempotency and read-your-writes | M2 | done |
| [9](step_9.md) | Catalog operations and namespaces | M2 | done |
| [10](step_10.md) | Query layer and the `Database` service trait | M2 | done |
| [11](step_11.md) | gRPC server | M3 | done |
| [11a](step_11a.md) | Maintenance round | M3 | done |
| [11b](step_11b.md) | Rust 1.99 and edition 2024 | M3 | done |
| [12](step_12.md) | REST/JSON API | M3 | done |
| [13](step_13.md) | Change stream, projection mode, bulk import/export | M3 | done |
| [13a](step_13a.md) | Docker image and optional server features | M3 | done |
| [14](step_14.md) | Python query methods and the remote client | M3 | done |
| [14a](step_14a.md) | Query shell | M3 | done |
| [14b](step_14b.md) | Benchmarks | M3 | done |
| [15](step_15.md) | Security | M4 | in progress (split into 15a to 15d) |
| [15a](step_15a.md) | Authentication and roles | M4 | done |
| [15b](step_15b.md) | TLS and mTLS | M4 | done |
| [15c](step_15c.md) | Audit log and `SECURITY.md` | M4 | done |
| [15d](step_15d.md) | Resource limits per client and namespace | M4 | todo |
| [16](step_16.md) | Operability | M4 | in progress (split into 16a to 16i) |
| [16a](step_16a.md) | Operator console | M4 | done |
| [16b](step_16b.md) | Configuration, health and logs | M4 | done |
| [16c](step_16c.md) | Metrics, status views, cancel and the console | M4 | done |
| [16d](step_16d.md) | Memory limit | M4 | done |
| [16e](step_16e.md) | `iwctl` against a server, archive pruning, backup throttling | M4 | done |
| [16f](step_16f.md) | Managed analytics jobs | M4 | done |
| [16g](step_16g.md) | OpenTelemetry traces | M4 | done |
| [16h](step_16h.md) | Windows | M4 | done |
| [16i](step_16i.md) | Operations guide | M4 | todo |
| [17](step_17.md) | Release 1.0 | M4 | todo |
| [18](step_18.md) | Replication and high availability | M5 | todo |

## Recurring gate: upstream check

[upstream-check.md](upstream-check.md) tracks our open issues in `ironweaver-core`: what each blocks, its workaround, and what to remove once it's fixed. Run it before steps 10, 11, 12, 13 and 17 and with every core bump (each of those steps starts with it).

## Milestone completion

| Milestone | Steps | Status |
|---|---|---|
| M0 Foundation | 1–3 | done |
| M1 Embedded durable | 4–7 | done, except the PyPI publish of 0.1.0 (prepared; the owner publishes) |
| M2 Service | 8–10 | done |
| M3 Network access | 11–14b | done |
| M4 Production 1.0 | 15–17 | todo (15a, 15b, 15c, 16a, 16b done) |
| M5 HA | 18 | todo |
