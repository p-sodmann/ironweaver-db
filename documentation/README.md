# Ironweaver DB documentation

Ironweaver DB is a durable, concurrent graph database built on [`ironweaver-core`](https://github.com/p-sodmann/Ironweaver). It runs embedded in a Rust or Python application, or as a server that clients reach over gRPC and REST.

## Start here

- [Concepts](concepts.md): namespaces, nodes and edges, commits, seqs and versions, idempotency keys, bounded reads, the change stream, marks and projections, jobs, backups.
- [Guarantees](guarantees.md): what is durable when, what each failure does, the isolation and limits. Every guarantee names the test that checks it.
- [Operations guide](operations.md): install, configure, secure, monitor, back up, restore and upgrade a server.

## Using it

| From | Reference |
|---|---|
| Python, embedded or remote | [Python API](python-api.md) |
| Rust, embedded | the `iwdb` crate's doc comments (`cargo doc -p iwdb --open`); its options and errors are those of the guarantees page |
| gRPC | [api/grpc.md](api/grpc.md), and the contract in [`proto/`](../proto/) |
| REST and JSON | [api/rest.md](api/rest.md), and the OpenAPI document [api/openapi.json](api/openapi.json) |
| The command line | [iwctl](iwctl.md): admin commands and the query shell |
| A browser | the [operator console](../console/README.md) |

Topics across the APIs:

- [Errors](api/errors.md): the error codes and their gRPC and HTTP statuses.
- [Configuration](api/config.md): every setting of the server and its environment variable.
- [Metrics](api/metrics.md): what `/metrics` exports.
- [Change stream](api/changes.md), [projections](api/projections.md), [import and export](api/import-export.md).

## On disk

The formats are versioned contracts: [WAL](formats/wal.md), [data directory](formats/data-dir.md), [backup](formats/backup.md), [archive](formats/archive.md), [users and grants](formats/auth.md). How versions stay compatible: [guarantees](guarantees.md#compatibility-between-versions).

## Security

[SECURITY.md](../SECURITY.md) says how to report a vulnerability. What authentication, TLS and the audit log promise is in the [guarantees](guarantees.md#authentication-and-authorisation).

## For contributors

- [Design](ironweaver-db.md): goals, guiding decisions, layers and milestones.
- [Architecture decision records](adr/): one file per decision, with its context and consequences.
- [Implementation steps](steps/README.md): the ordered plan, with what each step built and measured.
- [Benchmarks](benchmarks.md), [releasing](releasing.md), [Rust feature reference](rust_features.md) (1.85 to 1.99).
- [`ironweaver-core` review](ironweaver-core-review.md) and [upstream issues](upstream-issues.md): what the core provides and what we asked it for.
- [AGENTS.md](../AGENTS.md): the rules for working in the repository.
