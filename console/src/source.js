/* Ironweaver DB operator console: the Source contract (step 16a, ADR 0037).
 *
 * Every read and write of the console goes through one Source object; the pages never call anything else. There
 * are two: the mock (mock.js) and the REST API (rest.js, through serve.py); the pages don't know which they have. All methods return
 * Promises; a failure rejects with an Error carrying `code`, one of documentation/api/errors.md
 * (`invalid_argument`, `not_found`, `read_only`, `constraint_violation`, ...).
 *
 * Shapes are the REST API's JSON (camelCase, attributes in the core's value form {"Int": 30}, filters in its
 * Expr form, patterns as its text, mutations as `commit` bodies), with 64-bit numbers as JS numbers.
 *
 *   method                         REST                                         returns
 *   namespaces()                   GET /v1/namespaces                           [{id, name, createdMicros, createdSeq}]
 *   namespaceStatus(ns)            GET /v1/namespaces/{ns}                      NamespaceStatus (unsynced, sinceCheckpoint, lastCheckpointMicros: step 16c)
 *   schema(ns)                     GET .../schema + GET .../catalog             {labels: [{name, count, sampled, keys: {key: {Kind: n}}, moreKeys}], types: [{name, count}],
 *                                                                                constraints, nodes, edges, sampledNodes, sampledEdges}
 *   getNodes(ns, ids)              POST .../get-nodes                           {nodes: [Node|null], meta}
 *   getEdges(ns, ids)              POST .../get-edges                           {edges: [Edge|null], meta}
 *   find(ns, filter, {limit, cursor})  POST .../find                            {nodes, meta: {seq, next, truncated, work}}
 *   explain(ns, filter, {analyze}) POST .../explain                             {explain: {plan, estimatedCandidates, candidates, nodes, building}, meta}
 *   neighbours(ns, id, {limit})    POST .../subgraph (depth 1)                  {nodes, edges, truncated, meta}
 *   subgraph(ns, ids)              POST .../subgraph                            {edges, meta}
 *   matchPattern(ns, text, {limit}) POST .../match                              {columns: [{name, kind}], rows: [{nodes, edges}], meta}
 *   commit(ns, mutations)          POST .../commit                              {seq, edgeIds, timeMicros}
 *   createIndex(ns, path)          POST .../catalog (createIndex)               {seq}
 *   server()                       GET /v1/status, /v1/requests, /v1/consumers, /v1/metrics   Server (below)
 *   cancel(requestId)              POST /v1/requests/{id}/cancel                {request: Request}
 *   log(), onLog(f) -> unsubscribe GET /v1/log (polled by server())             [{t, level, msg}]
 *   logKind()                      none                                         'server' (the server's log) or 'page' (this page's requests:
 *                                                                                the server's log needs a server-wide admin)
 *   tick()                         none: the mock's clock; a server Source makes it a no-op
 *   session()                      GET /v1/auth/whoami                          {authEnabled, user: {name, admin, grants}}
 *   login(user, password)          POST /v1/auth/login (cookie: true)           the session, as session()
 *   logout()                       POST /v1/auth/logout                         {}
 *   onAuth(f) -> unsubscribe       none: f(null) when a call answers 401        (the session ended: log in again)
 *
 * Server (step 16c, ADR 0051), as the server answers, the counts since it started:
 *   {version, startedMicros, ready, fsync, memory: Memory, disk: {walBytes, checkpointBytes, freeBytes|null},
 *    requests: {active, total, timedOut, cancelled, rejected, denied}, namespaces: [NamespaceStatus],
 *    active: [Request], consumers: [{namespace, user, client, nextSeq, lag, lastPollMicros, polls}],
 *    operations: [{operation, calls, errors, p50Ms, p99Ms}]          from the metrics' histograms, since the start
 *    series: {commitsPerSec, commitP50, commitP99, fsyncP99, queryP50, queryP99, active, usedBytes, walBytes}, tickMs}
 *                                                                     the last minute and a half, one value per call of server()
 *   Request: {id, operation, namespace, user, client, startedMicros, elapsedMicros, cancellable}
 *   Memory (step 16d, ADR 0054): {graphBytes, payloadBytes, checkpointBytes, workingBytes, usedBytes (what the limit counts),
 *    limitBytes|null, warnBytes|null, refuseWritesBytes|null, state: 'normal'|'warn'|'refusing_writes',
 *    limitSource: 'config'|'cgroup v2'|'cgroup v1'|null}: the server's own lines and state, not the console's
 * Writes that add fail with `resource_exhausted` while the state is 'refusing_writes'; deletes and drops go on.
 * Every Source also has `endpoint`, the text of where it reads from.
 *
 * Label counts are exact (the core's label index); keys and edge types come from a sample (ADR 0053): the lists are
 * complete when sampledNodes is nodes and sampledEdges is edges. `find` has no total and explain no match count: both
 * would be unbounded counts (design rule 5).
 *
 * Authentication (step 15a, ADR 0046): with the server's [auth] enabled every call but login needs a session. The
 * REST Source holds it in the server's HttpOnly, SameSite=Strict cookie (no script can read it; never in
 * localStorage) and sends `X-Iwdb-Csrf` with every request. A call that answers 401 rejects with code
 * `unauthenticated` and tells the onAuth listeners, so the pages show the login instead of an outage.
 */
(function (root) {
  'use strict';
  const METHODS = ['namespaces', 'namespaceStatus', 'schema', 'getNodes', 'getEdges', 'find', 'explain', 'neighbours', 'subgraph', 'matchPattern', 'commit', 'createIndex', 'server', 'cancel', 'log', 'onLog', 'logKind', 'tick', 'session', 'login', 'logout', 'onAuth'];
  const api = { METHODS };
  root.IW = root.IW || {}; root.IW.sourceContract = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
