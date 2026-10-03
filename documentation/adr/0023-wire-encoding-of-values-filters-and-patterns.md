# ADR 0023: Wire encoding of values, filters and patterns

Status: accepted
Date: 2026-10-03

## Context

Step 11 defines the gRPC contract in `proto/ironweaver_db/v1`. Most of it is plain structure (requests, answers, entities, mutations, the catalog) and maps to proto messages directly. Three types are the core's and recursive:

- `Value` (attribute and meta values): 11 variants, lists and dicts nested at most 100 levels (`format::MAX_DEPTH`, counted the core's way since #31);
- `Expr` (filters): 9 variants, `And` / `Or` / `Not` nested at most 100 levels (`MAX_EXPR_DEPTH`), with `Value`s inside;
- `Pattern` (match): nodes and edges with labels, types, hop bounds, `Expr` filters and bound ids. It has a text form (`Display` / `parse`) that round-trips for every pattern `parse` returns, but not for filters other than property equality or for bound ids (`to_text` refuses those).

Constraints:

1. The protos are the canonical schema for REST too (step 12, through `pbjson`).
2. `buf breaking` must be able to guard them.
3. The depth limits must hold: values and filters nested more than 100 levels are invalid, and those up to 100 are valid, through every access method. Nothing a client sends may overflow the server's stack.
4. A decode error carries the core's message (upstream #29: postcard drops custom serde messages; `format::take_error()` recovers them).
5. Design rule 9: don't reimplement the core.

Options:

- **Real proto messages** (`Value` as a `oneof`, `Expr` as a tree of messages). The best schema: every language gets typed classes, buf guards every variant, `pbjson` gives readable JSON. But prost refuses messages nested more than 100 levels (`RECURSION_LIMIT`, not configurable except by turning it off, which lets 4 MiB of nested length prefixes overflow the stack). A list costs two message levels per value level (`Value` → `ValueList` → `Value`), a dict three (the map entry), plus the request's own levels: a value nested about 33 to 48 levels deep couldn't be sent, a namespace holding one couldn't be read, and the failure would be prost's `INTERNAL` decode error. Avoiding that means hand-writing the prost codecs (and later the serde) of both types. It also mirrors the core's types in our schema: a new `Expr` variant upstream needs a proto change before anyone can use it (rule 9).
- **The core's serde form as JSON text.** Readable, any language can write it, the core's messages survive. But serde_json refuses input nested more than 128 levels by default, and a value 100 levels deep is about 200 JSON levels (`{"List":[...]}`). Turning the limit off (`unbounded_depth`) leaves serde's skipping of unknown struct fields unbounded: one deeply nested unknown field overflows the stack before the core's counters see anything. So JSON either breaks constraint 3 or needs a stack-growing deserializer.
- **The core's serde form as postcard bytes.** Postcard is not self-describing, so nothing is ever skipped: every level of recursion goes through the core's own depth counters, and the limits are exactly the core's (constraint 3). The messages survive through `take_error` (constraint 4). It is compact and deterministic, and it is already our WAL's encoding of the same types, so a core bump that changed it would already fail our WAL fixtures. But it is opaque: non-Rust clients need an encoder for the core's serde layout, buf sees only `bytes`, and `pbjson` would show base64.
- **The pattern text.** Readable and the core's own syntax, but lossy for the patterns described above.

## Decision

Everything structural is a plain proto message. The three core types are thin wrapper messages around the core's serde form, encoded with postcard:

```proto
message Value { oneof form { bytes postcard = 1; } }
message Expr  { oneof form { bytes postcard = 1; } }
message Pattern { oneof form { string text = 1; bytes postcard = 2; } }
```

- **Attributes and meta are `map<string, Value>`**: keys are visible in every language and in JSON, and each value is decoded on its own.
- **Decoding** (`iwdb_server::convert`) clears `format::take_error()`, decodes with `postcard::take_from_bytes`, refuses trailing bytes, and on a failure reports `invalid_argument` with the core's message when there is one (#29), postcard's otherwise. A `Value` or `Expr` without a form (an empty oneof) is `invalid_argument`. The client side decodes answers the same way.
- **Patterns**: the server parses `text` with `Pattern::parse` (errors are the core's) or decodes `postcard`. The Rust client sends the text when `Pattern::to_text` accepts the pattern and postcard otherwise, so both paths are exercised.
- **The oneofs leave room**: another form (for example `string json = 2` on `Value` and `Expr`, if the Python client of step 14 wants one) is an additive change. A change of the core's serde layout would need a new member too: `postcard` means the layout of this contract version.
- **Guarding the inner encoding**: `buf breaking` guards the messages; a test in `iwdb-server` decodes fixed postcard bytes of every `Value` and `Expr` variant (and a pattern) and compares them with the expected values, so a core bump that changes the layout fails it, like the WAL fixtures do for records.
- **REST (step 12)** shows these three messages in the core's JSON form (`{"Int": 30}`, `{"Label": "Person"}`) and a pattern as its text, not as base64: `pbjson` generates the serde of every other message, and these three get a hand-written serde (`pbjson-build`'s `extern_path`). Step 12 decides how its JSON reader handles nesting deeper than serde_json's default limit; the protos don't change for it. The safe way needs the core: upstream [#57](https://github.com/p-sodmann/Ironweaver/issues/57) (filed in this step) asks for `deny_unknown_fields` on `Expr`'s struct variants and a JSON reader that relies on the core's counters.

## Consequences

- The depth limits are exactly the core's on every path, and a crafted request can't recurse deeper than the core's counters allow.
- Rust clients use the core's types directly; a new core variant needs no proto change.
- Non-Rust gRPC clients need a postcard encoder for `Value` and `Expr`: varint discriminants in declaration order, documented with examples in `documentation/api/grpc.md`. That is the price of the depth guarantee; step 14 decides whether the Python client writes one or asks for a JSON form.
- `grpcurl` and other reflection tools show values as base64 until a JSON form exists; REST is the human-readable access method.
- Answers encode each attribute value separately (a few bytes of overhead per value).

*Update, step 12 ([ADR 0030](0030-rest-json-api.md)): #57 was fixed upstream in `7e7b7fa` (`Value` / `Expr` / `Pattern::from_json_str`, unknown fields refused). REST reads these three with the core's readers: values and filters up to 100 levels deep in JSON too, so there is no JSON nesting limit of 64. They are excluded from pbjson's generation (`exclude`) and get a hand-written serde on the prost types; `extern_path` wasn't needed.*
