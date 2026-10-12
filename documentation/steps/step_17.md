# Step 17: Release 1.0

Status: in progress (split into 17a to 17f; 17b and 17c dropped)
Milestone: M4 Production 1.0
Depends on: steps 14, 15, 16 (15d was the last of them to finish; only 17f, the release itself, waited for it)

## Goal

Ship 1.0 with documented guarantees and a full verification suite.

## Tasks

- [x] First: run the [upstream check](upstream-check.md). Issues this step depends on: all open ones (the release notes list any that remain, with their workarounds). (17a; none is open)
- [ ] Compatibility fixtures: data directories (WAL + checkpoints) written by every released version open in the new one. (17a builds the fixture and its test; 17f writes the first one, for 1.0)
- ~~Continuous fuzzing: WAL reader, checkpoint loader, proto and JSON decoding, pattern and filter inputs. (17b)~~ Dropped on 2026-10-12 (see Plan change); CI keeps its short run of the WAL reader target.
- ~~24h soak test with mixed load; runbook for running it before each minor release. (17c)~~ Dropped on 2026-10-12 (see Plan change).
- [ ] Docs: concepts, guarantees (`documentation/guarantees.md`: durability per fsync policy, isolation, limits, failure behaviour), operations, gRPC and REST reference. (17d)
- [ ] Release pipeline: semver, a GitHub Release per tag with the Python wheels (embedded and client), the sdist, the server and `iwctl` binaries, and the multi-arch Docker image as files, CHANGELOG. No crates.io, PyPI or registry push (see Plan change). (17e)

## Plan change

2026-10-11: step 17 is too big for one PR, so it was split into ordered sub-steps, as steps 15 and 16 were. Step 17 is done when they are. The tasks above stay as the overview; each is ticked when its sub-step is done.

2026-10-11: **releases are published on GitHub only.** The owner decided not to publish to any outside platform for now: no crates.io, no PyPI, no Docker registry, no Helm repository. A release is a tag and a GitHub Release with its files. This also settles two things the old plan left open: the crates can't go to crates.io while `ironweaver-core` is a git dependency (ADR 0002), and OSS-Fuzz would hand our fuzzing to an outside service, so fuzzing runs nightly in our own CI instead. The optional Helm chart is dropped with the registry. Step 7's PyPI publish of 0.1.0 is withdrawn for the same reason (its task now says so).

2026-10-12: **17b (continuous fuzzing) and 17c (soak test) are dropped.** The owner decided 1.0 doesn't need them. What stays: the PR CI's 60 s run of the WAL reader's fuzz target (`fuzz/`; it found upstream [#101](https://github.com/p-sodmann/Ironweaver/issues/101)), and the crash harness, short in CI and long nightly. The acceptance criteria no longer name soak and fuzz suites, and the guarantees page makes no claim about behaviour over long runs. 17d now follows 17a directly.

| Sub-step | Covers | Why here |
|---|---|---|
| [17a](step_17a.md) | the upstream check; release compatibility fixtures | Gates the rest: the fixture is what every later release is checked against, and its generator must exist before 1.0 is tagged. |
| [17b](step_17b.md) | ~~continuous fuzzing~~ | Dropped on 2026-10-12. |
| [17c](step_17c.md) | ~~24 h soak test and its runbook~~ | Dropped on 2026-10-12. |
| [17d](step_17d.md) | documentation: concepts, guarantees, operations, API reference | Documents what the suites prove, so it comes after 17a. |
| [17e](step_17e.md) | release pipeline on GitHub Releases | Needs 17a's fixture step and 17d's docs in the release notes. |
| [17f](step_17f.md) | 1.0 | Needs 17a, 17d, 17e and step 15d. The owner tags. |

## Acceptance criteria

- Crash and compatibility suites green (soak and fuzz suites dropped on 2026-10-12, see Plan change).
- 1.0 released. M4 is done: update [README.md](README.md).
