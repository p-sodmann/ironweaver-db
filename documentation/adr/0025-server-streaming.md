# ADR 0025: Server-streaming streams one answer in chunks

Status: accepted
Date: 2026-10-03

## Context

The step asks for server-streaming of large results (subgraph, match, traversal). An answer can be large: up to `max_results` of 100 000 nodes (the default cap) with their attributes, far above gRPC's usual 4 MiB message limit.

Paginated reads (`find`, `neighbourhood`, `match_pattern`) return a keyset cursor that is valid only at the seq of the first page (ADR 0021). There are two ways to stream:

1. **Stream the answer of one trait call**, split into messages of bounded size.
2. **Follow the cursor on the server**: call the trait again with each page's cursor and stream page after page until there is none.

Option 2 is a loop of query logic in the adapter (rule 8), it is unbounded unless the adapter invents a total limit that no other access method has, and it fails halfway with `cursor_expired` whenever a commit lands between pages, after part of the result has been sent. The embedded store and REST would answer the same request differently.

## Decision

Option 1. A streaming RPC makes exactly one trait call with the request's options. If it fails, the RPC fails with its status before any message is sent. If it succeeds, the server sends the answer's items in chunks:

- a chunk holds items until its encoded size reaches 1 MiB (`CHUNK_BYTES`), so a message stays far below the message size limit unless a single item is larger;
- every chunk carries the same item fields; the **last** chunk also carries `meta` (seq, next cursor, `truncated`, work). A stream that ends without `meta` was cut off, and the client reports `unavailable`;
- an empty answer is one message with only `meta`.

Streaming RPCs: the reads whose answer is a list (`GetNodes`, `GetEdges`, `Find`, `Neighbourhood`, `Traverse`, `RandomWalks`, `Subgraph`, `MatchPattern`, `Analyze`). Everything else is unary.

The client follows cursors itself, as with the embedded store. Every stream is bounded by the request's limits, because it is one bounded answer.

**A client that goes away stops the work.** While the trait call runs, the handler awaits its future. When the client cancels or disconnects, hyper drops the handler, which drops the trait's future, and dropping it cancels the read's token (ADR 0020): the core's loops stop at their next check. Once the answer exists, a dropped stream drops what is left of it.

## Consequences

- One request, one answer, the same semantics as every other access method (REST's NDJSON in step 12 can use the same chunks).
- The server still builds the whole answer in memory before sending it; streaming bounds the message size and lets the client decode as chunks arrive, not the server's memory. The limits bound the memory.
- Streaming through a large result means paginating with cursors, which fail with `cursor_expired` if the namespace changes between pages (ADR 0021). Snapshot reads would lift that; they are not planned for M3.
