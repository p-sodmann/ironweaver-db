# Upstream check (recurring gate)

Status: recurring
Runs: before starting [step 10](step_10.md), [step 11](step_11.md), [step 13](step_13.md) and [step 17](step_17.md), and as part of every `ironweaver-core` bump ([ADR 0002](../adr/0002-ironweaver-core-dependency.md)).

## Goal

Find out which of our upstream issues have been fixed, adopt the fixes, and remove our workarounds. Make sure that an issue a step depends on is either fixed or has a workaround that still holds.

## How to run it

1. For each open issue in the table below, check its state: `gh issue view <n> -R p-sodmann/Ironweaver --json state,closedAt`. Also check whether a fix has been merged to `main` without closing the issue.
2. If a fix is merged, bump `ironweaver-core` to a revision that contains it, following ADR 0002's bump policy, as its own commit. Then work through the issue's "When fixed" column: remove the workaround, and update or delete the test that pins the old behaviour. Such a test fails after the bump by design.
3. Check the "Needed before" column against the step you're about to start. If an issue is still open and the step depends on it, apply the listed workaround or restriction in that step, and say so in its PR.
4. Update the table (state, date checked) and the core review. Commit as `upstream check: ...`.

## Open issues

Last checked: 2026-09-30 (all open).

| Issue | Finding | Needed before | Until fixed | When fixed |
|---|---|---|---|---|
| [#27](https://github.com/p-sodmann/Ironweaver/issues/27) | Budgets count nodes, not edges; `bfs`/`expand` poll cancellation per node | Step 10 (bounded reads), steps 11–12 (remote) | Count edges in the `bfs` `edge_ok` closure; don't expose `expand` to remote callers | Use the core's edge budget; drop the counting closure; allow `expand` remotely; update `visit_budget_counts_nodes_not_edges` in `core_smoke.rs` |
| [#28](https://github.com/p-sodmann/Ironweaver/issues/28) | `expect` in `remove_node` / `rename_node` on the op apply path | Step 11 (server; a panic under the write lock takes down all namespaces) | A panic during commit is handled as a crash and recovered from checkpoint + WAL | Nothing to remove; note in the core review that the apply path is panic-free |
| [#29](https://github.com/p-sodmann/Ironweaver/issues/29) | `Expr` depth errors lose their message under postcard | Step 11 (gRPC error details) | Map the generic decode error to "invalid filter" | Pass the core's message through; update `expr_and_pattern_round_trip` in `core_smoke.rs` |
| [#26](https://github.com/p-sodmann/Ironweaver/issues/26) | JSON loader reads `-0.0` as `0.0` (sonic-rs) | Step 13 (JSON export), step 17 (compatibility fixtures) | Checkpoints are binary; JSON export may turn `-0.0` into `0.0` | Allow `-0.0` in `tests/common/mod.rs` (`scalar`); update `json_loses_the_sign_of_negative_zero` in `db_graph.rs` |
| [#30](https://github.com/p-sodmann/Ironweaver/issues/30) | Private `Record::at`; bincode unconditional; "bincode" doc comments | Nothing | `DbRecord::at` copy (checked by `tests/db_record.rs`); `deny.toml` ignores RUSTSEC-2025-0141 | Replace the copy with the core's lookup; drop the `deny.toml` ignore if bincode can be turned off |

## Closed issues

None yet. Move rows here with the date and the core revision that fixed them.
