# Users, grants, API tokens and limits (auth format 2)

Status: stable contract (design rule 4). Implemented in `crates/iwdb/src/auth.rs`. Fixtures: `crates/iwdb/tests/fixtures/auth-v2/` and `auth-v1/` (`auth_fixture.rs`). Decisions: [ADR 0043](../adr/0043-users-and-roles-in-the-system-namespace.md) (where), [ADR 0044](../adr/0044-password-hashing-sessions-and-tokens.md) (hashes and tokens), [ADR 0059](../adr/0059-resource-limits-per-client-and-namespace.md) (limits, format 2).

The store's users live in its reserved namespace `_system` ([data-dir.md](data-dir.md#namespaces-namespaces-ns)), as nodes written with ordinary commits. Framing, checksums, torn tails, checkpoints and recovery are those of every namespace ([wal.md](wal.md), [data-dir.md](data-dir.md)); this document describes what the nodes hold. The namespace exists once the store has had a user: its first commit writes the format node with the first user.

| Node id | Label | Attributes |
|---|---|---|
| `auth` | `AuthFormat` | `format`: `Int` 2 |
| `user:<name>` | `User` | `name`: `String`; `password`: `String`, an argon2id PHC string (`$argon2id$v=19$m=<KiB>,t=<passes>,p=<lanes>$<salt>$<hash>`, base64 without padding); `admin`: `Bool`; `epoch`: `Int`, 1 at creation, +1 at every password change; `grants`: `Dict` from a namespace id (decimal, as a string) to `String` `read`, `write` or `admin`; `created`: `Int`, milliseconds since 1970 UTC; `limits` (optional): `Dict` with any of `requests_per_second`, `burst`, `max_running`, `max_results`, `max_visited`, `timeout_ms`, each an `Int` of at least 0 |
| `token:<sha256>` | `Token` | `user`: `String`; `name`: `String` (unique per user); `created`: `Int`, milliseconds since 1970 UTC; `expires`: `Int`, milliseconds since 1970 UTC, absent for a token that never expires |
| `limits:<namespace id>` | `NamespaceLimits` | any of `max_running`, `max_memory_bytes`, each an `Int` of at least 0 (the namespace id in decimal) |

- `<name>` is a user name: 1 to 64 ASCII letters, digits, `_` or `-`, starting with a letter or digit (the rules of namespace names).
- `<sha256>` is the SHA-256 of the token's text (`iwdb_` and 64 hex digits), in lower-case hex. The token itself is stored nowhere.
- A grant whose namespace id isn't live (the namespace was dropped) is ignored, and removed at the user's next change. A `limits:<id>` node of a namespace that isn't live is ignored, and removed at the next change of any namespace's limits.
- **Limits** (format 2): an absent key or attribute is unset (the server's default applies); 0 is no limit; a number is the limit. A user with no limit set has no `limits` attribute; a namespace with none has no node. A key that isn't one of those listed is `corrupt`: it would be a later format's limit, and ignoring it would enforce less than was set.
- Nothing else is in the namespace. Its catalog has no indexes or constraints.

**Reading.** Every read checks the `auth` node: formats 1 and 2 are read; another value is refused as `corrupt` ("the store's users are in format N, this version reads formats 1 to 2"). A namespace without the `auth` node and with seq 0 has no users (created, then a crash before the first user's commit). Any other missing or ill-typed attribute is `corrupt`.

**Format 1** (before step 15d) is format 2 without limits: no `limits` attributes and no `limits:` nodes. It is read as having no limits. **Every change** (a user, a grant, a token, a password, a limit) to a store in format 1 rewrites the `auth` node to `{format: 2}` in the same commit, so the bump is all or nothing. A binary that reads only format 1 refuses a format-2 store as `corrupt`, and never misreads it.

**Changing the format** means bumping `format`, keeping readers for formats 1 and 2, and adding a fixture `auth-v3/`.
