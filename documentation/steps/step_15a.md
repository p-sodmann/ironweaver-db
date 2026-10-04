# Step 15a: Authentication and roles

Status: todo
Milestone: M4 Production 1.0
Depends on: step 16b (the gate, the config and logs this builds on)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

Every database call to the server comes from a known principal and is allowed or refused by its role on the namespace, decided once. Operators log in to the console with a password; scripts use API tokens; `iwctl` and Python log in. There is no default password.

## Tasks

### Users, grants and tokens as durable state
- [ ] Decide in an ADR where users, password hashes, grants and API tokens live, so that every change goes through one commit pipeline and the WAL (design rule 2) and any new format is versioned (rule 4). Options: a reserved system namespace (a name ordinary namespaces can't take, its records written by the existing pipeline, so checkpoints, recovery, backup and restore cover it), the namespace log (`NAMESPACES`), or a store-level log of its own. Expected: the system namespace, hidden from `namespaces()` and refused to every direct call.
- [ ] Roles: `read`, `write`, `admin` per namespace (admin implies write implies read; admin covers the catalog and dropping the namespace), and a server-wide `admin` (users, grants, creating namespaces, every namespace). A table in code maps each `Database` operation to the role it needs.
- [ ] A crash test (design rule 3): a user, grant or token change is all-or-nothing and survives recovery, at every failpoint of its commit.

### Passwords, sessions and tokens
- [ ] Passwords hashed with argon2id (RustCrypto `argon2`), the parameters stored with the hash (PHC string) so they can be raised later; a hash with older parameters is re-hashed on the next successful login. Compared in constant time.
- [ ] Login gives a session token: at least 256 random bits from the OS, stored only as a hash (SHA-256 of the token), with an expiry and revocation (logout, password change, user deleted). API tokens work the same way, with a name and no expiry by default, durable; shown once on creation.
- [ ] No password or token is stored, logged, echoed or put in an error message. A test runs the server with JSON logs through logins, failed logins, token creation and password changes and greps every line for the secrets.
- [ ] Failed logins are slowed down per user and per client address in a bounded table (fixed size, oldest evicted), and logged with user and address, without the password. Unknown users take the same time as wrong passwords.

### The contract
- [ ] Proto first: an `AuthService` (`Login`, `Logout`, `WhoAmI`) and admin RPCs (users: create, set password, delete, list; grants: grant, revoke; API tokens: create, revoke, list). Then the REST routes (`POST /v1/auth/login`, `POST /v1/auth/logout`, `GET /v1/auth/whoami`, `/v1/users/...`, `/v1/tokens/...`) through `ops`, then the OpenAPI document. Clients send `authorization: Bearer <token>` in gRPC metadata and HTTP headers.
- [ ] Error codes `unauthenticated` (401, `UNAUTHENTICATED`) and `permission_denied` (403, `PERMISSION_DENIED`) in `Code`, `status.rs`, errors.md, the Python exceptions, and their tests.

### The authorisation point (design rule 8)
- [ ] Authentication (token to principal) in the gate, once, for both APIs; authorisation once, in front of the `Database` trait (e.g. `Authorized<D: Database>` checking the principal against the operation table and the namespace before delegating). No check in an adapter.
- [ ] Unauthenticated by design: `/v1/health/live`, `/v1/health/ready`, `grpc.health.v1`, the console's static files and `console-config.json`, and login. Every other call needs a principal.
- [ ] The embedded store and `iwctl` on a data directory stay unauthenticated (in process; file permissions are the boundary).

### Configuration and bootstrap
- [ ] `[auth]`: `enabled`, `session_lifetime_secs`, login rate limits (attempts, window, table size); `IWDB_AUTH_*` in `KEYS` and config.md. The default is decided in the ADR. With auth off, step 16b's rule holds: a non-loopback listen address needs an explicit flag. With auth on and a non-loopback address before TLS (15b), say what is required (an explicit flag acknowledging plaintext).
- [ ] No default password: the first admin comes from `iwctl user create` (offline, on the data directory) or from `IWDB_AUTH_BOOTSTRAP_PASSWORD` on a first start (no users yet; ignored, with a warning, otherwise). A server with auth on and no users refuses to start with a message that names both ways.

### The console
- [ ] A login page (or a login state on both pages) in the design system's look, and a logout. The REST Source sends credentials and treats 401 as "log in again", not as an outage. The mock Source has a login too.
- [ ] Decide in an ADR how the browser holds the session (an HttpOnly, Secure, SameSite=Strict cookie and why the JSON-only content-type guard of ADR 0030 is enough against CSRF, or add a token; or a bearer token in sessionStorage and its XSS exposure; never localStorage).
- [ ] Revisit `[console] public` (ADR 0041) with auth on; update ADR 0041 and `compose.yaml`.
- [ ] `serve.py` passes `Authorization` and cookies through. `npm test`, `npm run check`, `pytest console/test` green; the pages checked against a real server through `/console/` and through `serve.py`.

### Clients
- [ ] Rust: `client::Remote` and `RestRemote` take credentials (a token, or user and password that log in). The conformance suite runs authenticated.
- [ ] Python: `iwdb.connect(endpoint, token=...)` or `user=`/`password=` (logs in and keeps the token). The remote half of the Python suite runs with auth on; wrong passwords and expired tokens raise the new exception classes.
- [ ] `iwctl shell`: `--token`, or a password prompt, and `\login`. `iwctl user` (`create`, `passwd`, `delete`, `grant`, `revoke`, `list`) and `iwctl token` work offline on a data directory and online through the admin RPCs.

### Tests and docs
- [ ] Every role × operation combination, over gRPC and REST, generated from one table (role, operation, expected outcome).
- [ ] ADRs: user and role storage; password hashing and sessions; the authorisation point; the console's session; bootstrap. `guarantees.md`: what authentication guarantees and what it doesn't (no TLS until 15b). grpc.md, rest.md, errors.md, config.md, openapi.json.

## Acceptance criteria

- Tests for every role × operation combination over gRPC and REST, from one table.
- A user or grant change is all-or-nothing and survives recovery (crash test).
- No secret appears in the logs (test).
- A server with auth on and no users refuses to start; no default password exists.
- The console, `iwctl shell`, Python and the Rust clients log in; the conformance suite and the remote Python suite pass with auth on.

## Non-goals

- TLS and mTLS (step 15b). Until then a password crosses the network in clear; the docs and config say so.
- OIDC/JWT (a hook only: the principal comes from one authenticator interface).
- Per-client limits and quotas (step 15d); the audit log (step 15c; 15a only logs failed logins).
- Row- or label-level permissions.
