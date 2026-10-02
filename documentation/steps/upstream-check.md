# Upstream check (recurring gate)

Status: recurring
Runs: before starting [step 10](step_10.md), [step 11](step_11.md), [step 12](step_12.md), [step 13](step_13.md) and [step 17](step_17.md), and as part of every `ironweaver-core` bump ([ADR 0002](../adr/0002-ironweaver-core-dependency.md)).

## Goal

Find out which of our upstream issues have been fixed, adopt the fixes, and remove our workarounds. Make sure that an issue a step depends on is either fixed or has a workaround that still holds.

## How to run it

1. For each open issue in the table below, check its state: `gh issue view <n> -R p-sodmann/Ironweaver --json state,closedAt`. Also check whether a fix has been merged to `main` without closing the issue.
2. If a fix is merged, bump `ironweaver-core` to a revision that contains it, following ADR 0002's bump policy, as its own commit. Then work through the issue's "When fixed" column: remove the workaround, and update or delete the test that pins the old behaviour. Such a test fails after the bump by design.
3. Check the "Needed before" column against the step you're about to start. If an issue is still open and the step depends on it, apply the listed workaround or restriction in that step, and say so in its PR.
4. Update the table (state, date checked) and the core review. Commit as `upstream check: ...`.

## Open issues

Last checked: 2026-10-02 at core `cd09ea0`: no open issues. #26–#35 were fixed in `3b15149`, #46 in `cd09ea0` (see Closed issues).

| Issue | Finding | Needed before | Until fixed | When fixed |
|---|---|---|---|---|
| – | none open | | | |

## Closed issues

#26–#35 were fixed in core `3b15149` (bumped 2026-10-02, commit `core: bump ironweaver-core a14149e -> 3b15149`) and adopted in the upstream check that followed; #46 was fixed in `cd09ea0` (bumped the same day).

| Issue | Finding | What we did |
|---|---|---|
| [#26](https://github.com/p-sodmann/Ironweaver/issues/26) | JSON loader read `-0.0` as `0.0` | `common::scalar` generates `-0.0` again; `json_keeps_the_sign_of_negative_zero` in `db_graph.rs`. The fix left a gap for NaN and infinities through `Value`'s serde: [#46](https://github.com/p-sodmann/Ironweaver/issues/46) (open) |
| [#27](https://github.com/p-sodmann/Ironweaver/issues/27) | Budgets counted nodes, not edges; cancellation per node | Nothing to remove (no traversal code yet); step 10 uses `Budget::max_edges` and may expose `expand`. `edge_budget_and_cancellation_bound_a_hub` in `core_smoke.rs` |
| [#28](https://github.com/p-sodmann/Ironweaver/issues/28) | `expect` on the apply path | Nothing to remove; the apply path is panic-free (core review). Step 11 reconsiders ADR 0008's abort for the server |
| [#29](https://github.com/p-sodmann/Ironweaver/issues/29) | `Expr` depth errors lost their message under postcard | Nothing to remove (no filter decoding yet); step 11 uses `format::take_error()`. Checked in `expr_and_pattern_round_trip` |
| [#30](https://github.com/p-sodmann/Ironweaver/issues/30) | Private `Record::at`; bincode unconditional | `DbRecord::at` calls `record::lookup`; the core is used without default features, so bincode is gone and `deny.toml` ignores nothing. We can't read ironweaver 0.1 (format 1) files; step 13's import turns `format-v1` on if it needs to |
| [#31](https://github.com/p-sodmann/Ironweaver/issues/31) | `Value` serde rejected empty containers at depth 100 | The commit pipeline (`resolve::too_deep`) and the Python conversion use the core's rule: an empty container at depth 100 is accepted. Tests in `resolve.rs`, `tests/commit.rs`, `test_values.py`; `value_serde_and_the_file_format_agree_on_depth` |
| [#32](https://github.com/p-sodmann/Ironweaver/issues/32) | `write_atomic` ignored a failed directory fsync | `LogFs::write_atomic`'s doc comment updated. **Deviation from the plan:** the checkpointer keeps its own `sync_dir` after `write_checkpoint`. The core's fsync is now checked, so it is redundant for durability, but it is the only directory sync after a checkpoint's rename that `FailFs` can fail or pause at, and the fault tests and crash points (`SyncDir … /checkpoints`) rely on it. It costs one fsync per checkpoint. `write_atomic_reports_a_failed_directory_sync` |
| [#33](https://github.com/p-sodmann/Ironweaver/issues/33) | Binary header `flags`/`reserved` not checked | `check_checkpoint_header` removed from `verify`; the loaders refuse such a file, and `verify` reports it as "can't be loaded". Recovery treats it as a damaged checkpoint (it loaded before). `binary_header_flags_and_reserved_bytes_are_checked` |
| [#34](https://github.com/p-sodmann/Ironweaver/issues/34) | No off-graph index build | `Namespace::begin_index_build` / `scan_index_keys` / `apply_built` use the core's `IndexBuild` and `install_index`; our version check is gone. Measuring it found writer starvation during the scan, now fixed (`applies_waiting`, `BUILD_CHUNK` 2048): longest commit stall about 5 ms instead of 105 ms (ADR 0019 update). `an_index_is_built_off_the_graph_and_installed`, and `an_online_index_build_sees_commits_made_during_the_scan` in `tests/commit.rs` |
| [#35](https://github.com/p-sodmann/Ironweaver/issues/35) | No per-index statistics | `Ns::index_entries` uses `index_stats` (O(1)); `IndexStatus::size` (entries, distinct keys, memory) in Rust, Python and `iwctl`. `index_stats_are_reported_per_index` |
| [#46](https://github.com/p-sodmann/Ironweaver/issues/46) | `Value`'s serde wrote NaN and infinities to JSON as `null` (gap in the #26 fix; found in the `3b15149` bump, fixed in `cd09ea0`) | Nothing to remove (no JSON export or REST yet; no restriction was implemented). The pin tests now check the fix (`value_serde_keeps_non_finite_floats_in_json`, `json_keeps_negative_zero_nan_and_infinities`), and the proptest generators include infinities |
