# ADR 0021: Bounded reads, server limits and cursors

Status: accepted
Date: 2026-10-03

## Context

Design rule 5: every read has limits (results, nodes visited, edges examined, time) and nothing runs unbounded on behalf of a remote caller. The core provides `Budget { max_visited, max_edges, max_results, on_limit }` for its traversals, path expansion and walks (#27), and a cancel token for time. Its path search, pattern matcher and walk planning take no budget (upstream [#48](https://github.com/p-sodmann/Ironweaver/issues/48)).

Paginated reads need a cursor. A cursor carries `(namespace, seq)` plus a position (step 9 notes), and the database has no MVCC snapshot: a namespace is one graph behind a `RwLock`, and the next page is read from whatever the namespace holds then. Design rule 7 says the core's iteration order isn't part of our contract, and slot order changes after recovery.

## Decision

**Limits.** A request's `QueryOptions` carries `max_results`, `max_visited`, `max_edges` and `timeout`, each optional. The server's `LimitConfig` has a default and a hard cap for each (defaults 1 000 / 100 000 / 1 000 000 / 30 s; caps 100 000 / 10 000 000 / 100 000 000 / 5 min). A missing limit takes the default; a limit above the cap is lowered to it (as page sizes are in most APIs); 0 is `invalid_argument`. Each operation documents what it counts; where the core takes a `Budget`, it is the core's count.

**Reaching a limit.** By default the read fails with `budget_exceeded`. With `partial: true` it answers with what it found and `truncated: true`, and no cursor (the search can't be resumed). Exceptions, documented per operation: `max_results` is the page size of paginated reads (more results give a cursor, not an error) and keeps the top rows of analytics jobs (ranked results; `truncated` is set if rows were cut).

**Where the core takes no budget**, `iwdb-query` counts from outside, through the hooks the core offers, until #48 is fixed: BFS paths through `bidirectional_bfs`'s edge filter, Dijkstra and A* through the heuristic (nodes discovered), pattern matching by matches produced (against `max_visited`), and random walks by refusing a graph larger than the limits (planning reads all of it). The timeout bounds the rest. Filtered neighbourhoods use a small BFS of ours (`expand_filtered`) until `expand_limited` takes an edge filter ([#49](https://github.com/p-sodmann/Ironweaver/issues/49)).

**Every read operation has a test** that it stops: a `*_is_bounded` case per operation in the conformance suite, `every_read_times_out` (a deadline that is over before the read starts), and `a_timeout_stops_a_read_in_progress` (a match the matcher can't bound, stopped by the deadline; `iwdb/tests/query.rs`).

**Order and pagination.** Paginated reads (`find`, `neighbourhood`, `match_pattern`) sort their results by a key (node id; for matches, the row's node ids, then edge ids) and continue after the key of the last result returned: a keyset cursor. Each page runs the read again, bounded as the first, and keeps the smallest `max_results + 1` keys in memory. Reads that return a traversal order (`traverse`, `random_walks`) or one value (paths, subgraphs, explain) have no cursor; their limits bound the whole answer. Where they depend on the core's edge order, the documentation says so.

**Cursors are valid at one seq.** A cursor holds the namespace id (a namespace created again under the same name has another), the history id, the seq of the first page, a fingerprint of the request (FNV-1a over the request's fields, with `Expr` and `Pattern` through their serde JSON, which sorts dicts) and the keyset position. The next page is read only if the namespace is still at that seq; otherwise it fails with `cursor_expired`, and the client starts again. A cursor of another namespace, history or request, or a damaged one, is `invalid_argument`. Encoding: `c1` + hex of a versioned binary layout with a checksum (`iwdb_query::cursor`); opaque to clients, short-lived, so no N-1 reader is kept.

We considered serving the next page from the current state (keyset pagination makes that well defined: no duplicates, no skipped results that existed in both states, but a mix of states). It may be added later as an explicit option; failing is the safe default while the API has no snapshot reads.

## Consequences

- No read runs unbounded, and the limits are the same for every access method; servers configure them (`QueryConfig`), clients may only lower them.
- Pagination through a busy namespace fails with `cursor_expired` whenever a commit lands between pages. Callers that need stable large results raise `max_results` (up to the cap) or read at a quiet moment; snapshot reads would lift this.
- Each page re-runs its search, so reading N pages costs N searches; the budgets apply to each.
- Edges examined are reported as 0 for Dijkstra, A* and matching until #48 is fixed.
