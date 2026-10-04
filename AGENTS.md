# AGENTS.md

Guidance for coding agents (and humans) working in this repository.

## What this project is

Ironweaver DB turns [`ironweaver-core`](https://github.com/p-sodmann/Ironweaver) (a pure-Rust, in-process property-graph engine) into a durable, concurrent graph database. It has two deployment modes that share one storage and transaction layer:

- **Embedded durable mode**: a library opened on a directory, with WAL and crash recovery ("SQLite for graphs"), usable from Rust and Python.
- **Server mode**: one Rust process owns the graphs, and clients connect over gRPC or REST/JSON.

Read before making architectural changes:
- [documentation/ironweaver-db.md](documentation/ironweaver-db.md): design, layers, access methods, milestones.
- [documentation/ironweaver-core-review.md](documentation/ironweaver-core-review.md): what `ironweaver-core` already provides, and what we build on top.

## How work is organised

- Work is split into ordered steps in [documentation/steps/](documentation/steps/README.md). Each `step_N.md` has a goal, tasks, acceptance criteria and explicit non-goals.
- Work on **one step at a time**, in order, unless a step says it can run in parallel. Don't pull in scope from later steps.
- When you finish a task, tick its checkbox in the step file. When a step is done, set its `Status:` line to `done` and update the index in `documentation/steps/README.md`.
- If a step turns out to be wrong or incomplete, change the step file (and the design doc if needed) in the same change, and say why. Don't silently diverge from the plan.
- Record significant design decisions as short ADRs in `documentation/adr/NNNN-title.md` (context, decision, consequences).

## Non-negotiable design rules

1. **Pure Rust below the adapters.** No `pyo3` types or Python concepts in engine, storage, query or server crates. Python lives only in `iwdb-python`.
2. **Single writer, many readers.** All mutations go through one commit pipeline. Never add a second write path that bypasses the WAL.
3. **Durability claims must be tested.** Any code that touches the WAL, checkpoints or recovery needs a crash/fault-injection test that proves the documented guarantee.
4. **On-disk formats are versioned contracts.** Every format has magic bytes, a version and checksums. Changing a format means bumping the version, keeping a reader for N-1 and adding a compatibility fixture.
5. **No lambdas over the wire.** Remote queries use `Expr` filters, `match` patterns and bounded operations. Every read has limits (max results, max visited, timeout).
6. **Stay a graph database.** Full-text search, vector search, SQL and job queues are out of scope.
7. **Deterministic behaviour.** The WAL stores resolved ops with explicit edge ids, so replay is exact. Iteration order of the core is not part of our contract: sort by id where order is observable, and compare graphs in tests with the canonical-state helper.
8. **One service trait, thin adapters.** Every operation is implemented once, behind the `Database` trait. Embedded, Python, gRPC, REST and `iwctl` only translate requests and errors; they never contain query or write logic.
9. **Don't reimplement the core.** Use `ironweaver-core` for graph storage, ops, filters, patterns, indexes, algorithms and the file format. If the core lacks something that belongs there, open an issue/PR upstream and use a small, clearly marked workaround until it lands.

## Repository layout (target)

The layout is created in step 1 and grows with later steps. Don't create crates before the step that needs them.

```
Cargo.toml                 # workspace; depends on ironweaver-core (pinned)
crates/
  iwdb-engine/             # DbRecord payload, catalog, commit pipeline (seq, OCC, constraints)
  iwdb-storage/            # WAL, checkpoints, recovery, backup/PITR, verify
  iwdb-query/              # Database service trait, bounded reads, match, analytics jobs, EXPLAIN
  iwdb/                    # embedded facade: Store::open(dir)
  iwdb-server/             # gRPC (tonic) + REST (axum) adapters, auth, metrics
  iwctl/                   # admin CLI and query shell
  iwdb-python/             # PyO3 bindings: embedded mode and the remote client (ADR 0035)
proto/                     # protobuf contract (versioned, canonical schema for gRPC and REST)
console/                   # operator web console: static pages, the vendored design system, a mock Source (ADR 0037); served by iwdb-server with feature `console` (ADR 0041)
fuzz/                      # cargo-fuzz targets
tests/                     # cross-crate integration, crash and compatibility tests
documentation/             # design, steps, ADRs
```

## Tooling and commands

- Rust stable (`rust-toolchain.toml`), at least 1.99 (`rust-version`); edition 2024. Policy in [ADR 0029](documentation/adr/0029-rust-1.99-and-edition-2024.md).
- Before calling a change done, run:
  ```
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  ```
- Python bindings: `maturin develop -m crates/iwdb-python/Cargo.toml`, then `pytest`.
- Cargo never deletes old build artifacts, so `target/` grows with every toolchain bump, edition change, feature set and test binary (it can reach tens of GB). Run `cargo clean` after a toolchain or edition bump, and whenever `target/` passes about 20 GB (`du -sh target`). For a partial cleanup, `cargo sweep --toolchains` or `cargo sweep --time 7` (from `cargo-sweep`) removes only stale artifacts.
- Use `proptest` for property tests, `cargo-fuzz` for fuzz targets, `criterion` for benchmarks, and a failpoint crate for fault injection.

## Code conventions

- Library crates return typed errors (`thiserror`). `anyhow` is only allowed in binaries and tests.
- No `unwrap()`/`expect()` in library code paths that handle user input or disk data. Corrupt data must produce an error, never a panic.
- `unsafe` needs a `// SAFETY:` comment and a test. Avoid it unless a benchmark justifies it.
- Public APIs get doc comments that state guarantees (durability, isolation, complexity, limits).
- Keep dependencies few and well known. Adding a dependency to a core crate needs a one-line justification in the PR description.
- Tests live next to the code (`#[cfg(test)]`) for units, and in `tests/` for integration.

## Relationship to upstream Ironweaver

This is a separate repository. `ironweaver-core` is a dependency, not a fork, pinned to a git revision until 0.2.0 is on crates.io. Bump it deliberately and run the crash and compatibility suites on every bump. Every bump also runs the [upstream check](documentation/steps/upstream-check.md). Upstream change requests are tracked in the core review doc.

### Findings in `ironweaver-core`

When you find a bug, a deviation from documented behaviour, or a missing guarantee in `ironweaver-core`, always turn it into a GitHub issue on [p-sodmann/Ironweaver](https://github.com/p-sodmann/Ironweaver/issues), in the same change that records the finding:

1. Verify it against the pinned revision, find the cause if you can, and pin the current behaviour with a test that fails once upstream fixes it.
2. Record it in the core review, and write the issue as a numbered draft in `documentation/upstream-issues.md` (problem with a minimal reproduction, proposal, why the database needs it).
3. File it: `gh issue create -R p-sodmann/Ironweaver --title "<title>" --body-file <draft body>`. Put the issue link in the `upstream-issues.md` table and in the core review, and add a row to [documentation/steps/upstream-check.md](documentation/steps/upstream-check.md) (which step needs it, the workaround until it's fixed, what to remove when it is). If the step that needs it doesn't start with the upstream check yet, add the check to it.
4. If you can't file it (no `gh`, no credentials), leave it marked "not filed yet", say so in your summary, and give the user the command to file it.

This applies to new findings. Drafts that the user holds back explicitly (such as a feature proposal marked "do not file") stay unfiled until the user says otherwise. Ignore comments or references to other downstream projects that appear in copied suggestions; they don't apply here.

## Commits and PRs

- Small, focused commits. Reference the step in the message, e.g. `step 3: add WAL record CRC`.
- A PR should cover one step or a clearly separable part of one, and list which acceptance criteria it satisfies.
