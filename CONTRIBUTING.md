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
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Keep commits small and prefix them with the step, e.g. `step 3: add WAL record CRC`. A PR covers one step (or a clearly separable part of one) and lists the acceptance criteria it satisfies.

## License

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).
