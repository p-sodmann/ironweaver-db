# REST API

`iwdb-server` serves the REST/JSON API on the same port as gRPC ([grpc.md](grpc.md)): connections speak HTTP/1.1 or HTTP/2, and requests whose content type is `application/grpc...` go to gRPC, all others to REST. Every route is one operation of the `Database` trait, with the same semantics, limits and error codes as gRPC and the embedded store (design rule 8). Its messages are the gRPC contract's (`proto/ironweaver_db/v1`) in their proto3 JSON form, so both APIs share one schema.

- The OpenAPI 3.1 document is [openapi.json](openapi.json). A running server also serves it at `GET /v1/openapi.json`.
- Decisions are recorded in [ADR 0030](../adr/0030-rest-json-api.md), plus [ADR 0023](../adr/0023-wire-encoding-of-values-filters-and-patterns.md) for values, filters and patterns, and [ADR 0025](../adr/0025-server-streaming.md) for streamed answers.
- Authentication is on by default ([Authentication](#authentication), step 15a), and so is TLS (step 15b): the API is at `https://`, over HTTP/1.1 or HTTP/2 (ALPN), with the server's certificate ([grpc.md](grpc.md#tls), [config.md](config.md#tls-and-mtls)). curl needs `--cacert ca.pem` for a certificate of a private CA (`docker/dev-cert.sh`'s, for example).

## Routes

| Method | Path | Body / query | Answer | Trait method |
|---|---|---|---|---|
| GET | `/v1/namespaces` | – | `ListNamespacesResponse` | `namespaces` |
| PUT | `/v1/namespaces/{ns}` | `CreateNamespaceRequest` (optional) | `CreateNamespaceResponse` | `create_namespace` |
| DELETE | `/v1/namespaces/{ns}` | `DropNamespaceRequest` (optional) | `DropNamespaceResponse` | `drop_namespace` |
| GET | `/v1/namespaces/{ns}` | – | `GetNamespaceStatusResponse` | `namespace_status` |
| GET | `/v1/namespaces/{ns}/catalog` | options | `GetCatalogResponse` | `catalog` |
| GET | `/v1/namespaces/{ns}/schema` | options (`maxVisited`, `maxEdges`: the sample) | `GetSchemaResponse` | `schema` |
| POST | `/v1/namespaces/{ns}/catalog` | `CommitCatalogRequest` | `CommitCatalogResponse` | `commit_catalog` |
| POST | `/v1/namespaces/{ns}/commit` | `CommitRequest` | `CommitResponse` | `commit` |
| POST | `/v1/namespaces/{ns}/wait` | `WaitForSeqRequest` | `WaitForSeqResponse` | `wait_for_seq` |
| GET | `/v1/namespaces/{ns}/nodes/{id}` | options | `GetNodesResponse`, or 404 | `get_nodes` (one id) |
| POST | `/v1/namespaces/{ns}/get-nodes` | `GetNodesRequest` | `GetNodesResponse` (streamed) | `get_nodes` |
| GET | `/v1/namespaces/{ns}/edges/{id}` | options | `GetEdgesResponse`, or 404 | `get_edges` (one id) |
| POST | `/v1/namespaces/{ns}/get-edges` | `GetEdgesRequest` | `GetEdgesResponse` (streamed) | `get_edges` |
| POST | `/v1/namespaces/{ns}/find` | `FindRequest` | `FindResponse` (streamed) | `find` |
| POST | `/v1/namespaces/{ns}/explain` | `ExplainRequest` | `ExplainResponse` | `explain` |
| POST | `/v1/namespaces/{ns}/neighbourhood` | `NeighbourhoodRequest` | `NeighbourhoodResponse` (streamed) | `neighbourhood` |
| POST | `/v1/namespaces/{ns}/traverse` | `TraverseRequest` | `TraverseResponse` (streamed) | `traverse` |
| POST | `/v1/namespaces/{ns}/shortest-path` | `ShortestPathRequest` | `ShortestPathResponse` | `shortest_path` |
| POST | `/v1/namespaces/{ns}/random-walks` | `RandomWalksRequest` | `RandomWalksResponse` (streamed) | `random_walks` |
| POST | `/v1/namespaces/{ns}/subgraph` | `SubgraphRequest` | `SubgraphResponse` (streamed) | `subgraph` |
| POST | `/v1/namespaces/{ns}/match` | `MatchPatternRequest` | `MatchPatternResponse` (streamed) | `match_pattern` |
| POST | `/v1/namespaces/{ns}/analyze` | `AnalyzeRequest` | `AnalyzeResponse` (streamed) | `analyze` |
| GET | `/v1/namespaces/{ns}/changes` | `from_seq`, `wait` and options as query parameters | `GetChangesResponse` | `changes` |
| GET | `/v1/namespaces/{ns}/changes/stream` | `from_seq` and options as query parameters, `Last-Event-ID` | Server-Sent Events ([changes.md](changes.md)) | `changes` with `wait`, in a loop |
| GET | `/v1/health/live` | – | `Health`: 200 whenever the server answers | – (the server's, [config.md](config.md#health)) |
| GET | `/v1/health/ready` | – | `Health`: 200 when ready, 503 while recovering or shutting down | – (the server's) |
| GET | `/v1/openapi.json` | – | the OpenAPI document | – |
| POST | `/v1/auth/login` | `LoginRequest` | `LoginResponse` (with `cookie: true` the token is set as a cookie instead) | `Authenticate::login` |
| POST | `/v1/auth/logout` | – | `LogoutResponse` (clears the cookie) | `Authenticate::logout` |
| GET | `/v1/auth/whoami` | – | `WhoAmIResponse` | – (the caller's principal) |
| GET | `/v1/users` | – | `ListUsersResponse` | `Accounts::users` |
| POST | `/v1/users` | `CreateUserRequest` | `CreateUserResponse` | `Accounts::create_user` |
| DELETE | `/v1/users/{user}` | – | `DeleteUserResponse` | `Accounts::delete_user` |
| PUT | `/v1/users/{user}/password` | `SetPasswordRequest` | `SetPasswordResponse` | `Accounts::set_password` |
| PUT | `/v1/users/{user}/admin` | `SetAdminRequest` | `SetAdminResponse` | `Accounts::set_admin` |
| PUT | `/v1/users/{user}/grants/{ns}` | `GrantRequest` (`role`) | `GrantResponse` | `Accounts::grant` |
| DELETE | `/v1/users/{user}/grants/{ns}` | – | `RevokeResponse` | `Accounts::revoke` |
| GET | `/v1/users/{user}/tokens` | – | `ListTokensResponse` | `Accounts::tokens` |
| POST | `/v1/users/{user}/tokens` | `CreateTokenRequest` | `CreateTokenResponse` (the secret, once) | `Accounts::create_token` |
| DELETE | `/v1/users/{user}/tokens/{token}` | – | `RevokeTokenResponse` | `Accounts::revoke_token` |
| GET | `/v1/status` | – | `GetServerStatusResponse` | `Admin::server_status` |
| GET | `/v1/requests` | `user`, `limit` as query parameters | `ListRequestsResponse` | `Admin::active_requests` |
| POST | `/v1/requests/{request}/cancel` | `CancelRequestRequest` (optional) | `CancelRequestResponse` | `Admin::cancel_request` |
| GET | `/v1/consumers` | – | `ListConsumersResponse` | `Admin::consumers` |
| GET | `/v1/metrics` | – | `GetMetricsResponse` | `Admin::metrics` |
| GET | `/v1/log` | `after`, `limit` as query parameters | `GetLogResponse` | `Admin::log` |
| POST | `/v1/checkpoint` | `CheckpointRequest` (optional: `namespace`) | `CheckpointResponse` | `Admin::checkpoint` |
| POST | `/v1/backups` | `BackupRequest` (`name`, `max_bytes_per_second`, `no_verify`) | `BackupResponse` | `Admin::backup` |
| POST | `/v1/verify` | `VerifyRequest` (optional: `backup` or `archive`) | `VerifyResponse` | `Admin::verify` |
| POST | `/v1/archive/prune` | `PruneArchiveRequest` (`before`, `dry_run`) | `PruneArchiveResponse` | `Admin::prune_archive` |
| GET | `/metrics` | – | the metrics in Prometheus' text format ([metrics.md](metrics.md)) | `Admin::metrics` |

Path parameters are percent-encoded: node `a/b` is `/nodes/a%2Fb`. A test keeps this table equal to the server's route table.

The operator's routes (`/v1/status` to `/metrics`, step 16c) are described in [Operator reads](#operator-reads), the admin writes (`/v1/checkpoint` to `/v1/archive/prune`, step 16e) in [Admin writes](#admin-writes). The health routes are served in every build (also without the `rest` feature) and while the store recovers; until recovery has finished every other route answers 503 `unavailable` ([ADR 0040](../adr/0040-health-and-readiness.md)). With the `console` feature and `[console] enabled = true`, the operator console's pages are served at `/console/` on the same port ([ADR 0041](../adr/0041-console-served-by-the-server.md)); they aren't part of this API.

## Operator reads

`admin.proto`'s `AdminService` (step 16c, [ADRs 0050 to 0052](../adr/0051-the-status-views.md)): the server's status, the running requests and cancelling one, the change-stream readers, the metrics and the log tail. Every list is bounded (at most 1000 entries; the readers at most 1024).

- **Who may.** Any authenticated caller reads `/v1/status`, `/v1/consumers`, `/v1/metrics` and `/metrics`, narrowed to the namespaces it has a role on (and the series of no namespace), and lists and cancels its own requests. A server-wide admin sees and cancels everyone's requests and reads `/v1/log`. Cancelling is audited.
- **Cancel.** `POST /v1/requests/{id}/cancel` ends a running read: its caller gets 499 `cancelled` (gRPC `CANCELLED`), unless its answer was ready first. A request that isn't running (or is someone else's) is 404 `not_found`; a commit or another change is 400 `invalid_argument`, since cancelling it would only make its outcome unknown.
- **`GET /metrics`** answers in Prometheus' text format (version 0.0.4), in every build, also without `rest`. It needs credentials like any route: give Prometheus an API token (`authorization: { credentials_file: ... }` in its scrape config) over TLS. The server opens no connection to send them anywhere ([SECURITY.md](../../SECURITY.md)).

```sh
# $U and $T as in the example of the next section
curl -s $U/status -H "authorization: Bearer $T" | jq .status.requests
curl -s "$U/requests?limit=10" -H "authorization: Bearer $T"
curl -s -X POST $U/requests/42/cancel -H "authorization: Bearer $T"
curl -s https://127.0.0.1:7600/metrics -H "authorization: Bearer $T"
```

## Admin writes

The admin writes of `AdminService` (step 16e, [ADR 0055](../adr/0055-admin-writes-and-iwctl-against-a-server.md)): checkpoints, backups into the server's backup directory, verifying, and pruning the WAL archive. Only a server-wide admin may call them (403 otherwise), every call is audited, and none can be cancelled. They have no deadline: a throttled backup answers when its copy is done. `iwctl --server` makes the same calls ([iwctl.md](../iwctl.md)).

- **`POST /v1/checkpoint`**: `{}` checkpoints every namespace, `{"namespace": "social"}` one. Each answer says whether a checkpoint was written and what was removed (archived first, with `[store] archive`). It waits for a running backup's copy.
- **`POST /v1/backups`**: `{"name": "nightly-2026-10-06"}` writes a backup into `[backup] dir` under that name (1 to 128 ASCII letters, digits, `.`, `_` or `-`, not starting with `.`), then verifies it (`"no_verify": true` skips that). A name under which anything exists is 409 `conflict`; without `[backup] dir`, 400 `invalid_argument`. `max_bytes_per_second` overrides `[backup] max_bytes_per_second` (0: unthrottled). Checkpoints wait for the whole copy. A backup that fails is removed.
- **`POST /v1/verify`**: `{}` verifies the running store (its checkpoints and WAL up to each namespace's synced seq, and its live state), `{"backup": "<name>"}` a backup in the backup directory, `{"archive": true}` the WAL archive. Damage is in the report's `problems`, with 200. While the server refuses writes for memory, 503 `resource_exhausted`.
- **`POST /v1/archive/prune`**: `{"before": "<name>"}` removes from the WAL archive what no restore from that backup (or a later one) can need; `"dry_run": true` only reports it. A backup of another history is 400.
- **Restore** has no route: it is offline (`iwctl restore` on the server's host).

```sh
curl -s -X POST $U/backups -H "authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name": "nightly", "max_bytes_per_second": 52428800}' | jq .verify.problems
curl -s -X POST $U/archive/prune -H "authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"before": "nightly", "dry_run": true}'
```

## Authentication

With `[auth] enabled` (the default, [config.md](config.md#authentication-and-the-first-admin)) every route but the health routes, `POST /v1/auth/login` and `/v1/openapi.json` needs credentials; so do the console's API calls (its pages themselves are open). [ADRs 0043 to 0047](../adr/0045-the-authorisation-point.md).

- **Log in** with `POST /v1/auth/login` and `{"user": "...", "password": "..."}`. The answer's `token` is a session token (256 random bits) until `expiresMs`, logout, a password change, the user's deletion or a server restart. Scripts can use an **API token** instead (`POST /v1/users/{user}/tokens`): the same kind of bearer token, with a name, no expiry unless asked for, surviving restarts and password changes, revoked by name.
- **Send it** as `Authorization: Bearer <token>`.
- **The console's cookie.** With `"cookie": true` the login sets the session as `iwdb_session`, an `HttpOnly; SameSite=Strict` cookie (no script can read it, no other site sends it), and leaves `token` empty. A request authenticated by the cookie with a method other than GET or HEAD must also send `X-Iwdb-Csrf: 1`: a page of another origin can't send a custom header without a CORS preflight, which the server doesn't answer ([ADR 0046](../adr/0046-the-console-session.md)). Over TLS the cookie is also `Secure` (sent over HTTPS only); a server whose TLS is off sets it without, since browsers refuse Secure cookies from plain HTTP.
- **Client certificates** (mTLS, step 15b, with `[tls] client_ca`): a verified certificate whose subject's common name is a user authenticates as that user when the request carries no token or cookie (`curl --cert ann.pem --key ann.key`). Like the cookie, a browser sends it on its own, so a request it alone authenticates with a method other than GET or HEAD must also send `X-Iwdb-Csrf: 1`. With `[tls] client_auth = "required"` every route but health (and the console's pages) needs one, login included.
- **Errors.** No credentials, or an unknown, expired or revoked token: 401 `unauthenticated`. Credentials whose roles don't allow the operation: 403 `permission_denied`. A wrong user or password, and too many failed logins, are 401 with the same message whether the user exists or not.
- **Roles** are per namespace: `read` (reads, the change stream, the catalog, the status), `write` (and commits), `admin` (and catalog changes and dropping it); a server-wide admin has every role and manages users, grants and namespaces (creating one needs it). `GET /v1/namespaces` lists the namespaces the caller has a role on. Users change their own password with `currentPassword`, and manage their own tokens.

```sh
U=https://127.0.0.1:7600/v1
alias curl='curl --cacert docker/tls/ca.pem'   # compose's development CA (docker/dev-cert.sh)
T=$(curl -s $U/auth/login -H 'content-type: application/json' \
      -d '{"user": "admin", "password": "..."}' | jq -r .token)
curl -s $U/namespaces -H "authorization: Bearer $T"
curl -s -X POST $U/users -H "authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name": "ann", "password": "a long password"}'
curl -s -X PUT $U/users/ann/grants/default -H "authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"role": "ROLE_WRITE"}'
curl -s -X POST $U/users/ann/tokens -H "authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name": "ci"}'
```

## Requests

- **The body is the RPC's request message** in JSON, for example `{"filter": {"Label": "Person"}}` for `find`. The namespace (`namespace`, or `name` for namespaces) comes from the path. You can leave it out of the body; a body that names another namespace is `invalid_argument`.
- **Send `Content-Type: application/json`** with every non-empty body; anything else gets 415. curl's `-d` alone sends `application/x-www-form-urlencoded`, so add `-H 'content-type: application/json'`. The check also keeps web pages on other origins from writing to a server on your machine: a browser can't send that content type cross-site without a CORS preflight, and the server doesn't answer preflights.
- **An empty body is the empty message.** For `PUT` and `DELETE` on a namespace that means no idempotency key.
- **Unknown fields are refused** (`invalid_argument`), so a misspelt option fails instead of being ignored.
- **Bodies are limited** to `[server] max_message_bytes` (64 MiB by default); a larger one gets 413.
- **GET reads take their options as query parameters**: `min_seq`, `history`, `timeout_ms`, `max_results`, `max_visited`, `max_edges`, `partial` and `cursor`, for example `/v1/namespaces/default/nodes/ann?min_seq=12&timeout_ms=500`. Unknown parameters are refused.

### JSON form of the messages

The JSON form is the [proto3 JSON mapping](https://protobuf.dev/programming-guides/json/), as pbjson implements it:

- Field names are lowerCamelCase (`edgeTypes`, `minSeq`). The proto's snake_case names are accepted too.
- **64-bit integers are strings**: `"seq": "12"`, `"ids": ["3"]`. JSON numbers are accepted in requests.
- Enums are their names: `"direction": "DIRECTION_IN"`.
- `bytes` fields are base64. Floats are numbers, or `"NaN"`, `"Infinity"` and `"-Infinity"`.
- Fields with their default value (0, `""`, `false`, empty lists) are left out of answers. A oneof is one of its member fields, for example `{"upsertNode": {...}}` in a `Mutation`.

### Values, filters and patterns

These three use the core's own JSON form instead of their wrapper messages (ADR 0023, ADR 0030):

- **A value** is the core's `Value` serde form: `{"String": "ann"}`, `{"Int": 30}`, `{"Float": 1.5}`, `{"Bool": true}`, `"None"`, `{"List": [...]}`, `{"Dict": {"k": ...}}`, `{"Bytes": "<base64>"}`, `{"Date": "2024-05-01"}`, `{"DateTime": "2024-05-01T12:30:00+02:00"}`. Attributes and meta are maps of values: `"attr": {"age": {"Int": 30}}`. Dict keys are written sorted.
- **A filter** is the core's `Expr` serde form: `{"Label": "Person"}`, `{"Type": "knows"}`, `{"Compare": {"path": ["age"], "op": "Ge", "value": {"Int": 18}}}`, `{"In": {"path": ["city"], "values": [...]}}`, `{"Exists": {"path": ["email"]}}`, `{"And": [...]}`, `{"Or": [...]}`, `{"Not": {...}}`, `{"Const": true}`. Unknown fields are refused. The semantics are the core's; see [grpc.md](grpc.md#values-filters-and-patterns).
- **A pattern** is its text, `"(a:Person {age: 30})-[:knows*1..3]->(b)"`. Patterns the text can't express (bound ids, filters other than property equality) are the core's `Pattern` serde form, an object with `nodes` and `edges`. Usually it is simpler to send the text and add filters in `MatchPatternRequest.filters`.

**Depth.** Values and filters nested up to 100 levels are accepted anywhere in a request; deeper ones are `invalid_argument` with the core's message, as over gRPC. Answers can hold values 100 levels deep, which is about 200 levels of JSON. Some JSON parsers stop earlier (serde_json at 128 levels by default; turn its limit off with `disable_recursion_limit`). Python's `json` and JavaScript's `JSON.parse` read them.

## Answers

- **Unary operations** answer `200` with their response message.
- **Streamed operations** (marked in the table) answer `200` with **one message** by default: the chunks gRPC would stream, merged, so all items and the `meta` are in one object. With **`Accept: application/x-ndjson`** they answer with **NDJSON** instead: one chunk per line, each a response message with up to about 1 MiB of items, and only the last line has `meta` (ADR 0025). An empty answer is one line with only `meta`. Both forms are built before the first byte is sent, so a failure always comes as an error status, never as a cut-off body.
- **`meta`** holds `seq` (the state the read saw), `next` (the cursor of the next page, absent if there is none), `truncated` and `work`.
- **Pagination** works as over gRPC: send `meta.next` as `options.cursor` with the same request. The cursor is valid at one seq; if the namespace changed in between, the read fails with `cursor_expired` (410).
- **Single entities.** `GET .../nodes/{id}` and `GET .../edges/{id}` answer with the `GetNodesResponse` / `GetEdgesResponse` of that one id, or with 404 `not_found` if it doesn't exist. `POST .../get-nodes` answers for missing ids with empty entries (`{}`), in the order asked.

## Errors

A failed request answers with the HTTP status of its error code and an `Error` message as the body:

```json
{"code": "not_found", "message": "no namespace 'nope'"}
```

Branch on `code`, never on the message or the status alone: `conflict` and `constraint_violation` are both 409, and `read_only`, `unavailable` and `io` are all 503. The codes, their meaning, whether to retry, and the HTTP status of each are in [errors.md](errors.md). `cancelled` is 499, which no standard defines; you will rarely see it, because the client that cancelled has gone away.

The HTTP layer refuses some requests before any operation runs. These all have code `invalid_argument`:

| Status | When |
|---|---|
| 400 | the body isn't valid JSON or not the route's message, an unknown field or query parameter, an invalid path parameter |
| 404 | no route matches the path |
| 405 | the route doesn't take the method |
| 413 | the body is larger than `max_message_bytes` |
| 415 | a non-empty body without `Content-Type: application/json` |

An answer without an `Error` body (a proxy's 502, for example) didn't come from the server.

## Deadlines, cancellation and commits

- **Reads** run until `options.timeoutMs` (or `timeout_ms` in the query), capped by the server's maximum, counted from when the server starts the request. Running out of time is `timeout` (504). HTTP has no deadline header; a client-side timeout just closes the connection.
- **A client that closes its connection** (HTTP/1.1 or HTTP/2) stops its read: the server drops the request, and the read stops at the core's next check.
- **Commits** run to the end once accepted, even if the client goes away. After a client-side timeout, a lost connection or `unavailable`, retry with the same idempotency key (`options.idempotencyKey` for commits, `idempotencyKey` for namespaces). The retry returns the original result with `deduplicated: true` ([ADR 0015](../adr/0015-idempotency-keys.md)).

## Shutdown

On SIGINT or SIGTERM the server stops accepting connections. Idle HTTP/1.1 connections close at once, and busy ones after their current request. Running requests may finish for up to `drain_timeout_secs` (ADR 0027), and change streams (SSE) end at once with an `error` event (`unavailable`); everything else is as for gRPC ([grpc.md](grpc.md#shutdown)).

## Examples

```sh
# With authentication on, add -H "authorization: Bearer $T" to every call, and --cacert for a private CA (see above)
B=https://127.0.0.1:7600/v1/namespaces/default
J='content-type: application/json'

# Commit two nodes and an edge (64-bit numbers come back as strings)
curl -s $B/commit -H "$J" -d '{"mutations": [
  {"upsertNode": {"id": "ann", "labels": ["Person"], "attr": {"age": {"Int": 30}}}},
  {"upsertNode": {"id": "bob", "labels": ["Person"]}},
  {"addEdge": {"from": "ann", "to": "bob", "type": "knows"}}]}'
# {"result":{"seq":"1","edgeIds":["0"],"versions":[...],"timeMicros":"..."}}

# Read one node, after seq 1 (read-your-writes)
curl -s "$B/nodes/ann?min_seq=1"

# Find, 100 per page, as NDJSON
curl -s $B/find -H "$J" -H 'accept: application/x-ndjson' \
  -d '{"filter": {"Compare": {"path": ["age"], "op": "Ge", "value": {"Int": 18}}},
       "options": {"limits": {"maxResults": 100}}}'

# Match a pattern
curl -s $B/match -H "$J" -d '{"pattern": "(a:Person)-[:knows]->(b)"}'

# PageRank
curl -s $B/analyze -H "$J" -d '{"job": {"pageRank": {}}}'

# Follow the change stream from seq 1 (Server-Sent Events; Ctrl-C to stop)
curl -N "$B/changes/stream?from_seq=1"

# Create a namespace with an idempotency key (a retry answers "deduplicated": true), then drop it
curl -s -X PUT https://127.0.0.1:7600/v1/namespaces/other -H "$J" -d '{"idempotencyKey": "create-1"}'
curl -s -X DELETE https://127.0.0.1:7600/v1/namespaces/other
```

## The Rust client

`iwdb_server::client::RestRemote` (feature `client`) implements the `Database` trait over REST. It is the client the conformance suite runs over REST, and a reference for clients in other languages. Rust programs should prefer the gRPC `Remote` ([grpc.md](grpc.md#the-rust-client)). `RestRemote::ndjson(true)` reads streamed answers as NDJSON. Like `Remote` it takes a token (`with_token`) or logs in (`login`), implements `Accounts`, and speaks TLS to `https://` endpoints (`RestRemote::connect_tls` with a `ClientTls`: a CA, and a client certificate for mTLS). It sends `X-Iwdb-Csrf` with every request, so a client certificate alone can authenticate its writes.
