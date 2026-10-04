# Users, grants and API tokens (auth format 1)

Status: stable contract (design rule 4). Implemented in `crates/iwdb/src/auth.rs`. Fixture: `crates/iwdb/tests/fixtures/auth-v1/` (`auth_fixture.rs`). Decisions: [ADR 0043](../adr/0043-users-and-roles-in-the-system-namespace.md) (where), [ADR 0044](../adr/0044-password-hashing-sessions-and-tokens.md) (hashes and tokens).

The store's users live in its reserved namespace `_system` ([data-dir.md](data-dir.md#namespaces-namespaces-ns)), as nodes written with ordinary commits. Framing, checksums, torn tails, checkpoints and recovery are those of every namespace ([wal.md](wal.md), [data-dir.md](data-dir.md)); this document describes what the nodes hold. The namespace exists once the store has had a user: its first commit writes the format node with the first user.

| Node id | Label | Attributes |
|---|---|---|
| `auth` | `AuthFormat` | `format`: `Int` 1 |
| `user:<name>` | `User` | `name`: `String`; `password`: `String`, an argon2id PHC string (`$argon2id$v=19$m=<KiB>,t=<passes>,p=<lanes>$<salt>$<hash>`, base64 without padding); `admin`: `Bool`; `epoch`: `Int`, 1 at creation, +1 at every password change; `grants`: `Dict` from a namespace id (decimal, as a string) to `String` `read`, `write` or `admin`; `created`: `Int`, milliseconds since 1970 UTC |
| `token:<sha256>` | `Token` | `user`: `String`; `name`: `String` (unique per user); `created`: `Int`, milliseconds since 1970 UTC; `expires`: `Int`, milliseconds since 1970 UTC, absent for a token that never expires |

- `<name>` is a user name: 1 to 64 ASCII letters, digits, `_` or `-`, starting with a letter or digit (the rules of namespace names).
- `<sha256>` is the SHA-256 of the token's text (`iwdb_` and 64 hex digits), in lower-case hex. The token itself is stored nowhere.
- A grant whose namespace id isn't live (the namespace was dropped) is ignored, and removed at the user's next change.
- Nothing else is in the namespace. Its catalog has no indexes or constraints.

**Reading.** Every read checks the `auth` node: `format` 1 is read; another value is refused as `corrupt` ("the store's users are in format N, this version reads format 1"). A namespace without the `auth` node and with seq 0 has no users (created, then a crash before the first user's commit). Any other missing or ill-typed attribute is `corrupt`.

**Changing the format** means bumping `format`, keeping a reader for format 1, and adding a fixture `auth-v2/`.
