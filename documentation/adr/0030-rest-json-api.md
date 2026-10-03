# ADR 0030: The REST/JSON API

Status: accepted
Date: 2026-10-03

## Context

Step 12 serves the `Database` trait over HTTP/JSON for browsers, scripts and curl, with the same semantics as gRPC (step 11). The gRPC contract (`proto/ironweaver_db/v1`) is the canonical schema (ADR 0023): REST must not grow a second one, and every operation must still be implemented once, behind the trait (design rule 8). Points to decide:

- how REST and gRPC are served (one port or two) and how a request reaches the right one;
- how JSON is mapped onto the messages, in particular `Value`, `Expr` and `Pattern`, which carry the core's types as postcard (ADR 0023);
- how values and filters nested 100 levels deep are read from JSON without letting a crafted body overflow the stack (upstream #57, fixed in `7e7b7fa`);
- the routes, streamed answers, errors and the OpenAPI document.

## Decision

**One implementation per operation.** `iwdb_server::ops` holds every operation once: decode the request message, make one trait call, encode the answer (as chunks for streamed answers, ADR 0025). The gRPC service and the REST router only add their transport. No operation logic lives in either adapter.

**One port, both protocols.** The accept loop (ADR 0027) serves each connection with hyper-util's `auto` builder, which detects HTTP/1.1 or HTTP/2 per connection. A request whose `Content-Type` starts with `application/grpc` goes to the tonic service, and every other request goes to an axum `Router`. The config, the listen address, the message size limit and the graceful shutdown are shared. On shutdown an HTTP/1.1 connection closes after its current request, and an idle one closes at once.

**Messages as proto3 JSON.** pbjson generates `Serialize` / `Deserialize` for every message (build script, `pbjson-build`, with the same `btree_map` setting as prost). That gives lowerCamelCase field names (snake_case accepted too), 64-bit integers as decimal strings (numbers accepted too), enums by name, bytes as base64, and unknown fields refused.

**`Value`, `Expr`, `Pattern`: the core's JSON form.** These three messages are excluded from pbjson (`exclude`, not `extern_path`: the types are the prost-generated ones in our crate, so we implement serde on them directly) and get a hand-written serde (`rest/json.rs`). A `Value` or `Expr` is the core's serde form (`{"Int": 30}`, `{"Label": "Person"}`). A `Pattern` is its text, or the core's serde form (an object) for patterns the text can't express. Reading works in two steps:

1. The field's JSON is captured as a `serde_json::value::RawValue`. serde_json skips over it iteratively, without recursion and without counting depth.
2. The raw text is read by the core's `Value::from_json_str` / `Expr::from_json_str` / `Pattern::from_json_str` (#57). These turn serde_json's own recursion limit off and rely on the core's depth counters, which is safe because the core's types refuse unknown fields instead of skipping them.

So values and filters up to 100 levels deep are accepted wherever they sit in a request, 101 are refused with the core's message, exactly as over gRPC and embedded. The result is stored as postcard in the message, so `convert` decodes it the same way for both APIs. This costs one extra postcard encode per value over REST.

**Routes.** Resource-style, every namespace-scoped route under `/v1/namespaces/{ns}`. A shorter `/v1/{ns}/...` would collide with `/v1/namespaces` for a namespace named `namespaces`. The route table `rest::ROUTES` is the single source: the router is built from it, and the OpenAPI document describes it. Batch reads are `POST .../get-nodes` and `.../get-edges` rather than `POST .../nodes`, which would read as "create". `GET .../nodes/{id}` and `.../edges/{id}` answer 404 for a missing entity. Analytics are one route, `POST .../analyze`, with the job in the body (the `Job` oneof), not a route per job.

**Requests.** A route's body is its RPC's request message. The namespace is taken from the path, and a body naming another one is `invalid_argument`. An empty body is the empty message. A non-empty body must be `application/json`, otherwise the answer is 415. A cross-site page can't send that type without a CORS preflight, which the server doesn't answer, so a page on another origin can't commit through a browser to a server on localhost. GET reads take `QueryOptions` as query parameters. Deadlines are `options.timeoutMs` only: HTTP has no deadline header.

**Answers.** A unary operation answers with its response message. A streamed operation answers by default with its chunks merged into one message (protobuf merge: repeated fields concatenate, and `meta` comes from the last chunk), which is what curl and browsers want. With `Accept: application/x-ndjson` it answers with one chunk per line, the same chunks gRPC streams, `meta` in the last line. Both are built before the first byte is sent (ADR 0025), so a failure is always a status, never a broken stream. Server-Sent Events are for the change stream (step 13).

**Errors.** The HTTP status of the error's code (`status::http_status`, the HTTP column of errors.md) and the `Error` message (`{"code": "...", "message": "..."}`) as the body. Requests the HTTP layer refuses (no such route, wrong method, wrong media type, body too large) are `invalid_argument` with 404, 405, 415 or 413.

**OpenAPI.** `rest::openapi` generates an OpenAPI 3.1 document at runtime from the route table and the embedded descriptor set (the protos' comments become descriptions), and the server serves it at `/v1/openapi.json`. The schemas follow pbjson's mapping, and `Value` / `Expr` / `Pattern` are written out by hand. `documentation/api/openapi.json` is a copy that a test keeps equal to it. Tests check that every reference resolves, every schema is used, the document describes exactly the routes, and every RPC has a route. CI validates the copy against OpenAPI 3.1 with Redocly's recommended rules (`redocly.yaml`). A Rust OpenAPI parser (`oas3`) was tried as a test dependency and dropped: it adds about 40 crates (url, icu, regex) and checks little beyond types.

**A REST client for the conformance suite.** `client::RestRemote` (feature `client`) implements the trait over REST with the same `convert` functions as the gRPC `Remote`. The conformance suite runs over REST twice: answers as one message and as NDJSON.

## Consequences

- REST and gRPC can't drift: one schema, one implementation per operation, and the same conformance suite.
- Values and filters have the same depth limit, 100, on every access method.
- Clients that parse REST answers with serde_json's defaults (or another parser with a nesting limit) can't read values nested more than about 60 levels: such a value takes 2 JSON levels per level. The REST docs say so.
- 64-bit integers are strings in JSON (proto3 JSON), so JavaScript keeps them exact, but clients have to parse them.
- New dependencies of `iwdb-server`: axum (its router, `matchit`, brings the BSD-3-Clause license, now allowed in `deny.toml`), pbjson / pbjson-build, prost-types, and http, http-body-util, bytes and tower-service, which were already in the tree.
- Over HTTP/1.1 a client that closes its connection cancels its read, like over HTTP/2 (tested).
- The REST API has no authentication or TLS until step 15, like gRPC.
