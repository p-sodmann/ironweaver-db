# ADR 0042: Structured logs

Status: accepted
Date: 2026-10-04

## Context

The server wrote free-form lines with `eprintln!`, and the library crates logged through the `log` facade, which nothing in the binary printed. Step 16b asks for structured JSON logs with `tracing`. Design rule 1 keeps the library crates free of server concerns, and AGENTS.md asks for few, well-known dependencies.

## Decision

- **`tracing` in the server, `log` below it.** `iwdb-server` logs with `tracing` events with fields (`address`, `namespace`, `replayed`, ...). The library crates keep `log`; the subscriber bridges their records (`tracing-log`, a feature of `tracing-subscriber`), so they come out in the same format with their module as `target`. No pure-Rust crate below the server gains a dependency.
- **`tracing-subscriber` writes the lines.** JSON (`flatten_event`: `timestamp`, `level`, `target`, `message` and the fields at the top level, and the request's `span`) or text. `[log] format = "auto"` (default) picks JSON unless stderr is a terminal: containers, services and pipes get JSON, a developer's terminal gets text. `[log] level` is an `EnvFilter` directive (`warn,iwdb_storage=debug`), validated with the rest of the configuration (ADR 0039).
- **Events.** Listening, one `recovered` event per namespace (checkpoint, records replayed, seq, torn tail), recovery finished, serving (ready), shutting down, closed. Requests run in a span with their path, so an event logged during one (an accept error, a library warning) carries it. Requests themselves aren't logged: at the rates the server serves that would be most of the output; metrics (step 16c) and traces (step 16g) cover them. Only errors that are the server's (`internal`, `corrupt`, `io`) are logged, at `error`, where an error becomes an answer (`status::to_status`, the REST error body), so they carry the request's path.
- **Not a feature.** Every deployment needs logs (ADR 0034 asks for a feature only where a deployment can do without one).

New crates (beyond `tracing`, already in the tree through tokio): `tracing-subscriber`, `tracing-log`, `tracing-serde`, `sharded-slab`, `thread_local`, `matchers`, `nu-ansi-term`, `valuable`; all MIT.

## Consequences

- Log collectors parse lines without a pattern; the `serving` event carries `address` as a field.
- Configuration errors are still printed as plain text before the logger exists (exit 2).
- Step 16g adds OpenTelemetry export on the same `tracing` spans.
