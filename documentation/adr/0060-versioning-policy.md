# ADR 0060: Versioning policy

Status: accepted
Date: 2026-10-12

## Context

Step 17e builds the release pipeline, and 1.0 (step 17f) is the first release with a promise. A version number has to say what an upgrade can break, for each thing a user depends on:

- the Rust API (the crates are a git dependency, not on crates.io: step 17's Plan change, ADR 0002) and the Python package `ironweaver-db`, which holds both the embedded store and the remote client (`iwdb.connect`, ADR 0035);
- the on-disk formats, which design rule 4 already versions (magic bytes, a version, checksums; N-1 is read), with one fixture per format version and, since step 17a, one release fixture per release (`crates/iwdb/tests/fixtures/releases/<version>/`);
- the wire contract: the gRPC protos (`proto/ironweaver_db/v1`, guarded by `buf breaking` in CI, [grpc.md](../api/grpc.md#versioning)) and the REST API on the same messages (`/v1/...`, ADR 0030);
- what operators script and alert on: configuration keys and their `IWDB_*` variables (ADR 0039), error codes ([errors.md](../api/errors.md)), metric names and labels ([metrics.md](../api/metrics.md)), `iwctl`'s commands, `--json` output and exit codes, and the image's paths and port.

Releases are a git tag and a GitHub Release with files (step 17's Plan change), so the version must also map onto the wheel's version, which Python spells per PEP 440.

## Decision

**Ironweaver DB follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html), with one version for everything**: the workspace version (`[workspace.package]` in `Cargo.toml`) is the version of every crate, the binaries (`--version`), the wheel, the image and the tag `v<version>`. Nothing is versioned separately.

- **Major** (`2.0.0`): a breaking change, as defined below.
- **Minor** (`1.1.0`): something new that breaks nothing; a new on-disk format version, read alongside the old ones (an upgrade is one way, see below); a raised minimum Rust version (ADR 0029) or a dropped Python version past its end of life, named in the CHANGELOG.
- **Patch** (`1.0.1`): fixes only. **A patch release changes no on-disk format version**, so going back to an older patch of the same minor works.

### What 1.x promises

Within 1.x, upgrading to a later 1.x is a binary or image swap ([operations.md](../operations.md#upgrades)), and:

| Surface | Promise within 1.x | Breaking (needs 2.0) |
|---|---|---|
| **Rust API**: the `iwdb` crate (the embedded facade and what it re-exports) and `iwdb_server::client` (the Rust clients) | Public items, their signatures and their documented guarantees stay. New items, new variants of `#[non_exhaustive]` enums, new optional fields with a default are minor. | Removing or renaming a public item, changing a signature, adding a variant to an exhaustive enum, weakening a documented guarantee. The other crates (`iwdb-engine`, `iwdb-storage`, `iwdb-query`, `iwdb-server` but its client, `iwctl`, `iwdb-crash`) are internal: their Rust APIs may change in any release. |
| **Python API**: the names `iwdb` exports (no leading underscore), their arguments, return types and exceptions, as [python-api.md](../python-api.md) documents them | They stay; new functions, methods, keyword arguments with defaults and exception subclasses are minor. | Removing or renaming a name or an argument, changing what a call returns or raises, moving an exception out of its parent class. |
| **On-disk formats**: the data directory, WAL, checkpoints, backups, the WAL archive, the auth store | **Every 1.x opens what every earlier release since 1.0 wrote**: each release's fixture stays in the repository and every later version must open, verify and restore it ([guarantees.md](../guarantees.md#compatibility-between-versions)). Each format version also has its own fixture, and a reader for N-1 at least (design rule 4). An older layout is upgraded on open; after that, older versions refuse it. | Dropping the reader for a format some 1.x wrote, or removing a release fixture. A 2.0 opens what the last 1.x wrote, and may drop older readers; its release notes say from which version to upgrade first. |
| **Wire contract**: `proto/ironweaver_db/v1` and the REST routes under `/v1/` with their proto3 JSON | Only compatible changes: new fields, messages, RPCs, routes, enum values and `oneof` members. `buf breaking` checks the protos on every push; `wire.rs` pins the bytes inside `Value`, `Expr` and `Pattern`; the OpenAPI document is generated from the server. A `v2` package or route prefix may be added in a minor release, next to `v1`. | Anything `buf breaking` (the `FILE` rules) rejects, a changed meaning of a field, a removed route, removing `v1`. |
| **Error codes** ([errors.md](../api/errors.md)) | A code keeps its meaning and its gRPC status and HTTP status. New codes are minor. Messages are not part of the contract. | Renaming or removing a code, or changing its status mapping. |
| **Configuration**: keys, their `IWDB_*` variables, the CLI flags of `iwdb-server` | A config that a 1.x release accepted starts every later 1.x with the same meaning. New keys are minor. A renamed key keeps the old name as an alias until 2.0. | Removing a key or an alias, or changing a default in a way that weakens durability or security (`store.fsync`, TLS, authentication). Other defaults may change in a minor release, named in the CHANGELOG. |
| **Metrics** ([metrics.md](../api/metrics.md)) | Names, types and label names stay. New metrics, new labels on a new metric and new values of a bounded label (a new error `code`, a new `operation`) are minor. | Renaming or removing a metric or a label, changing a metric's type or unit. |
| **`iwctl`** ([iwctl.md](../iwctl.md)) | Commands, options, exit codes and the fields of `--json` output stay; new ones are minor. The text output is for people and may change. | Removing or renaming a command, an option, an exit code or a JSON field. |
| **The image** (ADR 0034) | `/var/lib/iwdb` as the volume, `/etc/iwdb/iwdb.toml`, `/etc/iwdb/tls/`, port 7600, uid 10001, the entrypoint `iwdb-server`, `iwctl` on the path. | Changing any of them. |

**What isn't promised**: downgrades across a format change (a newer layout is refused, `UnsupportedLayout`), performance figures, iteration order where the documentation doesn't give one (design rule 7), log lines and messages, the console's pages, and anything marked experimental in its documentation. A fix that makes the code do what [guarantees.md](../guarantees.md) or the API reference says is a patch, even if someone relied on the bug; the CHANGELOG names it.

**Before 1.0** nothing is promised: 0.x was never released.

### Pre-releases

- A pre-release is `X.Y.Z-alpha.N`, `X.Y.Z-beta.N` or `X.Y.Z-rc.N` (N a number, from 1), tagged `vX.Y.Z-rc.N`. No other suffix: these three have a PEP 440 spelling, and the release gate (`scripts/release-gate.sh`) refuses the rest. Build metadata (`+...`) isn't used.
- **The Python package spells them `X.Y.ZaN`, `X.Y.ZbN` and `X.Y.ZrcN`** (maturin's conversion, checked by the release workflow: the wheel is `ironweaver_db-1.0.0rc1-...whl`). `iwdb.__version__` and `--version` print the semver form, `1.0.0-rc.1`.
- The GitHub Release of a pre-release is marked **pre-release**, never "Latest".
- A pre-release promises nothing between itself and the final release: data it wrote may not open in the final version. It is tagged on a branch `release/X.Y.Z-rc.N` that isn't merged into `main`, so its version bump, CHANGELOG section and release fixture stay out of `main`; the fixtures in the repository are those of final releases only.

### Every release

- Has a `## [X.Y.Z] - <date>` section in `CHANGELOG.md` (Keep a Changelog); its text is the release's notes. Breaking changes (in a major) and changed defaults are listed first.
- Has its release fixture, written by the release commit and committed with it.
- Is checked by the release gate before anything is built: the tag is the workspace version, Cargo.lock agrees, the fixture and the CHANGELOG section exist.
- **Is never retagged.** A pushed version tag is never moved, deleted or reused, even when the workflow failed before publishing; a broken release is fixed by the next patch (or the next `rc.N`). The procedure is [releasing.md](../releasing.md).

## Consequences

- One version for everything is simple to read, but every release bumps every crate, including the internal ones, and a Python-only fix is a release of the server too. With one repository and one pipeline that costs nothing.
- The release fixtures make the format promise stronger than N-1: a 1.x opens every earlier 1.x's data, so a minor release that adds a format version keeps every older reader that a 1.x wrote. Dropping one waits for 2.0.
- Pre-release fixtures stay off `main`, so the repository doesn't collect fixtures of versions nobody keeps; the price is that a pre-release's commit is on a branch of its own.
- The internal crates have no promise. Someone using `iwdb-storage` directly from git is on their own; the facade is `iwdb`.
- Adding a value to a bounded metric label is minor: an alert on a closed set of codes may need a look after an upgrade. The CHANGELOG names new codes.
