# Ironweaver DB

A durable, concurrent graph database built around [`ironweaver-core`](https://github.com/p-sodmann/Ironweaver), the pure-Rust engine of the Ironweaver graph library.

Ironweaver DB adds what an in-process graph library lacks for production use: a write-ahead log and crash recovery, atomic transactions with optimistic concurrency, snapshot reads, a catalog, a bounded query API and network access over gRPC and REST/JSON. It runs either embedded (a library opened on a directory) or as a server that many clients share.

**Status:** early development. Step 1 (workspace, CI and core smoke tests) is done; see the [implementation steps](documentation/steps/README.md).

## Documentation

- [Design proposal](documentation/ironweaver-db.md): goals, guiding decisions, workstreams and milestones
- [ironweaver-core review](documentation/ironweaver-core-review.md): what the engine provides and which upstream changes would help
- [Upstream issue drafts](documentation/upstream-issues.md): the proposed changes to `ironweaver-core`
- [Architecture decision records](documentation/adr/)
- [Implementation steps](documentation/steps/README.md): the ordered task list
- [AGENTS.md](AGENTS.md): working rules for contributors and coding agents

## License

[MIT](LICENSE)
