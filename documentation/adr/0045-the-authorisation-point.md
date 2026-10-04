# ADR 0045: The authorisation point

Status: accepted
Date: 2026-10-04

## Context

With users and roles (ADR 0043), every database call to the server must be allowed or refused by the caller's role on the namespace. Design rule 8 says every operation is implemented once, behind the `Database` trait, and the adapters only translate. Points to decide: where credentials are read, where roles are checked, what each operation needs, what is open without credentials, and how a refusal reaches clients.

## Decision

**Credentials are read once, in the gate** (`iwdb_server::serve::Gate`), in front of both APIs: an `authorization: Bearer <token>` header (gRPC metadata is HTTP/2 headers, so the same header) or, over REST, the console's session cookie (ADR 0046). The gate turns the token into a `Principal` through the `Authenticate` trait (implemented by `Embedded`) and attaches a `Caller` (principal, token, client address) to the request. A missing, malformed, unknown or expired token is refused there with `unauthenticated`, before any adapter runs.

**Roles are checked once, by `iwdb_query::Authorized<D>`**: a wrapper of a `Database + Accounts` with the caller's principal, which checks `Operation::requires()` before it delegates. Both adapters build one per request from the `Caller` (`Adapter::db`, `rest::Shared::db`) and call `ops` with it, so `ops` and the operations are unchanged and no adapter decides anything. A request without a `Caller` (a server's service used without its gate, in code) is a server-wide admin's when authentication is off, and refused otherwise.

**The operation table** (`iwdb_query::auth::operations!`), the single source of what each operation needs:

| Needs | Operations |
|---|---|
| `read` on the namespace | `WaitForSeq`, `GetNodes`, `GetEdges`, `Find`, `Explain`, `Neighbourhood`, `Traverse`, `ShortestPath`, `RandomWalks`, `Subgraph`, `MatchPattern`, `Analyze`, `GetChanges` (and `Watch`), `GetCatalog`, `GetNamespaceStatus` |
| `write` on the namespace | `Commit` |
| `admin` on the namespace | `CommitCatalog`, `DropNamespace` |
| the server-wide admin | `CreateNamespace`, `ListUsers`, `CreateUser`, `DeleteUser`, `SetAdmin`, `Grant`, `Revoke` |
| being the user, or the server-wide admin | `SetPassword` (a non-admin gives its current password), `CreateToken`, `RevokeToken`, `ListTokens` |
| any authenticated caller | `ListNamespaces` (filtered to the namespaces the caller has a role on), `WhoAmI`, `Logout` |

Roles include the ones before them (`admin` > `write` > `read`); a server-wide admin has `admin` on every namespace. Grants on a namespace that doesn't exist can't be made (ADR 0043), so a namespace-level role never reaches a namespace created later.

**Open without credentials, by design:** `/v1/health/live`, `/v1/health/ready` and `grpc.health.v1.Health` (probes and load balancers have no credentials; they reveal only liveness and readiness), the console's static pages and `console-config.json` (they hold no data; every read they make goes through the API), `POST /v1/auth/login` and the `Login` RPC, and `/v1/openapi.json` (the published contract).

**In-process access stays unauthenticated.** The embedded store, Python's `iwdb.Store.open` and `iwctl` on a data directory call the store directly: whoever can open the data directory owns the store, and the operating system's file permissions are the boundary. That is also how the first admin is made (ADR 0047).

**Refusals** are two new codes of the error contract: `unauthenticated` (gRPC `UNAUTHENTICATED`, HTTP 401, Python `UnauthenticatedError`) and `permission_denied` (`PERMISSION_DENIED`, 403, `PermissionDeniedError`). A denial names the user, the operation and what it needs ("user 'ann' may not Commit: it needs the 'write' role on namespace 'social'"), never a secret.

**OIDC/JWT later** (step 15's "hooks"): a principal comes from one place, `Authenticate::authenticate(token)`; another authenticator (mTLS in step 15b, a JWT verifier) produces the same `Principal`.

## Consequences

- Every role × operation combination is tested over gRPC and REST from one table (`crates/iwdb-server/tests/roles.rs`: 38 operations, six kinds of caller, both APIs), and the conformance suite runs authenticated.
- A new operation needs a row in the operation table (a test checks the role test's table covers every operation) and a row in the role test.
- A principal is resolved when the request arrives. A namespace dropped and created again during that request's few microseconds would see the old namespace's grant; that needs a server admin doing both at that moment, and was judged acceptable.
- `Server::new` has authentication off (tests and embedders that serve in code choose with `Server::auth`); the binary turns it on per `[auth] enabled` (ADR 0047).
