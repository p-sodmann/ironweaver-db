# Step 11b: Rust 1.99 and edition 2024

Status: in progress
Milestone: M3 Network access
Depends on: step 11a

## Goal

Move from Rust 1.85 and edition 2021 to Rust 1.99 and edition 2024. Adopt a newer Rust feature only where it makes the code safer, simpler or faster, or removes a dependency. Nothing a user can observe changes: no change to the on-disk formats, the protos, the public API or the Python module. Policy and constraints: [ADR 0029](../adr/0029-rust-1.99-and-edition-2024.md).

## Inventory before the change (Rust 1.85)

- **Toolchain**: `rust-toolchain.toml` uses `channel = "stable"` (rustfmt, clippy). `rust-version = "1.85"`. Edition 2021 and `resolver = "2"` for every crate (all inherit from the workspace), and `fuzz/` (a workspace of its own, nightly) is on edition 2021 too. CI uses `dtolnay/rust-toolchain@stable`, an MSRV job on 1.85, a fuzz job on nightly, and maturin-action's manylinux container, whose rustup follows `rust-toolchain.toml`. No Dockerfiles.
- **Config**: `rustfmt.toml` (width 120, `Max` heuristics). `.cargo/config.toml` contains only `incompatible-rust-versions = "fallback"`. Lints: `unsafe_code = "forbid"`, clippy `unwrap_used`/`expect_used` warn. CI clippy runs with `-- -D warnings`. No `RUSTFLAGS`, no `CARGO_INCREMENTAL`, no custom linker.
- **Dependencies**: `cargo deny check advisories` passes. The 1.85 MSRV holds back `tonic-prost` 0.14.6 (needs 1.88) and `wasip2` 1.0.4 (needs 1.87). Of the crates std can now replace, only `fs4` is in the tree (the `LOCK` file).
- **Unsafe, FFI, layout**: no `unsafe` (forbidden), no mmap, arenas, allocators or SIMD. The only FFI is the PyO3 macros in `iwdb-python`. No `repr`, `transmute`, `bytemuck` or `zerocopy`: every format is explicit little-endian bytes, postcard or JSON, with magic, version and CRC.
- **Ordering**: no hand-written `PartialOrd`/`Ord`; every ordering is derived.
- **Locking**: `iwdb-storage/src/layout/lock.rs` locks the whole `LOCK` file with flock (via `fs4`), exclusive or shared, non-blocking, retrying for about 80 ms. Single writer: `Mutex<Wal>` and `RwLock<Namespace>` in `logged.rs`.
- **Edition hazards**: one lock guard in a scrutinee (`iwdb/src/request.rs`, `if let Some(thread) = lock(&self.thread).take()`, no `else`). No `env::set_var`, no `gen` identifiers. Two `macro_rules!` with `:expr` fragments. 109 functions return `impl Trait`.

### Baseline (Rust 1.85.1, macOS arm64, release; median of 5–7 runs, ms)

Measured by a throwaway harness outside the repository (there are no benchmarks until step 14). Three runs show the spread; the group commit is bound by fsync and varies by about ±10 %.

| Benchmark | Run 1 | Run 2 | Run 3 |
|---|---|---|---|
| insert 20k nodes + 60k edges, batches of 100, fsync off | 160.0 | 136.8 | 140.1 |
| 2000 single-node commits, group fsync | 172.9 | 188.0 | 199.8 |
| open with WAL replay (20k/60k) | 23.2 | 20.9 | 20.4 |
| open from checkpoint | 37.3 | 36.5 | 37.6 |
| checkpoint (20k/60k) | 48.0 | 50.9 | 48.1 |
| create + drop index | 1.31 | 1.36 | 1.29 |
| 100k property reads (`Store::node`) | 27.1 | 24.5 | 23.9 |
| `get_nodes`, 10k ids | 2.33 | 2.25 | 2.24 |
| full BFS traverse | 1.75 | 1.68 | 1.99 |
| match `(a:Even)-[:link]->(b:Odd)` | 8.99 | 9.04 | 9.05 |
| postcard round trip, 1000 mutations ×100 | 31.9 | 31.8 | 31.4 |
| Python: insert 10k nodes (50 transactions) | 107.5 | 82.9 | |
| Python: 10k `store.node` reads | 127.5 | 121.1 | |

The 1.85 build of the harness also wrote baseline data directories: a WAL-only store, a checkpoint with a WAL tail, and a store with namespaces, an index, idempotency keys and a dropped namespace. It also wrote a backup, and dumped the observable state of each (`iwdb_engine::testutil::state`). The cross-version check opens these with the new build.

## Tasks

- [ ] Edition 2024 (`cargo fix --edition`, then a manual review of every drop-order and capture change), as one commit, with the rustfmt style edition in a separate commit.
- [ ] `rust-version = "1.99"`, `resolver = "3"`, semver-compatible `cargo update`, and a fix for every new lint.
- [ ] Std file locking (`File::try_lock*`) instead of `fs4`.
- [ ] Features where they pay off; record the ones rejected, with the reason.
- [ ] CI: the MSRV job on 1.99.0, and clippy with `--all-features`.
- [ ] ADR 0029; ADR 0006, `formats/data-dir.md`, AGENTS.md, CONTRIBUTING.md and the CHANGELOG updated.

## Acceptance criteria

- `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo deny check`, the 1.99.0 MSRV build, `pytest` and the short crash run all pass.
- The 1.99 build opens the baseline data directories written by the 1.85 build, with identical observable state; the 1.85 build opens directories written by the 1.99 build. No change under `documentation/formats/` (apart from the lock note), `proto/` or the fixtures.
- The benchmarks show no regression beyond noise against the baseline.

## Non-goals

- Format, proto, behaviour or public-API changes. Upgrades to a new major version of a dependency (listed as follow-ups). Benchmarks in the repository (step 14).
