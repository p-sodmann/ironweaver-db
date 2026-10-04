# Step 16i: Operations guide

Status: todo
Milestone: M4 Production 1.0
Depends on: steps 16b to 16h (it documents what they built)

Split out of step 16 on 2026-10-04 (see its "Plan change").

## Goal

An operator can run the server from `documentation/operations.md` without reading the code.

## Tasks

- [ ] `documentation/operations.md`: install and configure (file and environment), probes, logs, metrics and alerts (suggested rules on the documented metrics), the status views and `iwctl`, backups, archive pruning and restore, the memory limit, jobs, traces, upgrades, Windows notes.
- [ ] Every command and config key it shows is checked by a test or taken from the code's tables.

## Acceptance criteria

- The guide covers every admin task of steps 16b to 16h, and its examples run.

## Non-goals

- Deployment recipes for specific orchestrators beyond a minimal Kubernetes probe example.
