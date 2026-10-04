# Step 15a: Authentication and roles

Status: done
Milestone: M4 Production 1.0
Depends on: step 16b (the gate, the config and logs this builds on)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

Every database call to the server comes from a known principal and is allowed or refused by its role on the namespace, decided once. Operators log in to the console with a password; scripts use API tokens; `iwctl` and Python log in. There is no default password.

## Tasks

### Users, grants and tokens as durable state
- [x] Decide in an ADR where users, password hashes, grants and API tokens live, so that every change goes through one commit pipeline and the WAL (design rule 2) and any new format is versioned (rule 4). Options: a reserved system namespace (a name ordinary namespaces can't take, its records written by the existing pipeline, so checkpoints, recovery, backup and restore cover it), the namespace log (`NAMESPACES`), or a store-level log of its own. Decided: the system namespace `_system`, hidden from `namespaces()` and refused to every direct call ([ADR 0043](../adr/0043-users-and-roles-in-the-system-namespace.md), [auth.md](../formats/auth.md), fixture `auth-v1`).
- [x] Roles: `read`, `write`, `admin` per namespace (admin implies write implies read; admin covers the catalog and dropping the namespace), and a server-wide `admin` (users, grants, creating namespaces, every namespace). A table in code maps each `Database` operation to the role it needs.
- [x] A crash test (design rule 3): a user, grant or token change is all-or-nothing and survives recovery, at every failpoint of its commit (`tests/crash/tests/auth_points.rs`).

### Passwords, sessions and tokens
- [x] Passwords hashed with argon2id (RustCrypto `argon2`), the parameters stored with the hash (PHC string) so they can be raised later; a hash with older parameters is re-hashed on the next successful login. Compared in constant time.
- [x] Login gives a session token: at least 256 random bits from the OS, stored only as a hash (SHA-256 of the token), with an expiry and revocation (logout, password change, user deleted). API tokens work the same way, with a name and no expiry by default, durable; shown once on creation.
- [x] No password or token is stored, logged, echoed or put in an error message. A test runs the server with JSON logs through logins, failed logins, token creation and password changes and greps every line for the secrets.
- [x] Failed logins are slowed down per user and per client address in a bounded table (fixed size, oldest evicted), and logged with user and address, without the password. Unknown users take the same time as wrong passwords.

### The contract
- [x] Proto first: an `AuthService` (`Login`, `Logout`, `WhoAmI`) and admin RPCs (users: create, set password, delete, list; grants: grant, revoke; API tokens: create, revoke, list). Then the REST routes (`POST /v1/auth/login`, `POST /v1/auth/logout`, `GET /v1/auth/whoami`, `/v1/users/...`, and tokens under `/v1/users/{user}/tokens`: see "Plan change") through `ops`, then the OpenAPI document. Clients send `authorization: Bearer <token>` in gRPC metadata and HTTP headers.
- [x] Error codes `unauthenticated` (401, `UNAUTHENTICATED`) and `permission_denied` (403, `PERMISSION_DENIED`) in `Code`, `status.rs`, errors.md, the Python exceptions, and their tests.

### The authorisation point (design rule 8)
- [x] Authentication (token to principal) in the gate, once, for both APIs; authorisation once, in front of the `Database` trait (e.g. `Authorized<D: Database>` checking the principal against the operation table and the namespace before delegating). No check in an adapter.
- [x] Unauthenticated by design: `/v1/health/live`, `/v1/health/ready`, `grpc.health.v1`, the console's static files and `console-config.json`, and login. Every other call needs a principal.
- [x] The embedded store and `iwctl` on a data directory stay unauthenticated (in process; file permissions are the boundary).

### Configuration and bootstrap
- [x] `[auth]`: `enabled`, `session_lifetime_secs`, login rate limits (attempts, window, table size); `IWDB_AUTH_*` in `KEYS` and config.md. The default is decided in the ADR. With auth off, step 16b's rule holds: a non-loopback listen address needs an explicit flag. With auth on and a non-loopback address before TLS (15b), say what is required (an explicit flag acknowledging plaintext).
- [x] No default password: the first admin comes from `iwctl user create` (offline, on the data directory) or from `IWDB_AUTH_BOOTSTRAP_PASSWORD` on a first start (no users yet; ignored, with a warning, otherwise). A server with auth on and no users refuses to start with a message that names both ways.

### The console
- [x] A login page (or a login state on both pages) in the design system's look, and a logout. The REST Source sends credentials and treats 401 as "log in again", not as an outage. The mock Source has a login too.
- [x] Decide in an ADR how the browser holds the session (an HttpOnly, Secure, SameSite=Strict cookie and why the JSON-only content-type guard of ADR 0030 is enough against CSRF, or add a token; or a bearer token in sessionStorage and its XSS exposure; never localStorage).
- [x] Revisit `[console] public` (ADR 0041) with auth on; update ADR 0041 and `compose.yaml`.
- [x] `serve.py` passes `Authorization` and cookies through. `npm test`, `npm run check`, `pytest console/test` green; the pages checked against a real server through `/console/` and through `serve.py`.

### Clients
- [x] Rust: `client::Remote` and `RestRemote` take credentials (a token, or user and password that log in). The conformance suite runs authenticated.
- [x] Python: `iwdb.connect(endpoint, token=...)` or `user=`/`password=` (logs in and keeps the token). The remote half of the Python suite runs with auth on; wrong passwords and expired tokens raise the new exception classes.
- [x] `iwctl shell`: `--token`, or a password prompt, and `\login`. `iwctl user` (`create`, `passwd`, `delete`, `grant`, `revoke`, `list`) and `iwctl token` work offline on a data directory and online through the admin RPCs.

### Tests and docs
- [x] Every role × operation combination, over gRPC and REST, generated from one table (role, operation, expected outcome).
- [x] ADRs: user and role storage; password hashing and sessions; the authorisation point; the console's session; bootstrap. `guarantees.md`: what authentication guarantees and what it doesn't (no TLS until 15b). grpc.md, rest.md, errors.md, config.md, openapi.json.

## Plan change

2026-10-04, while implementing:

- **Sessions are in memory**, API tokens durable (ADR 0044): a commit per login would grow the system namespace with every browser tab. A restart logs everyone out.
- **The login slowdown is a lockout window** per user and per address (refused until the window has passed), not an increasing delay: it bounds guesses without holding connections open.
- **The session cookie isn't `Secure`** until TLS (15b): browsers drop Secure cookies over plain HTTP off localhost (ADR 0046). A CSRF header is required on cookie-authenticated writes, because the JSON content-type guard doesn't cover the routes that take an empty body.
- **`[server] plaintext_public` replaces `[console] public`** for the whole server, with authentication on or off (ADR 0047); the Docker image doesn't set it, `compose.yaml` does.
- **The new dependencies are declared in `crates/iwdb/Cargo.toml`**, not the workspace manifest: they are used by `iwdb` only, and the workspace manifest had another session's uncommitted change at the time.
- **A store with users gives its next namespace id 3**: `_system` took id 2. Ids promise only to grow; one Python test assumed 2.
- **API tokens are routed under their user** (`/v1/users/{user}/tokens[/{token}]`), not `/v1/tokens/...`: a token belongs to a user and its name is unique per user.
- **`iwctl user admin <dir> <name> on|off`** was added (the `SetAdmin` RPC), and `iwctl shell` got `\logout` and `\whoami` besides `\login`.

## Notes

- New dependencies, in `iwdb` only: `argon2` (argon2id password hashes, RustCrypto), `sha2` (token hashes, RustCrypto, already in the tree), `getrandom` (salts and tokens from the OS, already in the tree).
- The license change of another session (ADR 0038) was committed on this branch as its own commit before step 15a's changes to the same files (`openapi.rs`, `openapi.json`), as the owner asked.

## Acceptance criteria

- Tests for every role × operation combination over gRPC and REST, from one table. (`every_role_and_operation_over_grpc_and_rest`: 38 operations × 6 callers × 2 APIs)
- A user or grant change is all-or-nothing and survives recovery (crash test). (`user_and_grant_changes_are_all_or_nothing`, under `always` and `group`; `reads_auth_format_1` for the fixture)
- No secret appears in the logs (test). (`no_secret_reaches_the_logs`)
- A server with auth on and no users refuses to start; no default password exists. (`a_store_without_users_refuses_to_start`)
- The console, `iwctl shell`, Python and the Rust clients log in; the conformance suite and the remote Python suite pass with auth on. (`the_shell_logs_in`, `users_and_tokens_on_a_server`, `test_api_tokens_and_roles`, `test_an_expired_session_is_unauthenticated`, the conformance suites over gRPC and REST, `the_console_session_is_a_cookie_with_a_csrf_header`, the console's `npm test`, and a headless-Chromium check of `/console/` and `serve.py`)

## Non-goals

- TLS and mTLS (step 15b). Until then a password crosses the network in clear; the docs and config say so.
- OIDC/JWT (a hook only: the principal comes from one authenticator interface).
- Per-client limits and quotas (step 15d); the audit log (step 15c; 15a only logs failed logins).
- Row- or label-level permissions.
