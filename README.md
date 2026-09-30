# Ironweaver DB

A durable, concurrent graph database built around [`ironweaver-core`](https://github.com/p-sodmann/Ironweaver), the pure-Rust engine of the Ironweaver graph library.

Ironweaver DB adds what an in-process graph library lacks for production use: a write-ahead log and crash recovery, atomic transactions with optimistic concurrency, snapshot reads, a catalog, a bounded query API and network access over gRPC and REST/JSON. It runs either embedded (a library opened on a directory) or as a server that many clients share.

**Status:** design phase. No code yet.

## Documentation

- [Design proposal](documentation/ironweaver-db.md): goals, guiding decisions, workstreams and milestones
- [ironweaver-core review](documentation/ironweaver-core-review.md): what the engine provides and which upstream changes would help
- [Implementation steps](documentation/steps/README.md): the ordered task list
- [AGENTS.md](AGENTS.md): working rules for contributors and coding agents

## License

MIT (to be added in step 1).
