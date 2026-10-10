# Step 16i: Operations guide

Status: done
Milestone: M4 Production 1.0
Depends on: steps 16b to 16h (it documents what they built)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

An operator can run the server from `documentation/operations.md` without reading the code.

## Tasks

- [x] `documentation/operations.md`: install and configure (file and environment), probes, logs, metrics and alerts (suggested rules on the documented metrics), the status views and `iwctl`, backups, archive pruning and restore, the memory limit, jobs, traces, upgrades, Windows notes.
- [x] Every command and config key it shows is checked by a test or taken from the code's tables.

## Acceptance criteria

- The guide covers every admin task of steps 16b to 16h, and its examples run.

## Non-goals

- Deployment recipes for specific orchestrators beyond a minimal Kubernetes probe example.

## Outcome

Done on 2026-10-10, after the upstream check at core `c385ec7` (nothing to adopt).

- [operations.md](../operations.md) covers every admin task of steps 16b to 16h as a short procedure, and links to config.md, metrics.md, errors.md, iwctl.md and guarantees.md for the full tables. Its only orchestrator recipe is a minimal Kubernetes probe example.
- **Checked examples.** The guide's header names the convention:
  - `crates/iwctl/src/guide.rs` parses every `iwctl` command line of its code blocks with `args::parse`;
  - `crates/iwdb-server/tests/operations.rs` checks that every setting and `IWDB_*` variable is in `config::KEYS`, that every TOML block passes `Config::parse` (a fragment gets `data_dir` and a certificate), and that every metric, the alert rules' included, is exported with the labels it selects on;
  - on Linux and macOS, the same test runs the blocks fenced as `sh run`, in order, against the `iwdb-server` binary and `iwctl`: first start with the bootstrap admin, tokens and users, probes, the log tail and audit files, a monitoring user and `/metrics`, the status views, catalog commands, backup, verify, pruning, an offline restore, a start on the restored directory, and a managed job. `sh run=server` starts the server it shows, `sh run=stopped` stops it with SIGTERM and checks a clean exit. Docker, Kubernetes and Windows examples aren't run (CI's docker job runs the image).
- **Alert rules** are checked by metric names and labels, not parsed as PromQL: the parser there is (`promql-parser`) brings a parser generator stack (cfgrammar, lrpar, lrlex, bincode), too much for a dev-dependency of this check. No new dependency.
- **Gap fixed here:** `iwctl status` printed a namespace's read-only cause but not its failed checkpoint (only `--json` had it); the text output now has a `checkpoint failed: <cause>` line (`a_namespace_shows_its_read_only_cause_and_failed_checkpoint`).
- **No metric was missing** for the alert rules: archive failures surface as a failed checkpoint (`iwdb_checkpoint_failed`).

## Follow-ups

- **`iwctl` has no command for three of 16c's status reads**: the change-stream readers and their lag (`ListConsumers`, `GET /v1/consumers`), the log tail (`GetLog`, `GET /v1/log`) and the metrics (`GetMetrics`, `/metrics`). The guide reads them over REST or in the console. Each would be one trait call and its printing (`iwctl consumers`, `iwctl log`); recorded rather than built, as new commands are new features. Starting a managed job from `iwctl` stays out by ADR 0056's decision.
- **A slow shutdown on Windows with traces on (found in this step's CI, not caused by it).** `a_collector_that_is_down_slows_and_fails_no_request` (`binary.rs`, step 16g) failed once on windows-latest (run 38068262637, the core bump of PR #24): with `drain_timeout_secs = 3` and the collector down, the server took 17.9 s to exit (the bound is 15 s); the same commit passed in its other run. The trace flush itself is bounded (`Tracing::shutdown` detaches a running export after its timeout), so the time goes elsewhere in the process's end; on Windows a connection to a closed port takes about 2 s to fail. Until it is found, the guide's promise that the shutdown's export stays within the drain holds for the flush, not necessarily for the process's exit on Windows.
