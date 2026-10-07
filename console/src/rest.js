/* Ironweaver DB operator console: the REST Source (step 16a). The Source contract (source.js) over the server's
 * REST API (documentation/api/rest.md), on the page's own origin: `console/serve.py` serves the pages and passes
 * /v1/... through to the server, because the server answers no CORS preflight (on purpose).
 *
 * It turns the proto3 JSON of the answers into the contract's shapes: 64-bit numbers (strings) into numbers,
 * absent defaults (edge id 0, empty lists, false) into values, AttrPath {keys} into lists.
 *
 * `server()` reads the status views (step 16c, ADR 0051): status, running requests, change-stream readers and the
 * metrics. The series of the last minute and a half are the differences between one call's metrics and the last's
 * (the server keeps no history); the latencies are estimated from the histograms' buckets, as Prometheus'
 * histogram_quantile does. It also polls the server's log, which needs a server-wide admin: for anyone else the log is
 * this page's own requests (`logKind()`).
 *
 * The session (step 15a, ADR 0046) is the server's HttpOnly cookie, set by `login` and sent by the browser on the
 * page's own origin; this script never sees the token. Every request carries `X-Iwdb-Csrf`, which the server
 * requires of cookie-authenticated writes (a page of another origin can't send it without a preflight). A 401
 * rejects with `unauthenticated` and calls the onAuth listeners with null.
 * Classic script: defines globalThis.IW.rest (and module.exports for node --test). */
(function (root) {
  'use strict';

  const SAMPLE_NODES = 10000;    // nodes `schema` samples for keys (label counts are exact)
  const SAMPLE_EDGES = 100000;   // edges it samples for types
  const SERIES = 30;             // values a series keeps: 90 s at the status page's 3 s
  const LOG_KEEP = 200;          // log events kept
  // Reads whose latency is the "query" series: not commits, not the change stream's long polls, not the operator's
  const QUERY_OPS = new Set(['GetNodes', 'GetEdges', 'Find', 'Explain', 'Neighbourhood', 'Traverse', 'ShortestPath', 'RandomWalks', 'Subgraph', 'MatchPattern', 'Analyze', 'GetCatalog', 'GetSchema', 'GetNamespaceStatus', 'ListNamespaces']);
  const VISIT = 5000;            // nodes a neighbourhood may visit
  class SourceError extends Error { constructor(code, message) { super(message); this.code = code; } }

  const n = (x) => (x == null ? 0 : Number(x));
  const opt = (x) => (x == null ? null : Number(x));
  const keys = (p) => (p && p.keys) || [];
  const node = (x) => ({ id: x.id, labels: x.labels || [], attr: x.attr || {}, meta: x.meta || {}, version: n(x.version) });
  const edge = (x) => ({ id: n(x.id), from: x.from, to: x.to, type: x.type, attr: x.attr || {}, meta: x.meta || {}, version: n(x.version) });
  const meta = (m = {}) => ({ seq: n(m.seq), next: m.next || '', truncated: !!m.truncated, work: { visited: n(m.work && m.work.visited), edges: n(m.work && m.work.edges) } });
  const enc = encodeURIComponent;

  function status(s) {
    return {
      id: n(s.id), name: s.name, createdMicros: n(s.createdMicros), seq: n(s.seq),
      syncedSeq: s.syncedSeq != null ? n(s.syncedSeq) : n(s.seq) === 0 ? 0 : null, checkpoint: opt(s.checkpoint),
      unsynced: opt(s.unsynced), sinceCheckpoint: n(s.sinceCheckpoint), lastCheckpointMicros: opt(s.lastCheckpointMicros),
      readOnly: s.readOnly || null, checkpointFailure: s.checkpointFailure || null,
      nodes: n(s.nodes), edges: n(s.edges), memoryBytes: n(s.memoryBytes),
      indexes: (s.indexes || []).map((ix) => {
        const out = { path: keys(ix.path), declared: !!ix.declared, unique: !!ix.unique };
        if (ix.building) out.building = { scanned: n(ix.building.scanned), total: n(ix.building.total) };
        else { out.ready = {}; if (ix.size) out.size = { entries: n(ix.size.entries), distinctKeys: n(ix.size.distinctKeys), memoryBytes: n(ix.size.memoryBytes) }; }
        return out;
      }),
      constraints: n(s.constraints),
      marks: (s.marks || []).map((m) => ({ name: m.name, position: n(m.position), seq: n(m.seq) })),
      recovery: { checkpoint: opt(s.recovery && s.recovery.checkpoint), replayed: n(s.recovery && s.recovery.replayed), seq: n(s.recovery && s.recovery.seq) },
    };
  }
  const request = (q) => ({
    id: n(q.id), operation: q.operation, namespace: q.namespace ?? null, user: q.user || '', client: q.client ?? null,
    startedMicros: n(q.startedMicros), elapsedMicros: n(q.elapsedMicros), cancellable: !!q.cancellable,
  });
  /** A managed job (step 16f): `state` as the contract's word ('running', not JOB_STATE_RUNNING). */
  const job = (j) => ({
    id: n(j.id), namespace: j.namespace || '', user: j.user || '', kind: j.kind || '',
    state: String(j.state || 'JOB_STATE_UNSPECIFIED').replace(/^JOB_STATE_/, '').toLowerCase(),
    createdMicros: n(j.createdMicros), startedMicros: opt(j.startedMicros), endedMicros: opt(j.endedMicros), elapsedMicros: n(j.elapsedMicros),
    progress: j.progress ? { phase: j.progress.phase || '', done: n(j.progress.done), total: opt(j.progress.total) } : null,
    nodes: opt(j.nodes), edges: opt(j.edges), rows: opt(j.rows), truncated: !!j.truncated, resultBytes: n(j.resultBytes),
    error: j.error ? { code: j.error.code || 'internal', message: j.error.message || '' } : null, expiresMicros: opt(j.expiresMicros),
  });
  const consumer = (c) => ({ namespace: c.namespace, user: c.user || '', client: c.client ?? null, nextSeq: n(c.nextSeq), lag: n(c.lag), lastPollMicros: n(c.lastPollMicros), polls: n(c.polls) });

  /* ---- the metrics (GET /v1/metrics, documentation/api/metrics.md) */
  const labelOf = (sample, name) => ((sample.labels || []).find((l) => l.name === name) || {}).value;
  const family = (m, name) => ((m.families || []).find((f) => f.name === name) || { samples: [] }).samples || [];
  const hist = (sample) => { const x = (sample && sample.histogram) || {}; return { bounds: x.boundsSeconds || [], counts: (x.counts || []).map(n) }; };
  /** The sum of histograms (same buckets), or of the difference of two (`minus`, counters only grow). */
  function addHist(into, h, sign = 1) {
    if (!into.bounds.length) into.bounds = h.bounds;
    h.counts.forEach((c, i) => { into.counts[i] = (into.counts[i] || 0) + sign * c; });
    return into;
  }
  /** The q-quantile of a histogram (counts per bucket, not cumulative; the last for +Inf), in ms: linear within
   *  its bucket, as Prometheus' histogram_quantile; the highest bound if it falls in +Inf; 0 without observations. */
  function quantile(h, q) {
    const total = h.counts.reduce((a, b) => a + Math.max(0, b), 0); if (!total) return 0;
    const rank = q * total; let seen = 0;
    for (let i = 0; i < h.counts.length; i++) {
      const c = Math.max(0, h.counts[i]);
      if (seen + c >= rank && c > 0) {
        if (i >= h.bounds.length) return h.bounds[h.bounds.length - 1] * 1000;
        const lo = i === 0 ? 0 : h.bounds[i - 1];
        return (lo + (h.bounds[i] - lo) * ((rank - seen) / c)) * 1000;
      }
      seen += c;
    }
    return h.bounds.length ? h.bounds[h.bounds.length - 1] * 1000 : 0;
  }
  const countOf = (h) => h.counts.reduce((a, b) => a + b, 0);
  /** What one metrics answer holds for the status page: cumulative histograms and the per-operation table. */
  function digest(m) {
    const queries = { bounds: [], counts: [] }; const byOp = new Map();
    family(m, 'iwdb_request_duration_seconds').forEach((s) => {
      const op = labelOf(s, 'operation'); const h = hist(s);
      byOp.set(op, { operation: op, calls: 0, errors: 0, hist: h });
      if (QUERY_OPS.has(op)) addHist(queries, h);
    });
    family(m, 'iwdb_requests_total').forEach((s) => {
      const op = labelOf(s, 'operation'); const e = byOp.get(op) || byOp.set(op, { operation: op, calls: 0, errors: 0, hist: { bounds: [], counts: [] } }).get(op);
      const c = n(s.counter); e.calls += c; if (labelOf(s, 'code') !== 'ok') e.errors += c;
    });
    const gauge = (name) => { const s = family(m, name)[0]; return s ? Number(s.gauge || 0) : 0; };
    return {
      commit: hist(family(m, 'iwdb_commit_duration_seconds')[0]), fsync: hist(family(m, 'iwdb_wal_fsync_duration_seconds')[0]), queries,
      active: gauge('iwdb_requests_active'),
      operations: [...byOp.values()].filter((o) => o.calls > 0).map((o) => ({ operation: o.operation, calls: o.calls, errors: o.errors, p50Ms: quantile(o.hist, 0.5), p99Ms: quantile(o.hist, 0.99) })),
    };
  }
  function plan(p) {
    if (!p) return { other: '?' };
    if (p.empty) return { empty: {} };
    if (p.label) return { label: { label: p.label.label } };
    if (p.index) return { index: { path: keys(p.index.path), op: { LOOKUP_POINT: 'Eq', LOOKUP_IN: 'In', LOOKUP_RANGE: 'Range' }[p.index.lookup] || 'lookup' } };
    if (p.union) return { union: { plans: (p.union.plans || []).map(plan) } };
    if (p.scan) return { scan: {} };
    return { other: p.other || JSON.stringify(p) };
  }
  /** Column names of a match, as `iwctl shell` names them: node variables, then edge variables. */
  function columns(text, row) {
    try {
      const p = root.IW.mock.parsePattern(text);
      return p.nodes.map((x, k) => ({ name: x.v || '_' + k, kind: 'node' })).concat(p.rels.map((x, k) => ({ name: x.v || '_e' + k, kind: 'edge' })));
    } catch (_) { // a pattern the console's parser doesn't know (several chains): positional names
      return (row ? row.nodes : []).map((_, k) => ({ name: 'n' + k, kind: 'node' })).concat((row ? row.edges || [] : []).map((_, k) => ({ name: 'e' + k, kind: 'edge' })));
    }
  }
  const ROLES = { ROLE_READ: 'read', ROLE_WRITE: 'write', ROLE_ADMIN: 'admin' };
  const user = (u = {}) => ({ name: u.name || '', admin: !!u.admin, grants: Object.fromEntries(Object.entries(u.grants || {}).map(([ns, r]) => [ns, ROLES[r] || r])) });
  const LEVELS = { LOG_LEVEL_TRACE: 'TRACE', LOG_LEVEL_DEBUG: 'DEBUG', LOG_LEVEL_INFO: 'INFO', LOG_LEVEL_WARN: 'WARN', LOG_LEVEL_ERROR: 'ERROR' };
  const clock = (d) => d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0');
  /** A server log event as a log line: the message, then its fields. */
  const logLine = (e) => ({
    t: clock(new Date(n(e.timeMicros) / 1000)), level: LEVELS[e.level] || 'INFO',
    msg: [e.message || ''].concat((e.fields || []).map((f) => f.name + '=' + f.value)).filter(Boolean).join(' · '),
  });

  /** A Source over the REST API at `base` ('' for the page's origin). `fetch` can be passed in for tests. */
  /** The status's memory (ADR 0054), with the enums by their short names. */
  const MEMORY_STATES = { MEMORY_STATE_NORMAL: 'normal', MEMORY_STATE_WARN: 'warn', MEMORY_STATE_REFUSING_WRITES: 'refusing_writes' };
  const LIMIT_SOURCES = { MEMORY_LIMIT_SOURCE_CONFIG: 'config', MEMORY_LIMIT_SOURCE_CGROUP_V2: 'cgroup v2', MEMORY_LIMIT_SOURCE_CGROUP_V1: 'cgroup v1' };
  const memoryOf = (m) => ({
    graphBytes: n(m.graphBytes), payloadBytes: n(m.payloadBytes), checkpointBytes: n(m.checkpointBytes), workingBytes: n(m.workingBytes),
    usedBytes: n(m.usedBytes), limitBytes: opt(m.limitBytes), warnBytes: opt(m.warnBytes), refuseWritesBytes: opt(m.refuseWritesBytes),
    state: MEMORY_STATES[m.state] || 'normal', limitSource: LIMIT_SOURCES[m.limitSource] || null,
  });

  function create(opts = {}) {
    const base = opts.base || '';
    const doFetch = opts.fetch || root.fetch.bind(root);
    // Two logs: this page's requests, and the server's once a server-wide admin's poll has read it
    const pageLog = []; const serverLog = []; const listeners = new Set(); const auth = new Set();
    let logKind = 'page'; let logAfter = 0; let logDenied = false;
    const tell = (e) => listeners.forEach((f) => { try { f(e); } catch (_) { /* its own */ } });
    const keep = (list, e) => { list.push(e); if (list.length > LOG_KEEP) list.shift(); };
    const emit = (level, msg) => { const e = { t: clock(new Date()), level, msg }; keep(pageLog, e); if (logKind === 'page') tell(e); };
    let config = null;
    let last = null; // the previous call's metrics: {at, commit, fsync, queries}
    const series = { commitsPerSec: [], commitP50: [], commitP99: [], fsyncP99: [], queryP50: [], queryP99: [], active: [], usedBytes: [], walBytes: [] };
    let tickMs = 3000;
    const push = (k, v) => { const a = series[k]; a.push(v); if (a.length > SERIES) a.shift(); };

    /** `quiet`: no line for a success; `expected`: statuses that are answers here, not failures to log. */
    async function call(method, path, body, quiet, expected = []) {
      const t0 = Date.now(); let res;
      try {
        const headers = { accept: 'application/json', 'x-iwdb-csrf': '1' };
        if (body !== undefined) headers['content-type'] = 'application/json';
        res = await doFetch(base + path, { method, headers, credentials: 'same-origin', body: body === undefined ? undefined : JSON.stringify(body) });
      } catch (e) {
        emit('ERROR', `${method} ${path} · no answer: ${e.message}`);
        throw new SourceError('unavailable', `no answer from ${base || 'the console server'}${path}: ${e.message}`);
      }
      const text = await res.text(); let json = null;
      try { json = text ? JSON.parse(text) : {}; } catch (_) { /* a proxy's page */ }
      const ms = Date.now() - t0;
      if (!res.ok) {
        const code = (json && json.code) || (res.status === 502 || res.status === 504 ? 'unavailable' : 'internal');
        const message = (json && json.message) || `HTTP ${res.status} from ${path}`;
        if (!expected.includes(res.status)) emit(res.status >= 500 ? 'ERROR' : 'WARN', `${method} ${path} · ${res.status} ${code} · ${ms} ms`);
        // The session ended (or there was none): the pages ask for a login, not report an outage
        if (res.status === 401 && path !== '/v1/auth/login') auth.forEach((f) => { try { f(null); } catch (_) { /* its own */ } });
        throw new SourceError(res.status === 401 ? 'unauthenticated' : code, message);
      }
      if (!quiet) emit('INFO', `${method} ${path} · ${res.status} · ${ms} ms`);
      return json;
    }
    const ns = (name) => '/v1/namespaces/' + enc(name);

    /** The server's new log events, if this caller may read them (a server-wide admin). */
    async function pollLog() {
      if (logDenied) return;
      try {
        const r = await call('GET', `/v1/log?after=${logAfter}&limit=1000`, undefined, true, [403]);
        const events = r.events || [];
        if (logKind === 'page') logKind = 'server';
        events.forEach((e) => { const line = logLine(e); keep(serverLog, line); tell(line); });
        logAfter = n(r.lastSeq) || logAfter;
      } catch (e) {
        if (e.code === 'permission_denied') logDenied = true; // stays this page's requests
      }
    }
    /** One step of the series: the differences to the previous call's metrics. */
    function step(d, status) {
      const now = Date.now();
      if (last) {
        const dt = Math.max(1, now - last.at) / 1000; tickMs = Math.round(dt * 1000);
        const delta = (a, b) => addHist(addHist({ bounds: [], counts: [] }, a), b, -1);
        const commits = delta(d.commit, last.commit); const fsyncs = delta(d.fsync, last.fsync); const queries = delta(d.queries, last.queries);
        push('commitsPerSec', countOf(commits) / dt);
        push('commitP50', quantile(commits, 0.5)); push('commitP99', quantile(commits, 0.99)); push('fsyncP99', quantile(fsyncs, 0.99));
        push('queryP50', quantile(queries, 0.5)); push('queryP99', quantile(queries, 0.99));
        push('active', d.active); push('usedBytes', n(status.memory && status.memory.usedBytes)); push('walBytes', n(status.disk && status.disk.walBytes));
      }
      last = { at: now, commit: d.commit, fsync: d.fsync, queries: d.queries };
    }

    const src = {
      kind: 'rest',
      namespaces: async () => ((await call('GET', '/v1/namespaces')).namespaces || []).map((x) => ({ id: n(x.id), name: x.name, createdMicros: n(x.createdMicros), createdSeq: n(x.createdSeq) })),
      namespaceStatus: async (name) => status((await call('GET', ns(name))).status),
      /** Labels with their exact counts; keys and edge types from a sample (step 16c, ADR 0053). */
      schema: async (name) => {
        const [r, cat] = await Promise.all([call('GET', ns(name) + `/schema?max_visited=${SAMPLE_NODES}&max_edges=${SAMPLE_EDGES}`), call('GET', ns(name) + '/catalog')]);
        const s = r.schema || {};
        const c = (cat.catalog && cat.catalog.constraints) || [];
        return {
          labels: (s.labels || []).map((l) => ({
            name: l.name, count: n(l.count), sampled: n(l.sampled), moreKeys: !!l.moreKeys,
            keys: Object.fromEntries((l.keys || []).map((k) => [k.name, Object.fromEntries((k.kinds || []).map((x) => [x.kind, n(x.count)]))])),
          })),
          types: (s.types || []).map((t) => ({ name: t.name ?? null, count: n(t.count) })),
          constraints: c.map((x) => ({ kind: x.kind === 'CONSTRAINT_KIND_REQUIRED' ? 'required' : 'unique', label: x.label, path: keys(x.path) })),
          nodes: n(s.nodes), edges: n(s.edges), sampledNodes: n(s.sampledNodes), sampledEdges: n(s.sampledEdges),
        };
      },
      getNodes: async (name, ids) => { const r = await call('POST', ns(name) + '/get-nodes', { ids }); return { nodes: (r.nodes || []).map((m) => (m.node ? node(m.node) : null)), meta: meta(r.meta) }; },
      getEdges: async (name, ids) => { const r = await call('POST', ns(name) + '/get-edges', { ids }); return { edges: (r.edges || []).map((m) => (m.edge ? edge(m.edge) : null)), meta: meta(r.meta) }; },
      find: async (name, filter, o = {}) => {
        const options = { limits: { maxResults: o.limit || 100 } }; if (o.cursor) options.cursor = o.cursor;
        const r = await call('POST', ns(name) + '/find', { filter, options });
        return { nodes: (r.nodes || []).map(node), meta: meta(r.meta) };
      },
      explain: async (name, filter, o = {}) => {
        const r = await call('POST', ns(name) + '/explain', { filter, analyze: !!o.analyze }); const e = r.explain || {};
        return { explain: { plan: plan(e.plan), estimatedCandidates: n(e.estimatedCandidates), candidates: opt(e.candidates) ?? undefined, nodes: n(e.nodes), building: (e.building || []).map(keys) }, meta: meta(r.meta) };
      },
      /** A node's neighbours (either direction) and the edges to them: a depth-1 subgraph, bounded. */
      neighbours: async (name, id, o = {}) => {
        const limit = o.limit || 24;
        const r = await call('POST', ns(name) + '/subgraph', { seeds: [id], depth: 1, direction: 'DIRECTION_BOTH', options: { limits: { maxVisited: VISIT }, partial: true } });
        const nodes = (r.nodes || []).map(node); if (!nodes.some((x) => x.id === id)) throw new SourceError('not_found', `no node ${JSON.stringify(id)}`);
        const edges = (r.edges || []).map(edge).filter((e) => e.from === id || e.to === id).sort((a, b) => a.id - b.id);
        const order = []; edges.forEach((e) => { const other = e.from === id ? e.to : e.from; if (other !== id && !order.includes(other)) order.push(other); });
        const keep = new Set(order.slice(0, limit)); const by = new Map(nodes.map((x) => [x.id, x]));
        return { nodes: [...keep].map((x) => by.get(x)).filter(Boolean), edges: edges.filter((e) => keep.has(e.from === id ? e.to : e.from)), truncated: order.length > limit || !!(r.meta && r.meta.truncated), meta: meta(r.meta) };
      },
      subgraph: async (name, ids) => {
        if (!ids.length) return { edges: [], meta: meta() };
        const r = await call('POST', ns(name) + '/subgraph', { seeds: ids, depth: 0 }); return { edges: (r.edges || []).map(edge), meta: meta(r.meta) };
      },
      matchPattern: async (name, text, o = {}) => {
        const r = await call('POST', ns(name) + '/match', { pattern: text, options: { limits: { maxResults: o.limit || 200 }, partial: true } });
        const rows = (r.rows || []).map((x) => ({ nodes: x.nodes || [], edges: (x.edges || []).map((p) => (p.ids || []).map(Number)) }));
        const m = meta(r.meta); m.truncated = m.truncated || !!m.next;
        return { columns: columns(text, r.rows && r.rows[0]), rows, meta: m };
      },
      commit: async (name, mutations) => { const r = (await call('POST', ns(name) + '/commit', { mutations })).result || {}; return { seq: n(r.seq), edgeIds: (r.edgeIds || []).map(Number), timeMicros: n(r.timeMicros) }; },
      createIndex: async (name, path) => { const r = (await call('POST', ns(name) + '/catalog', { change: { createIndex: { path: { keys: path } } } })).result || {}; return { seq: n(r.seq) }; },
      /** The server's status views (step 16c): status, running requests, change-stream readers, metrics. */
      server: async () => {
        // Relative to the page: the proxy serves it at /, the server at /console/ (ADR 0041)
        if (!config) {
          config = await doFetch(base ? base + '/console-config.json' : 'console-config.json').then((r) => (r.ok ? r.json() : {}), () => ({}));
          if (config && config.upstream) src.endpoint = config.upstream;
        }
        const [st, rq, co, m, jb] = await Promise.all([
          call('GET', '/v1/status', undefined, true), call('GET', '/v1/requests?limit=100', undefined, true),
          call('GET', '/v1/consumers', undefined, true), call('GET', '/v1/metrics', undefined, true),
          call('GET', '/v1/jobs?limit=50', undefined, true),
        ]);
        await pollLog();
        const s = st.status || {}; const d = digest(m); step(d, s);
        const r = s.requests || {};
        return {
          version: s.version || null, startedMicros: n(s.startedMicros), ready: !!s.ready, fsync: s.fsync || null,
          memory: memoryOf(s.memory || {}),
          disk: { walBytes: n(s.disk && s.disk.walBytes), checkpointBytes: n(s.disk && s.disk.checkpointBytes), freeBytes: opt(s.disk && s.disk.freeBytes) },
          requests: { active: n(r.active), total: n(r.total), timedOut: n(r.timedOut), cancelled: n(r.cancelled), rejected: n(r.rejected), denied: n(r.denied) },
          namespaces: (s.namespaces || []).map(status),
          active: (rq.requests || []).map(request), consumers: (co.consumers || []).map(consumer),
          jobs: (jb.jobs || []).map(job),
          jobCounts: { queued: n(s.jobs && s.jobs.queued), running: n(s.jobs && s.jobs.running), finished: n(s.jobs && s.jobs.finished), resultBytes: n(s.jobs && s.jobs.resultBytes) },
          operations: d.operations, series: JSON.parse(JSON.stringify(series)), tickMs,
        };
      },
      /** Cancel a running read (commits can't be: `invalid_argument`); its caller gets `cancelled`. */
      cancel: async (id) => {
        const r = await call('POST', `/v1/requests/${enc(String(id))}/cancel`, {});
        return { request: request(r.request || {}) };
      },
      /** Cancel a queued or running job (step 16f); one that ended is answered as it is. */
      cancelJob: async (id) => {
        const r = await call('POST', `/v1/jobs/${enc(String(id))}/cancel`, {});
        return { job: job(r.job || {}) };
      },
      log: () => (logKind === 'server' ? serverLog : pageLog).slice(),
      onLog: (f) => { listeners.add(f); return () => listeners.delete(f); },
      logKind: () => logKind,
      tick: () => {},
      /** Who the server takes this page for: {authEnabled, user}; rejects `unauthenticated` without a session. */
      session: async () => {
        const r = await call('GET', '/v1/auth/whoami', undefined, true);
        return { authEnabled: !!r.authEnabled, user: user(r.user) };
      },
      /** Log in: the server sets the session cookie (the answer holds no token). */
      login: async (name, password) => {
        const r = await call('POST', '/v1/auth/login', { user: name, password, cookie: true });
        return { authEnabled: true, user: user(r.user) };
      },
      logout: async () => { await call('POST', '/v1/auth/logout', {}); auth.forEach((f) => { try { f(null); } catch (_) { /* its own */ } }); return {}; },
      onAuth: (f) => { auth.add(f); return () => auth.delete(f); },
    };
    // Where it reads from: this origin (the server's /console/), or the proxy's server once server() read it
    src.endpoint = base || 'this origin';
    emit('INFO', `REST source on ${base || 'this origin'}`);
    return src;
  }

  const api = { create, SourceError, columns, plan, status, quantile, digest };
  root.IW = root.IW || {}; root.IW.rest = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
