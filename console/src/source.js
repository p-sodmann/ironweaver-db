/* Ironweaver DB operator console: the Source contract (step 16a, ADR 0037).
 *
 * Every read and write of the console goes through one Source object; the pages never call anything else. Today
 * the only Source is the mock (mock.js). Hooking the console up means writing a Source over the REST API
 * (documentation/api/rest.md) with these methods, and nothing in the pages changes. All methods return
 * Promises; a failure rejects with an Error carrying `code`, one of documentation/api/errors.md
 * (`invalid_argument`, `not_found`, `read_only`, `constraint_violation`, ...).
 *
 * Shapes are the REST API's JSON (camelCase, attributes in the core's value form {"Int": 30}, filters in its
 * Expr form, patterns as its text, mutations as `commit` bodies), with 64-bit numbers as JS numbers.
 *
 *   method                         REST today                                   returns
 *   namespaces()                   GET /v1/namespaces                           [{id, name, createdMicros, createdSeq}]
 *   namespaceStatus(ns)            GET /v1/namespaces/{ns}                      NamespaceStatus (+ lastCheckpointMs: step 16)
 *   schema(ns)                     new (step 16 status views)                   {labels: [{name, count, keys: {key: {Kind: n}}}], types: [{name, count}], constraints}
 *   getNodes(ns, ids)              POST .../get-nodes                           {nodes: [Node|null], meta}
 *   getEdges(ns, ids)              POST .../get-edges                           {edges: [Edge|null], meta}
 *   find(ns, filter, {limit, cursor})  POST .../find                            {nodes, total, meta: {seq, next, truncated, work}}  (total: mock only)
 *   explain(ns, filter, {analyze}) POST .../explain                             {explain: {plan, estimatedCandidates, candidates, matched, nodes, building}, meta}  (matched: mock only)
 *   neighbours(ns, id, {limit})    .../neighbourhood (depth 1) + .../subgraph   {nodes, edges, truncated, meta}
 *   subgraph(ns, ids)              POST .../subgraph                            {edges, meta}
 *   matchPattern(ns, text, {limit}) POST .../match                              {columns: [{name, kind}], rows: [{nodes, edges}], meta}
 *   commit(ns, mutations)          POST .../commit                              {seq, edgeIds, timeMicros}
 *   createIndex(ns, path)          POST .../catalog (createIndex)               {seq}
 *   server()                       new (step 16: status views and metrics)      see mock.js `server`
 *   cancel(requestId)              new (step 16: cancel a request)              {}
 *   log(), onLog(f) -> unsubscribe new (step 16: the server log)                [{t, level, msg}]
 *   tick()                         none: the mock's clock; a server Source makes it a no-op
 *
 * "new" marks what the server doesn't offer yet; step 16 adds it (see documentation/steps/step_16a.md).
 */
(function (root) {
  'use strict';
  const METHODS = ['namespaces', 'namespaceStatus', 'schema', 'getNodes', 'getEdges', 'find', 'explain', 'neighbours', 'subgraph', 'matchPattern', 'commit', 'createIndex', 'server', 'cancel', 'log', 'onLog', 'tick'];
  const api = { METHODS };
  root.IW = root.IW || {}; root.IW.sourceContract = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
