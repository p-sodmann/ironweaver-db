# ADR 0038: AGPL-3.0 and a commercial license

Status: accepted
Date: 2026-10-04

## Context

Since step 1 the repository carried an MIT `LICENSE`, matching `ironweaver-core`, but the repository was private and nothing was published under it. Before making it public we choose the license it will be published under. A permissive license lets anyone run the server as a hosted service, or ship a modified copy inside a closed product, without giving anything back. The project wants to be open source while reserving that kind of use for a paid license.

## Decision

- **Dual license: `AGPL-3.0-only` or a commercial license.** `LICENSE` holds the AGPL text, `COMMERCIAL-LICENSE.md` explains the choice and how to ask for a commercial license. "Only", not "or later", so a future AGPL version can't change the terms without a decision here.
- **The metadata says `AGPL-3.0-only`.** The workspace `Cargo.toml` (inherited by every crate), `pyproject.toml`, `console/package.json`, the Docker image label and the OpenAPI document. SPDX has no identifier for a per-licensee commercial license, so the commercial option appears only in prose (README, `COMMERCIAL-LICENSE.md`, the OpenAPI license name).
- **Contributions grant a relicensing right.** Dual licensing only works if the copyright holder may license every line commercially. `CONTRIBUTING.md` states that contributions are under the AGPL and that contributors also grant the maintainer the right to license them under other terms, including commercial ones.
- **Dependencies stay permissive.** `deny.toml` keeps allowing only permissive licenses: a copyleft dependency would block the commercial license. `ironweaver-core` stays MIT and a dependency (design rule 9); its license is unchanged.

## Consequences

- The MIT `LICENSE` was never distributed (the repository was private, no crate, wheel or image was published), so no MIT grant is outstanding: the first public version of Ironweaver DB, and its whole history, is under the AGPL or the commercial license.
- `ironweaver-core` is public and MIT, and stays so; only this repository's code changes license.
- A network service that runs a modified `iwdb-server` must offer its users that source, or hold a commercial license. Using an unmodified server over gRPC or REST doesn't make the client software AGPL.
- Contributions from people other than the maintainer need the grant in `CONTRIBUTING.md` (or a signed CLA, if one is added later) before they can be merged.
- The wording of `COMMERCIAL-LICENSE.md` and the contributor grant is not reviewed by a lawyer; have it reviewed before selling the first license.
