# REST API

`iwdb-server` serves the REST/JSON API on the same port as gRPC ([grpc.md](grpc.md)): connections speak HTTP/1.1 or HTTP/2, and requests whose content type is `application/grpc...` go to gRPC, all others to REST. Every route is one operation of the `Database` trait, with the same semantics, limits and error codes as gRPC and the embedded store (design rule 8). Its messages are the gRPC contract's (`proto/ironweaver_db/v1`) in their proto3 JSON form, so both APIs share one schema.

- The OpenAPI 3.1 document is [openapi.json](openapi.json). A running server also serves it at `GET /v1/openapi.json`.
- Decisions are recorded in [ADR 0030](../adr/0030-rest-json-api.md), plus [ADR 0023](../adr/0023-wire-encoding-of-values-filters-and-patterns.md) for values, filters and patterns, and [ADR 0025](../adr/0025-server-streaming.md) for streamed answers.
- There is no TLS and no authentication yet (step 15). Bind the server to localhost or a private network.

## Routes

| Method | Path | Body / query | Answer | Trait method |
|---|---|---|---|---|
| GET | `/v1/namespaces` | – | `ListNamespacesResponse` | `namespaces` |
| PUT | `/v1/namespaces/{ns}` | `CreateNamespaceRequest` (optional) | `CreateNamespaceResponse` | `create_namespace` |
| DELETE | `/v1/namespaces/{ns}` | `DropNamespaceRequest` (optional) | `DropNamespaceResponse` | `drop_namespace` |
| GET | `/v1/namespaces/{ns}` | – | `GetNamespaceStatusResponse` | `namespace_status` |
| GET | `/v1/namespaces/{ns}/catalog` | options | `GetCatalogResponse` | `catalog` |
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
| GET | `/v1/openapi.json` | – | the OpenAPI document | – |

Path parameters are percent-encoded: node `a/b` is `/nodes/a%2Fb`.

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

On SIGINT or SIGTERM the server stops accepting connections. Idle HTTP/1.1 connections close at once, and busy ones after their current request. Running requests may finish for up to `drain_timeout_secs` (ADR 0027); everything else is as for gRPC ([grpc.md](grpc.md#shutdown)).

## Examples

```sh
B=http://127.0.0.1:7600/v1/namespaces/default
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

# Create a namespace with an idempotency key (a retry answers "deduplicated": true), then drop it
curl -s -X PUT http://127.0.0.1:7600/v1/namespaces/other -H "$J" -d '{"idempotencyKey": "create-1"}'
curl -s -X DELETE http://127.0.0.1:7600/v1/namespaces/other
```

## The Rust client

`iwdb_server::client::RestRemote` (feature `client`) implements the `Database` trait over REST. It is the client the conformance suite runs over REST, and a reference for clients in other languages. Rust programs should prefer the gRPC `Remote` ([grpc.md](grpc.md#the-rust-client)). `RestRemote::ndjson(true)` reads streamed answers as NDJSON.
