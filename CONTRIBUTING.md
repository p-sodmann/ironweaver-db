# Contributing to Ironweaver DB

Thanks for helping. Start with [AGENTS.md](AGENTS.md): it holds the design rules, code conventions and the commands a change must pass. They apply to humans and coding agents alike.

## Workflow

1. Work is split into ordered steps in [documentation/steps/](documentation/steps/README.md). Pick the current step and stay within its scope; don't pull in work from later steps.
2. Tick task checkboxes in the step file as you finish them. When a step is complete, set its `Status:` to `done` and update the step index.
3. If the plan is wrong, change the step file (and the design doc) in the same change and say why.
4. Record significant design decisions as ADRs in [documentation/adr/](documentation/adr/) (see ADR 0001 for the template).

## Before opening a PR

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

You need Rust 1.99 or later (`rustup update`); `rust-toolchain.toml` selects the current stable. Since Rust 1.97 symbols use the v0 mangling scheme: profile and debug with current tools (`samply`, `perf`, `lldb`/`gdb`), older ones show mangled names.

## Python bindings

The bindings (`crates/iwdb-python`) are built with maturin and tested with pytest, in a virtualenv (`.venv` is ignored by git):

```
uv venv .venv --python 3.12        # or: python3 -m venv .venv
uv pip install --python .venv/bin/python maturin pytest hypothesis
.venv/bin/maturin develop -m crates/iwdb-python/Cargo.toml
.venv/bin/python -m pytest crates/iwdb-python/tests
```

## Fuzzing

The cargo-fuzz targets live in [`fuzz/`](fuzz/), a separate workspace that needs nightly, so the stable build, clippy, `cargo deny` and the MSRV check don't include it. CI runs each target for a minute; run them longer locally when you change a decoder:

```
cargo install cargo-fuzz
cargo +nightly fuzz run wal_reader -- -max_total_time=600 -malloc_limit_mb=256
```

Keep commits small and prefix them with the step, e.g. `step 3: add WAL record CRC`. A PR covers one step (or a clearly separable part of one) and lists the acceptance criteria it satisfies.

## License

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).
