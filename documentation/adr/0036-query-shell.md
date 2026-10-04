# ADR 0036: The query shell

Status: accepted
Date: 2026-10-04

## Context

Step 14a adds `iwctl shell`, an interactive client of a server like `psql` or `redis-cli`: `match` patterns, lookups, admin commands, tables or JSON. Points to decide: how it talks to the server, how filters and data are typed in, line editing, and what makes it scriptable.

## Decision

- **Over the `Database` trait.** The shell is a thin adapter (design rule 8): each command is one trait call on `iwdb_server::client::Remote` (ADR 0024), and `\next` repeats the last paginated call with its cursor. `iwctl` depends on `iwdb-server` with only the `client` feature.
- **The core's syntaxes, no query language of our own.** Patterns are the core's text (`Pattern::parse`). Filters are the core's JSON form (`Expr::from_json_str`), the same as REST's, which keeps the depth limit and the error messages. Attributes in `upsert-node` and `add-edge` are plain JSON: integers are `Int`, other numbers `Float`. That is enough to put data in and look at it; bulk loads go through imports.
- **Lines from stdin.** The shell reads one command per line from stdin, whether it is a terminal or a pipe, so a script is a file of commands (`iwctl shell … < script`). The prompt goes to stderr and only to a terminal; answers go to stdout, errors to stderr (in JSON mode, to stdout as `{"error": …}` lines, so a consumer reads one stream). An error doesn't end the session; the exit code says whether every command succeeded.
- **No line-editing crate.** Readline crates (`rustyline`, `reedline`) bring a dozen dependencies and terminal handling for history and editing, which `rlwrap iwctl shell …` already gives. AGENTS.md asks for few dependencies, so we add none; a later step can add one if the shell grows completion.
- **Output.** Tables are aligned per answer and end with a row count, `(truncated …)` and `(more: \next)`. `\json` prints one object per answer with the answer's `seq`, `cursor`, `truncated` and `work`, the fields of the Python answers (ADR 0035).

## Consequences

- No new syntax to specify or keep in step with the core: a new `Expr` variant works in the shell as soon as the core reads it.
- Filters are verbose to type. If that matters, a short form belongs upstream (a text form of `Expr`), not in the shell.
- No history or editing without `rlwrap`.
- Only one page of a read is kept: `\next` after another `find` or `match` continues that one.
