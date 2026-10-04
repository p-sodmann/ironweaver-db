# ADR 0039: Configuration from a file and the environment

Status: accepted
Date: 2026-10-04

## Context

Until step 16b, `iwdb-server` read one TOML file, required on the command line, and stopped at its first problem. In a container (ADR 0034) a setting meant mounting a file, and a typo in a mounted file meant one restart per mistake. Step 16b asks for environment overrides and validation with clear errors before the store opens. Points to decide: how variables are named, which settings they cover, what happens to a variable that names nothing, how errors are reported, and whether the file stays required.

## Decision

- **One list of settings.** `iwdb_server::config::KEYS` lists every scalar setting with its path in the file (`store.fsync`) and its variable (`IWDB_STORE_FSYNC`: `IWDB_` and the path in upper case, `.` as `_`). It is built by a macro from the `Config` fields, so the setter and getter of each setting are the field itself. The docs (`documentation/api/config.md`) are tested against it: same keys, same variables, same defaults. `--check-config` prints from it.
- **A variable wins over the file.** The file is read first (serde, with line numbers in its errors), then each `IWDB_*` variable that names a setting overrides its field. Values are read as the field's type: numbers, `true`/`false`/`1`/`0`, the enums by name. `[[projection]]`s stay file-only: they are lists of tables, and their secrets already come from a variable of their own (`url_env`).
- **The file is optional.** Only `data_dir` is required, from either source; `iwdb-server` with `IWDB_DATA_DIR` runs without a file. A relative `data_dir` from the file is relative to the file's directory, from the variable to the working directory.
- **Typos are errors, other names are not.** A variable under a section prefix (`IWDB_STORE_`, `IWDB_SERVER_`, `IWDB_LIMITS_`, `IWDB_LOG_`, `IWDB_CONSOLE_`) that names no setting is an error. Other `IWDB_*` variables are ignored: our own test harnesses use `IWDB_SERVER`, `IWDB_URL` and others, and refusing every unknown `IWDB_*` would break them and any wrapper script.
- **Every problem at once.** Validation collects problems instead of returning the first: each variable's parse error, unknown variables, the cross-field checks (limits, group commit, message size, log filter, console, projections). Each problem says where its value came from (`[server] max_message_bytes (from IWDB_SERVER_MAX_MESSAGE_BYTES) must be at least 1024`). A TOML syntax or type error in the file stops the file's other checks (serde stops there), but the variables are still checked. Exit code 2, before the store opens.
- **`--check-config`.** Validates, then prints the effective settings as TOML with each value's source (`default`, `file`, or the variable) as a comment. The output reads back as a config file with the same settings (tested).

Rejected: a layered config crate (`figment`, `config`): a dependency for what is about 150 lines here, and their merged-tree approach loses the file's line numbers in errors. Reading variables into the TOML tree before deserializing has the same problem.

## Consequences

- The Docker image can be configured with `-e IWDB_...` only; `compose.yaml` and the image's config file still work as before.
- Adding a setting means adding a field and a line to `keys!`; the docs test fails until `config.md` has its row.
- `ConfigError::Invalid` carries a list of problems instead of one message (a breaking change of the library's error type; only the binary and tests use it).
