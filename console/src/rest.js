/* Ironweaver DB operator console: the REST Source (step 16a). The Source contract (source.js) over the server's
 * REST API (documentation/api/rest.md), on the page's own origin: `console/serve.py` serves the pages and passes
 * /v1/... through to the server, because the server answers no CORS preflight (on purpose).
 *
 * It turns the proto3 JSON of the answers into the contract's shapes: 64-bit numbers (strings) into numbers,
 * absent defaults (edge id 0, empty lists, false) into values, AttrPath {keys} into lists.
 *
 * What the server can't answer yet (step 16): `schema` is sampled from the first SAMPLE nodes; `server` has the
 * namespaces' status and no metrics; `cancel` fails with `unavailable`; the log is this page's own requests.
 * Classic script: defines globalThis.IW.rest (and module.exports for node --test). */
(function (root) {
  'use strict';

  const SAMPLE = 2000;           // nodes `schema` reads to count labels and keys
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
      lastCheckpointMs: null,
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
  const unavailable = (what) => new SourceError('unavailable', `${what} needs the server's status views (step 16)`);

  /** A Source over the REST API at `base` ('' for the page's origin). `fetch` can be passed in for tests. */
  function create(opts = {}) {
    const base = opts.base || '';
    const doFetch = opts.fetch || root.fetch.bind(root);
    const log = []; const listeners = new Set();
    const emit = (level, msg) => {
      const d = new Date(); const e = { t: d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0'), level, msg };
      log.push(e); if (log.length > 200) log.shift(); listeners.forEach((f) => { try { f(e); } catch (_) { /* its own */ } });
    };
    let config = null;

    async function call(method, path, body, quiet) {
      const t0 = Date.now(); let res;
      try {
        res = await doFetch(base + path, { method, headers: body === undefined ? { accept: 'application/json' } : { 'content-type': 'application/json', accept: 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
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
        emit(res.status >= 500 ? 'ERROR' : 'WARN', `${method} ${path} · ${res.status} ${code} · ${ms} ms`);
        throw new SourceError(code, message);
      }
      if (!quiet) emit('INFO', `${method} ${path} · ${res.status} · ${ms} ms`);
      return json;
    }
    const ns = (name) => '/v1/namespaces/' + enc(name);

    const src = {
      kind: 'rest',
      namespaces: async () => ((await call('GET', '/v1/namespaces')).namespaces || []).map((x) => ({ id: n(x.id), name: x.name, createdMicros: n(x.createdMicros), createdSeq: n(x.createdSeq) })),
      namespaceStatus: async (name) => status((await call('GET', ns(name))).status),
      /** Labels, edge types and keys of the first SAMPLE nodes (exact when the namespace is smaller): the server
       *  has no such read yet. */
      schema: async (name) => {
        const [found, cat] = await Promise.all([call('POST', ns(name) + '/find', { filter: { Const: true }, options: { limits: { maxResults: SAMPLE } } }), call('GET', ns(name) + '/catalog')]);
        const nodes = (found.nodes || []).map(node);
        const sub = nodes.length ? await call('POST', ns(name) + '/subgraph', { seeds: nodes.map((x) => x.id), depth: 0 }) : { edges: [] };
        const labels = new Map(); const types = new Map();
        nodes.forEach((x) => x.labels.forEach((l) => {
          const e = labels.get(l) || labels.set(l, { name: l, count: 0, keys: {} }).get(l); e.count++;
          Object.entries(x.attr).forEach(([k, v]) => { const kind = v === 'None' ? 'None' : Object.keys(v)[0]; const kk = e.keys[k] || (e.keys[k] = {}); kk[kind] = (kk[kind] || 0) + 1; });
        }));
        (sub.edges || []).forEach((e) => { const t = e.type || '(untyped)'; const x = types.get(t) || types.set(t, { name: t, count: 0 }).get(t); x.count++; });
        const byName = (a, b) => (a.name < b.name ? -1 : 1);
        const c = (cat.catalog && cat.catalog.constraints) || [];
        return {
          labels: [...labels.values()].sort(byName), types: [...types.values()].sort(byName),
          constraints: c.map((x) => ({ kind: x.kind === 'CONSTRAINT_KIND_REQUIRED' ? 'required' : 'unique', label: x.label, path: keys(x.path) })),
          sampled: !!(found.meta && found.meta.next), sample: nodes.length,
        };
      },
      getNodes: async (name, ids) => { const r = await call('POST', ns(name) + '/get-nodes', { ids }); return { nodes: (r.nodes || []).map((m) => (m.node ? node(m.node) : null)), meta: meta(r.meta) }; },
      getEdges: async (name, ids) => { const r = await call('POST', ns(name) + '/get-edges', { ids }); return { edges: (r.edges || []).map((m) => (m.edge ? edge(m.edge) : null)), meta: meta(r.meta) }; },
      find: async (name, filter, o = {}) => {
        const options = { limits: { maxResults: o.limit || 100 } }; if (o.cursor) options.cursor = o.cursor;
        const r = await call('POST', ns(name) + '/find', { filter, options });
        return { nodes: (r.nodes || []).map(node), total: undefined, meta: meta(r.meta) };
      },
      explain: async (name, filter, o = {}) => {
        const r = await call('POST', ns(name) + '/explain', { filter, analyze: !!o.analyze }); const e = r.explain || {};
        return { explain: { plan: plan(e.plan), estimatedCandidates: n(e.estimatedCandidates), candidates: opt(e.candidates) ?? undefined, matched: undefined, nodes: n(e.nodes), building: (e.building || []).map(keys) }, meta: meta(r.meta) };
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
      /** What the server reports: its readiness and every namespace's status; the metrics come with step 16c. */
      server: async () => {
        // Relative to the page: the proxy serves it at /, the server at /console/ (ADR 0041)
        if (!config) config = await doFetch(base ? base + '/console-config.json' : 'console-config.json').then((r) => (r.ok ? r.json() : {}), () => ({}));
        // Readiness (step 16b): 503 is an answer here, not a failure
        const health = await doFetch(base + '/v1/health/ready', { method: 'GET', headers: { accept: 'application/json' } }).then((r) => r.json().catch(() => ({})), () => ({}));
        const list = await call('GET', '/v1/namespaces', undefined, true);
        const spaces = await Promise.all((list.namespaces || []).map((x) => call('GET', ns(x.name), undefined, true).then((r) => status(r.status))));
        const problems = spaces.some((s) => s.readOnly || s.checkpointFailure);
        return {
          version: config.version || null, startedMicros: null, ready: health.ready === true, health: problems || health.ready !== true ? 'warn' : 'ok', fsync: null,
          endpoints: { rest: config.upstream || base || 'this origin' }, dataDir: null,
          memory: null, disk: null, requests: null, series: null, tickMs: null,
          operations: null, active: null, consumers: null, jobs: null, namespaces: spaces, partial: true,
        };
      },
      cancel: async () => { throw unavailable('cancelling a request'); },
      log: () => log.slice(),
      onLog: (f) => { listeners.add(f); return () => listeners.delete(f); },
      tick: () => {},
    };
    emit('INFO', `REST source on ${base || 'this origin'}`);
    return src;
  }

  const api = { create, SourceError, columns, plan, status };
  root.IW = root.IW || {}; root.IW.rest = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
