# Step 11b: Rust 1.99 and edition 2024

Status: done
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

- [x] Edition 2024 (`cargo fix --edition`, then a manual review of every drop-order and capture change), as one commit, with the rustfmt style edition in a separate commit.
- [x] `rust-version = "1.99"`, `resolver = "3"`, semver-compatible `cargo update`, and a fix for every new lint.
- [x] Bump `ironweaver-core` to `ca308f0` (upstream PR #56: edition 2024, Rust 1.99, `rand` 0.9), with the upstream check. *Added during the step: the core moved to Rust 1.99 at the same time.*
- [x] Std file locking (`File::try_lock*`) instead of `fs4`.
- [x] Features where they pay off; record the ones rejected, with the reason.
- [x] CI: the MSRV job on 1.99.0, `CARGO_BUILD_WARNINGS=deny`, and clippy, tests and the MSRV check with `--all-features`.
- [x] ADR 0029; ADR 0006, `formats/data-dir.md`, AGENTS.md, CONTRIBUTING.md, the core review and the CHANGELOG updated.

## Acceptance criteria

- `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo deny check`, the 1.99.0 MSRV build, `pytest` and the short crash run all pass.
- The 1.99 build opens the baseline data directories written by the 1.85 build, with identical observable state; the 1.85 build opens directories written by the 1.99 build. No change under `documentation/formats/` (apart from the lock note), `proto/` or the fixtures.
- The benchmarks show no regression beyond noise against the baseline.

## Non-goals

- Format, proto, behaviour or public-API changes. Upgrades to a new major version of a dependency (listed as follow-ups). Benchmarks in the repository (step 14).

## Result

Toolchain: Rust 1.85.1 / edition 2021 → **Rust 1.99.0 / edition 2024**, `rust-version = "1.99"`, `channel = "stable"` (floating; ADR 0029). `ironweaver-core` `d15a7ec` → `ca308f0`.

### Edition migration: drop order and lock guards

`cargo fix --edition`, then the 2024-compatibility lints run by hand over the workspace (15 hits):

- **`if let` rescope** (5: `iwdb-python/src/convert.rs`, `transaction.rs`, `iwdb-storage/src/verify.rs` ×3): the scrutinee temporaries are PyO3 cast/extract errors and `io::Error`; dropping them before the `else` has no effect. The rewrites into nested `match` that `cargo fix` made were reverted.
- **Tail-expression drop order** (9): `iwdb-query/src/exec.rs` (timer loop and `Slot::poll`: the temporary is an `Option` moved out or empty; the guards are named locals), `iwdb-server/src/client.rs`, `serve.rs`, `iwdb-storage/src/archive.rs`, `verify.rs` ×2, `iwdb/src/embedded.rs` (plain values), `iwdb/tests/data_dir.rs` (a `Store` is now dropped before its `TempDir`: better). **No WAL, checkpoint or namespace lock guard is involved.**
- The one lock guard in a scrutinee, `iwdb/src/request.rs` `if let Some(thread) = lock(&self.thread).take()`, has no `else`: the join happens under the mutex in both editions, and the timer thread never takes that mutex.
- Two tests in `iwdb/tests/namespaces.rs` needed a `let` binding only to end a `MutexGuard` temporary before the locals; edition 2024 makes it unnecessary.
- `expr_2021` fragments reverted to `expr` (no caller passes `const {}` or `_`); `Outcome::words` keeps a precise `use<'_>`.
- No `env::set_var`, `unsafe`, `extern` blocks or `gen` identifiers.

### Upgrade gotchas (rust_features.md §16)

- **Enum layout on disk (1.97):** not applicable. No `repr`, `transmute`, `bytemuck` or `zerocopy`; every format is explicitly encoded (LE bytes, postcard, JSON) with magic, version and CRC.
- **`Ord` consistency (1.96, 1.98):** no hand-written `PartialOrd`/`Ord` in our crates or in the core's index keys (all derived).
- **`Copy` hot paths (1.93):** no regression attributable to it; see the benchmarks.
- 1.99's legacy integer constants, `no_mangle` generics and doctest attributes: none in the tree.

### Dependencies

- Removed: **`fs4`** → `std::fs::File::try_lock` / `try_lock_shared` (same `flock` / `LockFileEx` calls). Through the core bump: `rand` 0.8, `rand_chacha` 0.3, `rand_core` 0.6, `getrandom` 0.2.
- Updated (semver-compatible, held back by the 1.85 MSRV before): `tonic`, `tonic-build`, `tonic-prost`, `tonic-prost-build` 0.14.5 → 0.14.6; also `tokio` 1.53.2, `uuid` 1.27, `libc`, `cc`.
- Every direct dependency is on its latest release; `cargo deny check` passes (only the existing duplicate warnings: `syn` 2/3, `getrandom` 0.3/0.4, through upstream crates).

### Features adopted

| Feature | Where | Replaces | Benefit |
|---|---|---|---|
| `File::try_lock*` (1.89) | `iwdb-storage/src/layout/lock.rs` | `fs4::FileExt` | one dependency fewer, same semantics |
| let chains (1.88, 2024) | 29 sites in engine, storage, query, server, store (clippy `collapsible_if` at the new MSRV) | nested `if let` / `if` pairs | flatter code; same short-circuit order; at most three clauses |
| `is_multiple_of` (1.87) | `iwdb-server/src/convert.rs` `millis`, two tests | `x % n == 0` | states intent |
| `array_windows` (1.94) | checkpointer segment removal, writer's older-segment sync, `verify` coverage, manifest id check | `windows(2)` + `pair[0]` / `pair[1]` | destructured, no indexing |
| `assert_matches!` (1.96) | 151 test assertions | `assert!(matches!(..))` | the failure shows the value; 10 kept (value not `Debug`, or a compound condition) |
| `build.warnings` (1.97) | CI (`CARGO_BUILD_WARNINGS=deny`) | clippy's `-D warnings` only | warnings fail `cargo test` builds too; cache unaffected |
| resolver 3 | workspace | `.cargo/config.toml` fallback | file removed |

### Considered and rejected

- `strict_*` (1.91): disk and network values already use `checked_*`/`saturating_*` with errors or bounded `get`; there are no in-memory offsets where a panic would be better.
- `as_array` / `split_off*` / `as_chunks` for decoders: the header readers check lengths first and read fixed offsets; no gain.
- `cast_signed` for the WAL's `u64 as i64` time: identical behaviour, `as` is clear in context.
- `bool: TryFrom<int>`, bit operations, `NonZero::div_ceil`: no bool bytes, bitmaps or page counts on disk.
- `RwLockWriteGuard::downgrade` (1.92): the index build and the commit apply release the write lock on purpose before reading (ADR 0014/0019); downgrading would hold readers' exclusion longer for nothing.
- `Atomic*::update` (1.95): the only read-modify-write is `fetch_max`, already the right primitive.
- `get_disjoint_mut`, `extract_if`, `push_mut`, `Vec::pop_if`: no such patterns (graph storage is the core's).
- `core::range` types, `NumBuffer`, `fmt::from_fn`, `substr_range`, `next_if_map`: no range structs; integer formatting isn't on a hot path; the pattern parser is the core's.
- §8 / §9 (unsafe, SIMD, `cold_path`, `select_unpredictable`), §10 (algebraic floats), §12 (upcasting): no unsafe, SIMD or float scoring code, no trait bridges; no benchmark evidence.
- `dead_code_pub_in_binary` (1.97): no findings in `iwctl`, `iwdb-server` or `iwdb-crash`; their modules are private, so `dead_code` already covers them.
- `raw_borrows_via_references` (1.99): there is no unsafe code.

### Unsafe code

None: the workspace keeps `unsafe_code = "forbid"`. Miri (nightly) runs the WAL frame codec tests (`iwdb-storage` `format::`, 6 passed) and the engine's unit tests clean.

### Verification

- `cargo fmt --check`, `CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --all-features`, `cargo test --workspace --all-features` (427 passed, 7 ignored: the same as before), `cargo deny check`: pass after every group.
- Crash harness, 150 cycles × 3 fsync policies: 450 passed after the edition change, after the core bump, and after the lock change.
- Fuzz `wal_reader` (nightly, 60 s): 2.35 million inputs, no crash.
- **Cross-version:** the 1.99 build opens the data directories written by the 1.85 build (WAL-only, checkpoint + tail, namespaces with index, keys and a dropped namespace, and a backup restored) with byte-identical observable state; the 1.85 build opens directories written by the 1.99 build identically; and the same workload gives the same state on both. `writing_the_records_gives_the_current_fixture_bytes` shows the WAL bytes are unchanged; the other files differ only in commit times, the random history id and their CRCs.
- **Locking across builds and processes:** a 1.85 store (fs4) and a 1.99 store (std) exclude each other in both directions; `kill -9` releases the lock (also `the_lock_is_released_when_the_process_dies`).
- **Python:** the 1.99 wheel passes `pytest` (83); the 1.85 and 1.99 wheels read both sets of directories identically.

### Benchmarks (final, interleaved 5 + 5 runs, median (min–max), ms)

The 1.85 build of the old sources against the 1.99 build of this step, alternating so that machine load affects both alike (same harness as the baseline above).

| Benchmark | Rust 1.85, before | Rust 1.99, after | Change |
|---|---|---|---|
| checkpoint (20k/60k) | 39.07 (38.04–47.87) | 38.91 (37.97–38.98) | −0.4 % |
| 2000 single-node commits, group fsync | 191.29 (172.04–195.29) | 190.77 (187.81–192.17) | −0.3 % |
| `get_nodes`, 10k ids | 2.32 (2.23–2.68) | 2.30 (2.25–2.33) | −0.8 % |
| create + drop index | 1.32 (1.21–1.39) | 1.35 (1.28–1.38) | +2.5 % |
| insert 20k nodes + 60k edges, fsync off | 120.17 (119.13–126.27) | 119.67 (117.05–144.88) | −0.4 % |
| match `(a:Even)-[:link]->(b:Odd)` | 9.42 (9.33–11.37) | 9.28 (9.15–9.54) | −1.6 % |
| open from checkpoint | 38.84 (38.51–45.96) | 40.27 (39.27–40.31) | +3.7 % |
| open with WAL replay | 22.68 (21.42–24.39) | 22.26 (21.98–22.78) | −1.8 % |
| postcard round trip, 1000 mutations ×100 | 33.16 (32.31–33.52) | 31.67 (31.31–32.33) | −4.5 % |
| 100k property reads | 25.83 (24.81–26.11) | 24.88 (24.24–25.28) | −3.7 % |
| full BFS traverse | 1.86 (1.76–3.78) | 1.77 (1.71–1.89) | −4.8 % |
| Python: insert 10k nodes | 80.5 | 79.0 | −1.9 % |
| Python: 10k `store.node` reads | 117.5 | 107.5 | −8.5 % |

No regression beyond noise. Open from checkpoint is the one benchmark consistently slower (+1.4 ms). The old sources built with 1.99 give +1.4 % there, so the compiler accounts for part of it; the load path is the core's checkpoint loader, which this step doesn't change, and the 1.85 baseline itself moved by 3.5 % between sessions. Nothing to revert; step 14's benchmarks should keep an eye on it. The 1.93 `Copy`-specialization change shows no measurable effect.
