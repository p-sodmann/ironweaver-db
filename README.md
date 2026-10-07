# Ironweaver DB

A durable, concurrent graph database built around [`ironweaver-core`](https://github.com/p-sodmann/Ironweaver), the pure-Rust engine of the Ironweaver graph library.

Ironweaver DB adds what an in-process graph library lacks for production use: a write-ahead log and crash recovery, atomic transactions with optimistic concurrency, snapshot reads, a catalog, a bounded query API and network access over gRPC and REST/JSON. It runs either embedded (a library opened on a directory) or as a server that many clients share.

**Status:** early development. Milestone M1 (embedded durable) is done: a store in a directory with a write-ahead log, checkpoints and crash recovery (tested by a kill -9 harness), online backups, WAL archiving, point-in-time restore, `verify`, the `iwctl` admin CLI and Python bindings (`pip install ironweaver-db` once 0.1.0 is published). See the [implementation steps](documentation/steps/README.md).

```python
import iwdb

with iwdb.Store.open("data") as store:
    with store.transaction() as tx:
        tx.upsert_node("alice", labels=["Person"], attr={"name": "Alice"})
    print(store.node("alice"))
```

The server in Docker (gRPC and REST on port 7600 over TLS, the console at `https://127.0.0.1:7600/console/`). Authentication is on: the first start makes the user `admin` with the password you give. The image has no certificate; for development, `docker/dev-cert.sh` makes one (clients trust `docker/tls/ca.pem`):

```
sh docker/dev-cert.sh
IWDB_ADMIN_PASSWORD=... docker compose up --build
```

`docker build --build-arg FEATURES="" .` builds a gRPC-only image without REST or the Postgres projection source ([details](documentation/api/grpc.md#features-and-docker)).

## By the numbers

Lines of code, not counting blank lines or vendored code (as of October 2026):

| Language | Files | Lines |
|---|---:|---:|
| Rust | 220 | 68,917 |
| Python | 19 | 2,419 |
| JavaScript | 9 | 2,188 |

About 22,000 of the Rust lines are integration tests in `tests/` directories, and the inline `#[cfg(test)]` modules add more.

## Documentation

- [Design proposal](documentation/ironweaver-db.md): goals, guiding decisions, workstreams and milestones
- [ironweaver-core review](documentation/ironweaver-core-review.md): what the engine provides and which upstream changes would help
- [Upstream issue drafts](documentation/upstream-issues.md): the proposed changes to `ironweaver-core`
- [Guarantees](documentation/guarantees.md): what is durable when, and what each failure does
- [On-disk formats](documentation/formats/): the WAL, the data directory, backups and archives
- [Python API](documentation/python-api.md), [iwctl](documentation/iwctl.md), [releasing](documentation/releasing.md)
- [Operator console](console/README.md): explore a graph and see the server's status in a browser (on mock data for now)
- [Architecture decision records](documentation/adr/)
- [Implementation steps](documentation/steps/README.md): the ordered task list
- [AGENTS.md](AGENTS.md): working rules for contributors and coding agents
- [SECURITY.md](SECURITY.md): how to report a vulnerability; no telemetry

## License

Ironweaver DB is dual-licensed:

- [GNU AGPL-3.0](LICENSE) (`AGPL-3.0-only`), free of charge, or
- a [commercial license](COMMERCIAL-LICENSE.md) for using it in proprietary products or services without the AGPL's obligations.

See [ADR 0038](documentation/adr/0038-agpl-and-commercial-license.md) for the reasons.
