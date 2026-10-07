/* Ironweaver DB operator console: the mock Source (step 16a).
 *
 * A Source is everything the console reads and writes (the contract is in source.js). This one keeps a few
 * small, generated namespaces in memory and simulates a running server: background commits, checkpoints,
 * requests, change-stream consumers and the metrics the status page plots. Nothing here talks to a server.
 *
 * Shapes follow the REST API (documentation/api/rest.md): camelCase fields, attributes as the core's value
 * JSON ({"Int": 30}), filters as the core's Expr JSON, patterns as the core's text, mutations as the bodies of
 * `commit`. 64-bit numbers are plain JS numbers here; the REST source converts.
 *
 * Classic script (pages open from file://): defines globalThis.IW.mock, and module.exports for node --test.
 */
(function (root) {
  'use strict';

  /* ------------------------------------------------------------------ utils */
  function prng(seed) { // mulberry32: the same data on every load
    let a = seed >>> 0;
    return function () {
      a = (a + 0x6d2b79f5) >>> 0; let t = a;
      t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }
  const pick = (r, a) => a[Math.floor(r() * a.length)];
  const between = (r, lo, hi) => lo + Math.floor(r() * (hi - lo + 1));
  const clone = (v) => JSON.parse(JSON.stringify(v));
  const dec = (x) => { const v = Math.round(x * 100) / 100; return Number.isInteger(v) ? v + 0.5 : v; }; // a Float, two decimals

  class SourceError extends Error {
    constructor(code, message) { super(message); this.code = code; }
  }
  const fail = (code, msg) => { throw new SourceError(code, msg); };

  /* The core's value JSON <-> JS, for the mock's own comparisons. */
  function plain(v) {
    if (v === 'None' || v == null) return null;
    const k = Object.keys(v)[0];
    if (k === 'List') return v.List.map(plain);
    if (k === 'Dict') return Object.fromEntries(Object.entries(v.Dict).map(([a, b]) => [a, plain(b)]));
    return v[k];
  }
  function valueOf(x) {
    if (x === null) return 'None';
    if (typeof x === 'boolean') return { Bool: x };
    if (typeof x === 'number') return Number.isInteger(x) ? { Int: x } : { Float: x };
    if (Array.isArray(x)) return { List: x.map(valueOf) };
    if (typeof x === 'object') return { Dict: Object.fromEntries(Object.entries(x).sort().map(([a, b]) => [a, valueOf(b)])) };
    return { String: String(x) };
  }
  const kindOf = (v) => (v === 'None' || v == null ? 'None' : Object.keys(v)[0]);
  function at(attr, path) {
    let v = attr[path[0]];
    for (const p of path.slice(1)) { if (!v || kindOf(v) !== 'Dict') return undefined; v = v.Dict[p]; }
    return v;
  }
  function cmp(a, b) { // the core orders Int and Float together; other kinds only among themselves
    const ka = kindOf(a), kb = kindOf(b);
    const num = (k) => k === 'Int' || k === 'Float';
    if (!(ka === kb || (num(ka) && num(kb)))) return null;
    const x = plain(a), y = plain(b);
    return x < y ? -1 : x > y ? 1 : 0;
  }

  /* ------------------------------------------------------------------ filters (the core's Expr JSON) */
  const FILTER_KEYS = ['Label', 'Type', 'Compare', 'In', 'Exists', 'And', 'Or', 'Not', 'Const'];
  function checkFilter(f, depth = 0) {
    if (depth > 32) fail('invalid_argument', 'filter nested deeper than 32');
    if (!f || typeof f !== 'object' || Array.isArray(f)) fail('invalid_argument', 'a filter is an object like {"Label": "Person"}');
    const keys = Object.keys(f);
    if (keys.length !== 1 || !FILTER_KEYS.includes(keys[0])) fail('invalid_argument', `unknown filter ${JSON.stringify(keys)}; expected one of ${FILTER_KEYS.join(', ')}`);
    const k = keys[0], b = f[k];
    if (k === 'And' || k === 'Or') { if (!Array.isArray(b)) fail('invalid_argument', k + ' takes a list'); b.forEach((x) => checkFilter(x, depth + 1)); }
    if (k === 'Not') checkFilter(b, depth + 1);
    if ((k === 'Compare' || k === 'In' || k === 'Exists') && !(b && Array.isArray(b.path))) fail('invalid_argument', k + ' needs a "path" list');
    if (k === 'Compare' && !['Eq', 'Ne', 'Lt', 'Le', 'Gt', 'Ge'].includes(b.op)) fail('invalid_argument', 'Compare "op" is one of Eq, Ne, Lt, Le, Gt, Ge');
  }
  function test(f, n) {
    const k = Object.keys(f)[0], b = f[k];
    switch (k) {
      case 'Const': return !!b;
      case 'Label': return n.labels.includes(b);
      case 'Type': return n.type === b;
      case 'Exists': return at(n.attr, b.path) !== undefined;
      case 'In': { const v = at(n.attr, b.path); return v !== undefined && b.values.some((x) => cmp(v, x) === 0); }
      case 'Compare': {
        const v = at(n.attr, b.path); if (v === undefined) return false;
        const c = cmp(v, b.value); if (c === null) return b.op === 'Ne';
        return { Eq: c === 0, Ne: c !== 0, Lt: c < 0, Le: c <= 0, Gt: c > 0, Ge: c >= 0 }[b.op];
      }
      case 'And': return b.every((x) => test(x, n));
      case 'Or': return b.some((x) => test(x, n));
      case 'Not': return !test(b, n);
    }
    return false;
  }

  /* ------------------------------------------------------------------ patterns (the core's text, chains only) */
  function parsePattern(src) {
    let i = 0; const s = src.trim();
    const err = (m) => fail('invalid_argument', `pattern: ${m} at ${i + 1}: ${s.slice(Math.max(0, i - 8), i + 12)}`);
    const ws = () => { while (i < s.length && /\s/.test(s[i])) i++; };
    const ident = () => { ws(); const m = /^[A-Za-z_][A-Za-z0-9_]*/.exec(s.slice(i)); if (!m) return null; i += m[0].length; return m[0]; };
    const eat = (t) => { ws(); if (s.startsWith(t, i)) { i += t.length; return true; } return false; };
    const literal = () => {
      ws(); const rest = s.slice(i); let m;
      if ((m = /^-?\d+\.\d+/.exec(rest))) { i += m[0].length; return { Float: parseFloat(m[0]) }; }
      if ((m = /^-?\d+/.exec(rest))) { i += m[0].length; return { Int: parseInt(m[0], 10) }; }
      if ((m = /^'((?:[^'\\]|\\.)*)'|^"((?:[^"\\]|\\.)*)"/.exec(rest))) { i += m[0].length; return { String: (m[1] ?? m[2]).replace(/\\(.)/g, '$1') }; }
      if (rest.startsWith('true')) { i += 4; return { Bool: true }; }
      if (rest.startsWith('false')) { i += 5; return { Bool: false }; }
      return err('expected a number, a quoted string, true or false');
    };
    const props = () => {
      const out = {}; if (!eat('{')) return out;
      if (eat('}')) return out;
      do { const k = ident(); if (!k) err('expected a key'); if (!eat(':')) err("expected ':'"); out[k] = literal(); } while (eat(','));
      if (!eat('}')) err("expected '}'");
      return out;
    };
    const node = () => {
      if (!eat('(')) err("expected '('");
      const v = ident(); const labels = [];
      while (eat(':')) { const l = ident(); if (!l) err('expected a label'); labels.push(l); }
      const p = props(); if (!eat(')')) err("expected ')'");
      return { v, labels, props: p };
    };
    const rel = () => {
      ws(); let dir;
      if (eat('<-')) dir = 'in'; else if (eat('-')) dir = 'out'; else return null;
      const r = { v: null, types: [], min: 1, max: 1, props: {} };
      if (eat('[')) {
        r.v = ident();
        if (eat(':')) { do { const t = ident(); if (!t) err('expected a type'); r.types.push(t); } while (eat('|')); }
        if (eat('*')) {
          ws(); const m = /^(\d+)?(?:\.\.(\d+)?)?/.exec(s.slice(i)); i += m[0].length;
          r.min = m[1] ? +m[1] : 1; r.max = m[2] ? +m[2] : m[0].includes('..') || !m[1] ? 4 : r.min;
          if (r.max > 4) err('variable-length edges go at most 4 deep in the mock');
        }
        r.props = props(); if (!eat(']')) err("expected ']'");
      }
      if (dir === 'in') { if (!eat('-')) err("expected '-'"); }
      else if (eat('->')) dir = 'out'; else if (eat('-')) dir = 'both'; else err("expected '->' or '-'");
      r.dir = dir; return r;
    };
    const nodes = [node()], rels = [];
    for (;;) { const r = rel(); if (!r) break; rels.push(r); nodes.push(node()); }
    ws(); if (i < s.length) err(s[i] === ',' ? 'the mock matches one chain; several patterns are not supported' : 'unexpected text');
    return { nodes, rels };
  }

  /* ------------------------------------------------------------------ datasets */
  const FIRST = ['ann', 'bob', 'carol', 'dave', 'erin', 'frank', 'grace', 'heidi', 'ivan', 'judy', 'mallory', 'niaj', 'olivia', 'peggy', 'rupert', 'sybil', 'trent', 'uma', 'victor', 'wendy', 'xena', 'yusuf', 'zoe', 'amir', 'bea', 'chen', 'dana', 'emil', 'fatima', 'goran'];
  const LAST = ['Okafor', 'Lindqvist', 'Moreau', 'Tanaka', 'Novak', 'Schmidt', 'Silva', 'Haddad', 'Kowalski', 'Ivanova', 'Byrne', 'Rossi', 'Nakamura', 'Fischer', 'Duarte'];
  const CITIES = [['berlin', 'Berlin', 'DE', 3755251], ['lisbon', 'Lisbon', 'PT', 545923], ['osaka', 'Osaka', 'JP', 2752412], ['lagos', 'Lagos', 'NG', 15388000], ['montreal', 'Montreal', 'CA', 1762949], ['krakow', 'Kraków', 'PL', 803282], ['lyon', 'Lyon', 'FR', 522250], ['porto', 'Porto', 'PT', 231800], ['seoul', 'Seoul', 'KR', 9411000], ['vienna', 'Vienna', 'AT', 1982097]];
  const COMPANIES = [['acme', 'Acme Rail', 1952, 'transport'], ['helio', 'Helio Grid', 2011, 'energy'], ['quill', 'Quill & Ink', 1987, 'publishing'], ['nordlys', 'Nordlys Bio', 2016, 'health'], ['tessel', 'Tessel Works', 2003, 'manufacturing'], ['ferro', 'Ferro Logistics', 1979, 'transport'], ['mosaic', 'Mosaic Labs', 2019, 'software'], ['basalt', 'Basalt Insurance', 1931, 'finance']];

  function emptyNs(id, name, created) {
    return { id, name, created, createdSeq: id * 3 - 2, nodes: new Map(), edges: new Map(), nextEdge: 0, seq: 0, synced: 0, checkpoint: null, readOnly: null, checkpointFailure: null, indexes: [], constraints: [], marks: [], recovery: { checkpoint: null, replayed: 0, seq: 0 }, lastCheckpointAgoS: null };
  }
  function put(ns, id, labels, attr) { ns.nodes.set(id, { id, labels: labels.slice().sort(), attr: Object.fromEntries(Object.entries(attr).filter(([, v]) => v !== undefined).map(([k, v]) => [k, valueOf(v)])), meta: {}, version: 1 }); }
  function link(ns, from, to, type, attr = {}) { const id = ns.nextEdge++; ns.edges.set(id, { id, from, to, type, attr: Object.fromEntries(Object.entries(attr).map(([k, v]) => [k, valueOf(v)])), meta: {}, version: 1 }); return id; }

  function social() {
    const r = prng(7); const ns = emptyNs(1, 'social', Date.UTC(2026, 6, 2, 9, 12) * 1000);
    CITIES.forEach(([id, name, country, population]) => put(ns, id, ['City'], { name, country, population }));
    COMPANIES.forEach(([id, name, founded, sector]) => put(ns, id, ['Company'], { name, founded, sector }));
    const people = [];
    for (let k = 0; k < 64; k++) {
      const first = FIRST[k % FIRST.length]; const id = k < FIRST.length ? first : first + '-' + (k + 1);
      const name = first[0].toUpperCase() + first.slice(1) + ' ' + pick(r, LAST);
      const age = between(r, 19, 71);
      put(ns, id, ['Person'], { name, age, email: r() < 0.85 ? id.replace('-', '.') + '@example.org' : undefined, joined: undefined, score: dec(r() * 100), tags: r() < 0.3 ? [pick(r, ['admin', 'beta', 'staff', 'vip'])] : undefined });
      const n = ns.nodes.get(id); n.attr.joined = { Date: `202${between(r, 2, 6)}-0${between(r, 1, 9)}-1${between(r, 0, 9)}` };
      people.push(id);
    }
    people.forEach((p, k) => {
      link(ns, p, pick(r, CITIES)[0], 'LIVES_IN', { since: between(r, 2001, 2026) });
      if (r() < 0.8) link(ns, p, pick(r, COMPANIES)[0], 'WORKS_AT', { role: pick(r, ['engineer', 'analyst', 'manager', 'designer', 'operator']) });
      const n = between(r, 1, 4);
      for (let j = 0; j < n; j++) { const q = people[(k + between(r, 1, 63)) % people.length]; if (q !== p) link(ns, p, q, 'KNOWS', { weight: dec(r()) }); }
    });
    ns.nodes.get('ann').labels = ['Admin', 'Person'];
    ns.indexes = [{ path: ['email'], declared: false, unique: true }, { path: ['age'], declared: true, unique: false }, { path: ['name'], declared: true, unique: false }];
    ns.constraints = [{ kind: 'unique', label: 'Person', path: ['email'] }];
    ns.seq = 4120; ns.synced = 4120; ns.checkpoint = 4096; ns.lastCheckpointAgoS = 312; ns.recovery = { checkpoint: 4032, replayed: 61, seq: 4093 };
    return ns;
  }

  function inventory() {
    const r = prng(11); const ns = emptyNs(2, 'inventory', Date.UTC(2026, 7, 14, 15, 40) * 1000);
    const wh = ['wh-hamburg', 'wh-rotterdam', 'wh-gdansk', 'wh-valencia'];
    wh.forEach((id) => put(ns, id, ['Warehouse'], { name: id.slice(3)[0].toUpperCase() + id.slice(4), capacity: between(r, 20, 90) * 1000 }));
    const sup = [];
    for (let k = 0; k < 12; k++) { const id = 'sup-' + String(k + 1).padStart(2, '0'); sup.push(id); put(ns, id, ['Supplier'], { name: pick(r, ['Arc', 'Brass', 'Cobalt', 'Delta', 'Ember', 'Flint', 'Gauge', 'Hollow']) + ' ' + pick(r, ['Metals', 'Plastics', 'Components', 'Optics', 'Textiles']), country: pick(r, ['DE', 'PL', 'CN', 'VN', 'MX', 'TR']), rating: between(r, 1, 5) }); }
    const parts = [];
    for (let k = 0; k < 70; k++) { const id = 'part-' + String(1000 + k * 7); parts.push(id); put(ns, id, ['Part'], { sku: 'P' + (1000 + k * 7), name: pick(r, ['bolt', 'gasket', 'bearing', 'hinge', 'panel', 'lens', 'spring', 'bracket', 'cable', 'valve']) + ' ' + pick(r, ['M4', 'M6', 'S', 'L', 'XL', 'A2', 'B7']), weight_g: between(r, 2, 900), unit_cost: dec(r() * 40) }); link(ns, id, pick(r, sup), 'SUPPLIED_BY', { lead_days: between(r, 3, 60) }); }
    for (let k = 0; k < 36; k++) {
      const id = 'prod-' + String(k + 1).padStart(3, '0');
      put(ns, id, ['Product'], { sku: 'X' + (200 + k), name: pick(r, ['Desk lamp', 'Bike rack', 'Shelf', 'Kettle', 'Router', 'Stool', 'Speaker', 'Planter', 'Clock']) + ' ' + pick(r, ['One', 'Two', 'Pro', 'Mini', 'Max']), price: dec(r() * 300), active: r() < 0.9 });
      const n = between(r, 2, 6); for (let j = 0; j < n; j++) link(ns, id, pick(r, parts), 'CONTAINS', { qty: between(r, 1, 12) });
      link(ns, id, pick(r, wh), 'STOCKED_IN', { units: between(r, 0, 4000) });
    }
    ns.indexes = [{ path: ['sku'], declared: false, unique: true }, { path: ['price'], declared: true, unique: false }];
    ns.constraints = [{ kind: 'unique', label: 'Part', path: ['sku'] }, { kind: 'unique', label: 'Product', path: ['sku'] }];
    ns.seq = 911; ns.synced = 911; ns.checkpoint = 896; ns.lastCheckpointAgoS = 4210; ns.recovery = { checkpoint: 896, replayed: 0, seq: 896 };
    return ns;
  }

  function orders() {
    const r = prng(23); const ns = emptyNs(3, 'orders', Date.UTC(2026, 8, 1, 6, 0) * 1000);
    for (let k = 0; k < 30; k++) put(ns, 'cust-' + (k + 1), ['Customer'], { name: pick(r, FIRST) + ' ' + pick(r, LAST), tier: pick(r, ['free', 'plus', 'pro']) });
    ns.orderN = 0;
    for (let k = 0; k < 120; k++) addOrder(ns, r);
    ns.indexes = [{ path: ['status'], declared: true, unique: false }];
    ns.marks = [{ name: 'kafka:orders.v2', position: 88214, seq: 0 }];
    ns.seq = 18230; ns.synced = 18230; ns.checkpoint = 18176; ns.marks[0].seq = ns.seq; ns.recovery = { checkpoint: 18048, replayed: 128, seq: 18176 };
    return ns;
  }
  function addOrder(ns, r) {
    const id = 'ord-' + String(50000 + ns.orderN++);
    const placed = new Date(Date.UTC(2026, 9, 4, 6) - (300 - ns.orderN) * 97000).toISOString().slice(0, 19) + 'Z';
    put(ns, id, ['Order'], { total: dec(r() * 500), status: pick(r, ['placed', 'paid', 'paid', 'shipped', 'delivered']), items: between(r, 1, 9) });
    ns.nodes.get(id).attr.placed = { DateTime: placed };
    link(ns, 'cust-' + between(r, 1, 30), id, 'PLACED');
    return id;
  }

  function archive() {
    const r = prng(31); const ns = emptyNs(4, 'archive_2025', Date.UTC(2025, 0, 2, 0, 0) * 1000);
    for (let k = 0; k < 24; k++) put(ns, 'evt-' + (k + 1), ['Event'], { kind: pick(r, ['login', 'export', 'grant', 'revoke']), at: undefined });
    for (let k = 1; k < 24; k++) link(ns, 'evt-' + k, 'evt-' + (k + 1), 'NEXT');
    ns.seq = 2400; ns.synced = 2400; ns.checkpoint = 2400; ns.lastCheckpointAgoS = 86400 * 40; ns.recovery = { checkpoint: 2400, replayed: 0, seq: 2400 };
    return ns;
  }

  /* ------------------------------------------------------------------ the mock server */
  // Operations by their RPC names, as the server's metrics label them
  const OPS = ['GetNodes', 'Find', 'Explain', 'Neighbourhood', 'Traverse', 'ShortestPath', 'Subgraph', 'MatchPattern', 'Commit', 'GetChanges'];
  const BASE_P50 = { GetNodes: 0.18, Find: 1.4, Explain: 0.12, Neighbourhood: 0.9, Traverse: 2.6, ShortestPath: 3.1, Subgraph: 1.1, MatchPattern: 4.8, Commit: 1.9, GetChanges: 0.3 };

  /**
   * A mock Source. `scenario`: 'calm' (default) or 'degraded' (a read-only namespace, a failed checkpoint and
   * memory above the warning line, to see how the console shows problems). `latency`: [min, max] ms per call.
   */
  function create(opts = {}) {
    const scenario = opts.scenario || 'calm';
    // The logged-in mock user, per tab (opts.storage for tests)
    const storage = opts.storage !== undefined ? opts.storage : (() => { try { return root.sessionStorage || null; } catch (_) { return null; } })();
    let memory = null;
    const session = {
      get: () => { try { return storage ? storage.getItem('iwdb.mock.user') : memory; } catch (_) { return memory; } },
      set: (v) => { memory = v; try { if (storage) { if (v) storage.setItem('iwdb.mock.user', v); else storage.removeItem('iwdb.mock.user'); } } catch (_) { /* private mode */ } },
    };
    const authListeners = new Set();
    const latency = opts.latency || [8, 40];
    const r = prng(99);
    const spaces = [social(), inventory(), orders(), archive()];
    const byName = new Map(spaces.map((s) => [s.name, s]));
    const t0 = opts.now || Date.now();
    const server = {
      version: '0.3.0-dev', startedMicros: (t0 - 3 * 86400e3 - 4 * 3600e3 - 17 * 60e3) * 1000, fsync: 'always',
      ticks: 0, series: {}, opStats: Object.fromEntries(OPS.map((o) => [o, { calls: between(r, 2000, 90000), errors: between(r, 0, 12), p50: BASE_P50[o], p99: BASE_P50[o] * 6.5 }])),
      total: 412000, rejected: 3, timedOut: 11, cancelled: 2, denied: 5, active: [], nextReq: 4100, consumers: [], log: [], listeners: new Set(),
      walBytes: 182 * 2 ** 20, checkpointBytes: 61 * 2 ** 20, diskFree: 212 * 2 ** 30,
    };
    // The mock's clock: a simulated second per tick
    const nowMicros = () => (t0 + server.ticks * 1000) * 1000;
    if (scenario === 'degraded') {
      const a = byName.get('archive_2025'); a.readOnly = 'a WAL write failed: No space left on device (os error 28)'; a.synced = 2398;
      const o = byName.get('orders'); o.checkpointFailure = 'writing checkpoint 18176: Permission denied (os error 13)';
      server.diskFree = 3.1 * 2 ** 30;
    }
    server.consumers = [
      { namespace: 'social', user: 'search-indexer', client: '10.4.2.17', nextSeq: 4119, lastPollTicks: 0, polls: 81233 },
      { namespace: 'orders', user: 'warehouse-sync', client: '10.4.2.31', nextSeq: 18191, lastPollTicks: 0, polls: 40210 },
    ];
    // Managed analytics jobs (step 16f, ADR 0056): their own PRNG, so the rest of the traffic stays as it was
    const rj = prng(7);
    server.jobs = [
      { id: 4031, namespace: 'social', user: 'reporting', kind: 'page_rank', state: 'done', createdTicks: -900, startedTicks: -890, endedTicks: -420, nodes: 0, edges: 0, rows: 1000, truncated: true },
      { id: 4077, namespace: 'orders', user: 'reporting', kind: 'leiden', state: 'running', createdTicks: -300, startedTicks: -295, endedTicks: null, leftTicks: 200 },
      { id: 4098, namespace: 'social', user: 'admin', kind: 'triangles', state: 'queued', createdTicks: -5, startedTicks: null, endedTicks: null, leftTicks: 40 },
    ];
    server.jobsDone = 41; server.jobsFailed = 2; server.jobsCancelled = 3;
    const JOB_RETENTION_TICKS = 3600;
    const jobResultBytes = (j) => (j.state === 'done' ? j.rows * 40 : 0);
    const jobWorking = (j) => (j.state === 'running' || j.state === 'collecting' ? 48 * (j.nodes || 0) + 16 * (j.edges || 0) : 0);
    // A build in progress, so the status page shows one (social: an index on ["joined"])
    byName.get('social').indexes.push({ path: ['joined'], declared: true, unique: false, building: { scanned: 4, total: byName.get('social').nodes.size } });

    const series = ['commitsPerSec', 'commitP50', 'commitP99', 'fsyncP99', 'queryP50', 'queryP99', 'active', 'usedBytes', 'walBytes'];
    series.forEach((k) => { server.series[k] = []; });
    const emit = (level, msg) => {
      const d = new Date(); const e = { t: d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0'), level, msg };
      server.log.push(e); if (server.log.length > 200) server.log.shift();
      server.listeners.forEach((f) => { try { f(e); } catch (_) { /* a listener's error is its own */ } });
    };
    const memoryOf = (ns) => {
      let b = 0; ns.nodes.forEach((n) => { b += 96 + n.id.length + 24 * n.labels.length; });
      return b + ns.edges.size * 64 + ns.indexes.length * ns.nodes.size * 40;
    };
    const graphBytes = () => spaces.reduce((a, s) => a + memoryOf(s), 0);
    // The server's memory (step 16d, ADR 0054): the payloads (their JSON's length and a map each), the checkpointers'
    // copies of both, index builds while they run; a limit from the cgroup, its lines and the state with its band
    const payloadOf = (ns) => {
      let b = 0; const add = (x) => { b += 120 + JSON.stringify(x.attr).length; };
      ns.nodes.forEach(add); ns.edges.forEach(add); return b;
    };
    const memoryParts = () => {
      const graph = graphBytes(); const payload = spaces.reduce((a, s) => a + payloadOf(s), 0);
      let working = 0; spaces.forEach((s) => s.indexes.forEach((ix) => { if (ix.building) working += 8 * ix.building.total + 96 * ix.building.scanned; }));
      server.jobs.forEach((j) => { working += jobResultBytes(j) + jobWorking(j); });
      return { graph, payload, checkpoint: graph + payload, working };
    };
    const usedBytes = () => { const p = memoryParts(); return p.graph + p.payload + p.checkpoint + p.working; };
    const limit = { bytes: 4 * 2 ** 20, source: 'cgroup v2', warnAt: 0.8, refuseAt: 0.9, band: 0.05 };
    let memState = 'normal';
    const memoryNow = () => {
      const used = usedBytes(); const at = (f) => f * limit.bytes;
      const rising = used >= at(limit.refuseAt) ? 'refusing_writes' : used >= at(limit.warnAt) ? 'warn' : 'normal';
      const rank = { normal: 0, warn: 1, refusing_writes: 2 };
      if (rank[rising] >= rank[memState]) memState = rising;
      else if (memState === 'refusing_writes' && used >= at(limit.refuseAt - limit.band)) memState = 'refusing_writes';
      else if (memState !== 'normal' && used >= at(limit.warnAt - limit.band)) memState = 'warn';
      else memState = 'normal';
      const p = memoryParts();
      return {
        graphBytes: p.graph, payloadBytes: p.payload, checkpointBytes: p.checkpoint, workingBytes: p.working, usedBytes: used,
        limitBytes: limit.bytes, warnBytes: Math.floor(at(limit.warnAt)), refuseWritesBytes: Math.floor(at(limit.refuseAt)),
        state: memState, limitSource: limit.source,
      };
    };
    /** Writes that add are refused above the line, before anything changes (as on the server). */
    const admit = () => {
      const m = memoryNow();
      if (m.state === 'refusing_writes') fail('resource_exhausted', `memory limit: writes are refused above ${m.refuseWritesBytes} bytes (${m.usedBytes} of ${m.limitBytes} bytes in use); deletes and drops are accepted`);
    };

    /* One simulated second of traffic; the status page calls tick() on its timer, the tests call it directly. */
    function tick() {
      server.ticks++;
      const o = byName.get('orders');
      const commits = Math.max(0, Math.round(14 + 6 * Math.sin(server.ticks / 7) + r() * 6));
      if (!o.readOnly && memoryNow().state !== 'refusing_writes') {
        for (let k = 0; k < Math.min(commits, 3); k++) { addOrder(o, r); }
        o.seq += commits; o.synced = o.seq; o.marks[0].position += commits * 3; o.marks[0].seq = o.seq;
      }
      spaces.forEach((s) => {
        if (s.readOnly || s.checkpointFailure) return;
        if (s.seq - (s.checkpoint || 0) > 256) { s.checkpoint = s.seq; s.lastCheckpointAgoS = 0; emit('INFO', `checkpoint ${s.name} · seq ${s.seq} · ${(18 + r() * 30).toFixed(1)} ms`); } else if (s.lastCheckpointAgoS != null) s.lastCheckpointAgoS++;
      });
      if (o.checkpointFailure && server.ticks % 15 === 0) emit('ERROR', `checkpoint orders failed · ${o.checkpointFailure}`);
      const spike = r() < 0.06 ? 3 + r() * 5 : 1;
      const p50 = 1.6 + r() * 0.5, p99 = (6 + r() * 3) * spike;
      if (spike > 1) emit('WARN', `slow commit · orders · ${p99.toFixed(1)} ms (p99 budget 10 ms)`);
      const push = (k, v) => { const a = server.series[k]; a.push(v); if (a.length > 90) a.shift(); };
      push('commitsPerSec', commits); push('commitP50', p50); push('commitP99', p99); push('fsyncP99', p99 * 0.62);
      push('queryP50', 1.1 + r() * 0.6); push('queryP99', 9 + r() * 6 * spike); push('usedBytes', usedBytes());
      server.walBytes += commits * 380; if (server.ticks % 40 === 0) server.walBytes = Math.round(server.walBytes * 0.6);
      push('walBytes', server.walBytes);
      OPS.forEach((op) => { const st = server.opStats[op]; const n = between(r, 0, op === 'Commit' ? commits : 20); st.calls += n; server.total += n; st.p50 = BASE_P50[op] * (0.85 + r() * 0.3); st.p99 = st.p50 * (5 + r() * 3) * (op === 'Commit' ? spike : 1); });
      // Requests come and go
      server.active = server.active.filter((q) => (q.leftTicks -= 1) > 0);
      while (server.active.length < 2 + Math.floor(r() * 4)) {
        const op = pick(r, ['Find', 'MatchPattern', 'Traverse', 'Neighbourhood', 'GetChanges', 'Subgraph', 'Commit']);
        server.active.push({ id: server.nextReq++, operation: op, namespace: pick(r, ['social', 'inventory', 'orders']), user: pick(r, ['admin', 'search-indexer', 'reporting']), client: pick(r, ['10.4.2.17', '10.4.2.31', '10.4.3.8', '127.0.0.1']), startedTicks: server.ticks, leftTicks: between(r, 1, op === 'GetChanges' ? 30 : 6) });
      }
      push('active', server.active.length);
      if (r() < 0.02) { server.timedOut++; emit('WARN', 'request timed out · MatchPattern · social · 30 s'); }
      server.consumers.forEach((c) => { const ns = byName.get(c.namespace); c.nextSeq = Math.min(ns.seq + 1, c.nextSeq + Math.round(commits * (0.7 + r() * 0.5))); c.polls++; c.lastPollTicks = server.ticks; });
      spaces.forEach((s) => s.indexes.forEach((ix) => {
        if (!ix.building) return;
        if (server.ticks % 3) return; // an online build reads in batches between commits (ADR 0019)
        ix.building.scanned = Math.min(ix.building.total, ix.building.scanned + 1);
        if (ix.building.scanned >= ix.building.total) { delete ix.building; s.seq++; s.synced = s.seq; emit('INFO', `index ${s.name} [${ix.path.join('.')}] ready`); }
      }));
      // Jobs: at most two run, the queue moves up, a new one now and then; ended ones are kept for an hour
      server.jobs.forEach((j) => {
        if (j.state === 'running' && (j.leftTicks -= 1) <= 0) {
          j.state = 'done'; j.endedTicks = server.ticks; j.rows = Math.min(1000, j.nodes); j.truncated = j.nodes > 1000; server.jobsDone++;
          emit('INFO', `job ${j.id} done · ${j.kind} · ${j.namespace} · ${j.rows} rows`);
        }
      });
      server.jobs.filter((j) => j.state === 'queued').forEach((j) => {
        if (server.jobs.filter((x) => x.state === 'running').length >= 2) return;
        const ns = byName.get(j.namespace); j.state = 'running'; j.startedTicks = server.ticks; j.nodes = ns.nodes.size; j.edges = ns.edges.size;
      });
      if (rj() < 0.03 && server.jobs.filter((j) => !jobEnded(j)).length < 6) {
        server.jobs.push({ id: server.nextReq++, namespace: pick(rj, ['social', 'orders', 'inventory']), user: pick(rj, ['reporting', 'admin']), kind: pick(rj, ['page_rank', 'weakly_connected_components', 'leiden', 'core_number']), state: 'queued', createdTicks: server.ticks, startedTicks: null, endedTicks: null, leftTicks: between(rj, 20, 400) });
      }
      server.jobs = server.jobs.filter((j) => !jobEnded(j) || server.ticks - j.endedTicks < JOB_RETENTION_TICKS).slice(-20);
      if (server.ticks % 6 === 0) emit('INFO', `commit ${o.name} · seq ${o.seq} · ${commits} commits/s · mark ${o.marks[0].name} at ${o.marks[0].position}`);
    }
    // Sizes of the jobs that started before the first look
    server.jobs.forEach((j) => { if (j.startedTicks != null) { const ns = byName.get(j.namespace); j.nodes = ns.nodes.size; j.edges = ns.edges.size; } });
    for (let k = 0; k < 90; k++) tick(); // a minute and a half of history before the first look
    // Degraded: just above the warning line, as the first look finds it
    if (scenario === 'degraded') { limit.bytes = Math.round(usedBytes() / 0.82); memState = 'warn'; }
    server.log = [];
    emit('INFO', 'recovery done · 4 namespaces · 190 records replayed · 212 ms');
    emit('INFO', 'serving 0.0.0.0:7600 · gRPC, REST, health and the console');
    if (scenario === 'degraded') emit('ERROR', 'archive_2025 is read-only · a WAL write failed: No space left on device (os error 28)');

    /* ------------------------------------------------------------------ the Source */
    const wait = () => new Promise((res) => setTimeout(res, latency[0] + Math.random() * (latency[1] - latency[0])));
    const nsOf = (name) => byName.get(name) || fail('not_found', `no namespace ${JSON.stringify(name)}`);
    const nodeOut = (n) => clone(n);
    const edgeOut = (e) => clone(e);
    const meta = (ns, extra = {}) => ({ seq: ns.seq, next: '', truncated: false, work: { visited: 0, edges: 0 }, ...extra });
    const call = async (op, f) => {
      await wait(); const t = performance.now();
      try { return f(); } catch (e) { server.opStats[op] && server.opStats[op].errors++; throw e; } finally {
        const st = server.opStats[op]; if (st) { st.calls++; st.p50 = st.p50 * 0.95 + (performance.now() - t + 0.1) * 0.05; }
      }
    };
    const pageOf = (items, limit, cursor) => {
      const from = cursor ? parseInt(cursor, 36) : 0;
      if (cursor && !(from >= 0)) fail('invalid_argument', 'not a cursor of this read');
      const slice = items.slice(from, from + limit);
      return { slice, next: from + limit < items.length ? (from + limit).toString(36) : '' };
    };
    const sortedNodes = (ns) => [...ns.nodes.values()].sort((a, b) => (a.id < b.id ? -1 : 1));

    const requestOut = (q) => ({
      id: q.id, operation: q.operation, namespace: q.namespace, user: q.user, client: q.client, startedMicros: nowMicros() - (server.ticks - q.startedTicks) * 1e6 - 120e3,
      elapsedMicros: (server.ticks - q.startedTicks) * 1e6 + 120e3, cancellable: q.operation !== 'Commit',
    });

    function jobEnded(j) { return !['queued', 'collecting', 'running'].includes(j.state); }
    const tickMicros = (t) => (t == null ? null : nowMicros() - (server.ticks - t) * 1e6);
    const jobOut = (j) => ({
      id: j.id, namespace: j.namespace, user: j.user, kind: j.kind, state: j.state,
      createdMicros: tickMicros(j.createdTicks), startedMicros: tickMicros(j.startedTicks), endedMicros: tickMicros(j.endedTicks),
      elapsedMicros: j.startedTicks == null ? 0 : ((j.endedTicks ?? server.ticks) - j.startedTicks) * 1e6,
      nodes: j.startedTicks == null ? null : j.nodes, edges: j.startedTicks == null ? null : j.edges,
      rows: j.state === 'done' ? j.rows : null, truncated: j.state === 'done' && !!j.truncated, resultBytes: jobResultBytes(j),
      error: j.error ? { ...j.error } : null, expiresMicros: j.endedTicks == null ? null : tickMicros(j.endedTicks) + JOB_RETENTION_TICKS * 1e6,
    });

    function statusOf(ns) {
      return {
        id: ns.id, name: ns.name, createdMicros: ns.created, seq: ns.seq, syncedSeq: ns.synced, checkpoint: ns.checkpoint,
        unsynced: ns.seq - ns.synced, sinceCheckpoint: ns.seq - (ns.checkpoint || 0),
        lastCheckpointMicros: ns.lastCheckpointAgoS == null ? null : nowMicros() - ns.lastCheckpointAgoS * 1e6,
        readOnly: ns.readOnly, checkpointFailure: ns.checkpointFailure, nodes: ns.nodes.size, edges: ns.edges.size, memoryBytes: memoryOf(ns),
        indexes: ns.indexes.slice().sort((a, b) => (a.path.join('.') < b.path.join('.') ? -1 : 1)).map((ix) => {
          const out = { path: ix.path, declared: ix.declared, unique: ix.unique };
          if (ix.building) out.building = { ...ix.building };
          else {
            const vals = [...ns.nodes.values()].map((n) => at(n.attr, ix.path)).filter((v) => v !== undefined && !['List', 'Dict'].includes(kindOf(v)));
            out.ready = {}; out.size = { entries: vals.length, distinctKeys: new Set(vals.map((v) => JSON.stringify(v))).size, memoryBytes: vals.length * 48 };
          }
          return out;
        }),
        constraints: ns.constraints.length, marks: clone(ns.marks), recovery: clone(ns.recovery),
      };
    }

    function matchIn(ns, text, limit) {
      const p = parsePattern(text);
      const nodeVars = p.nodes.map((n, k) => n.v || '_' + k);
      const edgeVars = p.rels.map((e, k) => e.v || '_e' + k);
      const dup = [...nodeVars, ...edgeVars].find((v, k, a) => a.indexOf(v) !== k);
      if (dup) fail('invalid_argument', `pattern: variable ${dup} is used twice (the mock doesn't join on variables)`);
      const okNode = (pn, n) => pn.labels.every((l) => n.labels.includes(l)) && Object.entries(pn.props).every(([k, v]) => n.attr[k] !== undefined && cmp(n.attr[k], v) === 0);
      const okEdge = (pe, e) => (!pe.types.length || pe.types.includes(e.type)) && Object.entries(pe.props).every(([k, v]) => e.attr[k] !== undefined && cmp(e.attr[k], v) === 0);
      const out = new Map(), inn = new Map();
      ns.edges.forEach((e) => { (out.get(e.from) || out.set(e.from, []).get(e.from)).push(e); (inn.get(e.to) || inn.set(e.to, []).get(e.to)).push(e); });
      const steps = (id, dir) => (dir === 'out' ? (out.get(id) || []).map((e) => [e, e.to]) : dir === 'in' ? (inn.get(id) || []).map((e) => [e, e.from]) : (out.get(id) || []).map((e) => [e, e.to]).concat((inn.get(id) || []).map((e) => [e, e.from])));
      // One more than the limit: if it is found, there are more (truncated)
      const rows = []; let visited = 0, walked = 0; const want = limit + 1;
      const walk = (k, ids, paths) => {
        if (rows.length >= want) return;
        if (k === p.rels.length) { rows.push({ nodes: ids.slice(), edges: paths.map((x) => x.slice()) }); return; }
        const pe = p.rels[k], pn = p.nodes[k + 1];
        const go = (id, path, depth, used) => {
          if (depth >= pe.min) { const n = ns.nodes.get(id); visited++; if (okNode(pn, n)) { ids.push(id); paths.push(path); walk(k + 1, ids, paths); ids.pop(); paths.pop(); } }
          if (depth >= pe.max) return;
          for (const [e, to] of steps(id, pe.dir)) { walked++; if (used.has(e.id) || !okEdge(pe, e)) continue; used.add(e.id); path.push(e.id); go(to, path, depth + 1, used); path.pop(); used.delete(e.id); if (rows.length >= want) return; }
        };
        go(ids[ids.length - 1], [], 0, new Set(paths.flat()));
      };
      for (const n of sortedNodes(ns)) { visited++; if (okNode(p.nodes[0], n)) walk(0, [n.id], []); if (rows.length >= want) break; }
      const truncated = rows.length > limit; rows.length = Math.min(rows.length, limit);
      return { columns: nodeVars.map((name) => ({ name, kind: 'node' })).concat(edgeVars.map((name) => ({ name, kind: 'edge' }))), rows, meta: meta(ns, { truncated, work: { visited, edges: walked } }) };
    }

    function planOf(ns, f) {
      const k = Object.keys(f)[0], b = f[k];
      const indexed = (path) => ns.indexes.find((ix) => !ix.building && ix.path.join('.') === path.join('.'));
      if (k === 'Const' && b === false) return { empty: {} };
      if (k === 'Label') return { label: { label: b } };
      if ((k === 'Compare' && b.op !== 'Ne') || k === 'In') { const ix = indexed(b.path); if (ix) return { index: { path: b.path, op: k === 'In' ? 'In' : b.op } }; }
      if (k === 'And') { const plans = b.map((x) => planOf(ns, x)).filter((x) => !x.scan); if (plans.length) return plans.find((x) => x.index) || plans[0]; }
      if (k === 'Or') { const plans = b.map((x) => planOf(ns, x)); if (!plans.some((x) => x.scan)) return { union: { plans } }; }
      return { scan: {} };
    }
    function candidates(ns, plan) {
      const all = [...ns.nodes.values()];
      if (plan.empty) return [];
      if (plan.label) return all.filter((n) => n.labels.includes(plan.label.label));
      if (plan.index) return all.filter((n) => at(n.attr, plan.index.path) !== undefined);
      if (plan.union) return [...new Set(plan.union.plans.flatMap((x) => candidates(ns, x)))];
      return all;
    }
    function buildingPaths(ns, f) {
      const paths = []; const walkF = (x) => { const k = Object.keys(x)[0], b = x[k]; if (b && b.path) paths.push(b.path); if (Array.isArray(b)) b.forEach(walkF); if (k === 'Not') walkF(b); };
      walkF(f);
      return paths.filter((p) => ns.indexes.some((ix) => ix.building && ix.path.join('.') === p.join('.')));
    }

    function applyMutations(ns, mutations) {
      if (ns.readOnly) fail('read_only', `namespace ${ns.name} is read-only: ${ns.readOnly}`);
      if (!Array.isArray(mutations) || !mutations.length) fail('invalid_argument', 'a commit needs at least one mutation');
      // All or nothing: apply to copies, then swap
      const nodes = new Map([...ns.nodes].map(([k, v]) => [k, clone(v)])); const edges = new Map([...ns.edges].map(([k, v]) => [k, clone(v)]));
      let nextEdge = ns.nextEdge; const edgeIds = [];
      const node = (id) => nodes.get(id) || fail('not_found', `no node ${JSON.stringify(id)}`);
      for (const m of mutations) {
        const [k] = Object.keys(m); const b = m[k];
        if (k === 'upsertNode') { const old = nodes.get(b.id); nodes.set(b.id, { id: b.id, labels: (b.labels || []).slice().sort(), attr: clone(b.attr || {}), meta: clone(b.meta || {}), version: old ? old.version + 1 : 1 }); }
        else if (k === 'deleteNode') { node(b.id); nodes.delete(b.id); [...edges.values()].forEach((e) => { if (e.from === b.id || e.to === b.id) edges.delete(e.id); }); }
        else if (k === 'addEdge') { node(b.from); node(b.to); const id = nextEdge++; edges.set(id, { id, from: b.from, to: b.to, type: b.type, attr: clone(b.attr || {}), meta: {}, version: 1 }); edgeIds.push(id); }
        else if (k === 'deleteEdge') { if (!edges.delete(b.id)) fail('not_found', `no edge ${b.id}`); }
        else if (k === 'setAttr' || k === 'removeAttr') {
          const t = b.target.node != null ? node(b.target.node) : edges.get(b.target.edge) || fail('not_found', `no edge ${b.target.edge}`);
          if (k === 'setAttr') t.attr[b.key] = clone(b.value); else delete t.attr[b.key];
          t.version++;
        } else fail('invalid_argument', `unknown mutation ${JSON.stringify(k)}`);
      }
      // Unique constraints hold after the commit
      for (const c of ns.constraints) {
        const seen = new Map();
        nodes.forEach((n) => { if (!n.labels.includes(c.label)) return; const v = at(n.attr, c.path); if (v === undefined) return; const key = JSON.stringify(v); if (seen.has(key)) fail('constraint_violation', `unique ${c.label}.${c.path.join('.')}: ${seen.get(key)} and ${n.id} have ${plain(v)}`); seen.set(key, n.id); });
      }
      ns.nodes = nodes; ns.edges = edges; ns.nextEdge = nextEdge; ns.seq++; ns.synced = ns.seq;
      return { seq: ns.seq, edgeIds, timeMicros: Date.now() * 1000 };
    }

    return {
      kind: 'mock', scenario, endpoint: 'mock data',
      /** Namespaces, by name. */
      namespaces: () => call('ListNamespaces', () => spaces.map((s) => ({ id: s.id, name: s.name, createdMicros: s.created, createdSeq: s.createdSeq }))),
      namespaceStatus: (name) => call('GetNamespaceStatus', () => statusOf(nsOf(name))),
      /** Labels and edge types with counts, and the attribute keys (with value kinds) seen per label. The mock's
       *  namespaces are small: its sample is every node and edge. */
      schema: (name) => call('GetSchema', () => {
        const ns = nsOf(name); const labels = new Map(); const types = new Map();
        ns.nodes.forEach((n) => n.labels.forEach((l) => {
          const e = labels.get(l) || labels.set(l, { name: l, count: 0, sampled: 0, keys: {}, moreKeys: false }).get(l); e.count++; e.sampled++;
          Object.entries(n.attr).forEach(([k, v]) => { const kinds = e.keys[k] || (e.keys[k] = {}); kinds[kindOf(v)] = (kinds[kindOf(v)] || 0) + 1; });
        }));
        ns.edges.forEach((e) => { const t = types.get(e.type) || types.set(e.type, { name: e.type, count: 0 }).get(e.type); t.count++; });
        const byName2 = (a, b) => (a.name < b.name ? -1 : 1);
        return {
          labels: [...labels.values()].sort(byName2), types: [...types.values()].sort(byName2), constraints: clone(ns.constraints),
          nodes: ns.nodes.size, edges: ns.edges.size, sampledNodes: ns.nodes.size, sampledEdges: ns.edges.size,
        };
      }),
      getNodes: (name, ids) => call('GetNodes', () => { const ns = nsOf(name); return { nodes: ids.map((id) => (ns.nodes.has(id) ? nodeOut(ns.nodes.get(id)) : null)), meta: meta(ns) }; }),
      find: (name, filter, o = {}) => call('Find', () => {
        const ns = nsOf(name); checkFilter(filter);
        const plan = planOf(ns, filter); const cands = candidates(ns, plan).sort((a, b) => (a.id < b.id ? -1 : 1));
        const hits = cands.filter((n) => test(filter, n));
        const { slice, next } = pageOf(hits, Math.min(o.limit || 100, 10000), o.cursor);
        return { nodes: slice.map(nodeOut), meta: meta(ns, { next, work: { visited: cands.length, edges: 0 } }) };
      }),
      explain: (name, filter, o = {}) => call('Explain', () => {
        const ns = nsOf(name); checkFilter(filter); const plan = planOf(ns, filter);
        const cands = candidates(ns, plan);
        const est = plan.scan ? ns.nodes.size : Math.max(1, Math.round(cands.length * (0.8 + 0.4 * ((cands.length * 7) % 10) / 10)));
        return { explain: { plan, estimatedCandidates: est, candidates: o.analyze ? cands.length : undefined, nodes: ns.nodes.size, building: buildingPaths(ns, filter) }, meta: meta(ns, { work: { visited: o.analyze ? cands.length : 0, edges: 0 } }) };
      }),
      /** A node's neighbours (either direction) and the edges between the node and them (the REST Source reads
       *  a depth-1 `subgraph`). */
      neighbours: (name, id, o = {}) => call('Subgraph', () => {
        const ns = nsOf(name); if (!ns.nodes.has(id)) fail('not_found', `no node ${JSON.stringify(id)}`);
        const limit = o.limit || 24; const edges = [...ns.edges.values()].filter((e) => e.from === id || e.to === id).sort((a, b) => a.id - b.id);
        const ids = []; edges.forEach((e) => { const other = e.from === id ? e.to : e.from; if (!ids.includes(other)) ids.push(other); });
        const keep = new Set(ids.slice(0, limit));
        return { nodes: [...keep].map((x) => nodeOut(ns.nodes.get(x))), edges: edges.filter((e) => keep.has(e.from === id ? e.to : e.from)).map(edgeOut), truncated: ids.length > limit, meta: meta(ns) };
      }),
      /** The edges among a set of nodes (the induced subgraph). */
      subgraph: (name, ids) => call('Subgraph', () => { const ns = nsOf(name); const s = new Set(ids); return { edges: [...ns.edges.values()].filter((e) => s.has(e.from) && s.has(e.to)).map(edgeOut), meta: meta(ns) }; }),
      getEdges: (name, ids) => call('GetEdges', () => { const ns = nsOf(name); return { edges: ids.map((id) => (ns.edges.has(id) ? edgeOut(ns.edges.get(id)) : null)), meta: meta(ns) }; }),
      matchPattern: (name, pattern, o = {}) => call('MatchPattern', () => matchIn(nsOf(name), pattern, Math.min(o.limit || 200, 10000))),
      commit: (name, mutations) => call('Commit', () => { if (!mutations.every((m) => m.deleteNode || m.deleteEdge || m.removeAttr || m.removeLabel)) admit(); const res = applyMutations(nsOf(name), mutations); emit('INFO', `commit ${name} · seq ${res.seq} · ${mutations.length} mutation${mutations.length > 1 ? 's' : ''}`); return res; }),
      /** Catalog operations; the mock knows createIndex. */
      createIndex: (name, path) => call('CommitCatalog', () => {
        const ns = nsOf(name); if (ns.readOnly) fail('read_only', `namespace ${name} is read-only: ${ns.readOnly}`);
        admit();
        if (ns.indexes.some((ix) => ix.path.join('.') === path.join('.'))) fail('conflict', `an index on [${path.join(', ')}] exists`);
        ns.indexes.push({ path, declared: true, unique: false, building: { scanned: 0, total: ns.nodes.size } }); ns.seq++;
        emit('INFO', `index ${name} [${path.join('.')}] building · online (ADR 0019)`);
        return { seq: ns.seq };
      }),
      /** What the server reports about itself: the shape of the REST Source's (step 16c, source.js). */
      server: () => call('GetServerStatus', () => ({
        version: server.version, startedMicros: server.startedMicros, ready: true, fsync: server.fsync,
        memory: memoryNow(),
        disk: { walBytes: server.walBytes, checkpointBytes: server.checkpointBytes, freeBytes: server.diskFree },
        requests: { active: server.active.length, total: server.total, timedOut: server.timedOut, cancelled: server.cancelled, rejected: server.rejected, denied: server.denied },
        namespaces: spaces.map(statusOf),
        active: server.active.map(requestOut),
        jobs: server.jobs.slice().reverse().map(jobOut),
        jobCounts: {
          queued: server.jobs.filter((j) => j.state === 'queued').length, running: server.jobs.filter((j) => j.state === 'running').length,
          finished: server.jobs.filter(jobEnded).length, resultBytes: server.jobs.reduce((a, j) => a + jobResultBytes(j), 0),
        },
        consumers: server.consumers.map((c) => ({ namespace: c.namespace, user: c.user, client: c.client, nextSeq: c.nextSeq, lag: Math.max(0, byName.get(c.namespace).seq + 1 - c.nextSeq), lastPollMicros: nowMicros() - (server.ticks - c.lastPollTicks) * 1e6, polls: c.polls })),
        operations: OPS.map((op) => ({ operation: op, calls: server.opStats[op].calls, errors: server.opStats[op].errors, p50Ms: server.opStats[op].p50, p99Ms: server.opStats[op].p99 })),
        series: clone(server.series), tickMs: 1000,
      })),
      /** Cancel a running read; commits can't be (as on the server). */
      cancel: (id) => call('CancelRequest', () => {
        const k = server.active.findIndex((q) => q.id === id); if (k < 0) fail('not_found', `no running request ${id}`);
        const q = server.active[k];
        if (q.operation === 'Commit') fail('invalid_argument', `request ${id} is a commit: cancelling it would only make its outcome unknown`);
        server.active.splice(k, 1); server.cancelled++; emit('WARN', `request cancelled · id=${q.id} · operation=${q.operation} · namespace=${q.namespace}`);
        return { request: requestOut(q) };
      }),
      /** Cancel a queued or running job (step 16f); one that ended is answered as it is (as on the server). */
      cancelJob: (id) => call('CancelJob', () => {
        const j = server.jobs.find((x) => x.id === id); if (!j) fail('not_found', `no job ${id}`);
        if (!jobEnded(j)) {
          j.state = 'cancelled'; j.endedTicks = server.ticks; j.error = { code: 'cancelled', message: 'the job was cancelled (CancelJob)' }; server.jobsCancelled++;
          emit('WARN', `job cancelled · id=${j.id} · ${j.kind} · namespace=${j.namespace}`);
        }
        return { job: jobOut(j) };
      }),
      log: () => server.log.slice(),
      onLog: (f) => { server.listeners.add(f); return () => server.listeners.delete(f); },
      logKind: () => 'server',
      tick,
      /** The mock's login (step 15a): MOCK_USERS, remembered for the tab (sessionStorage; a mock flag, not a
       *  credential). Data calls don't check it: the pages show the login until session() answers. */
      session: () => call('whoami', () => {
        const name = session.get(); const u = name && MOCK_USERS[name];
        if (!u) fail('unauthenticated', 'log in: this mock knows ' + Object.keys(MOCK_USERS).map((k) => `${k} / ${MOCK_USERS[k].password}`).join(', '));
        return { authEnabled: true, user: { name, admin: u.admin, grants: clone(u.grants) } };
      }),
      login: (name, password) => call('login', () => {
        const u = MOCK_USERS[name];
        if (!u || u.password !== password) { emit('WARN', `failed login for user ${JSON.stringify(name)}`); fail('unauthenticated', 'wrong user or password'); }
        session.set(name); emit('INFO', `user ${JSON.stringify(name)} logged in`);
        return { authEnabled: true, user: { name, admin: u.admin, grants: clone(u.grants) } };
      }),
      logout: () => call('logout', () => { session.set(null); authListeners.forEach((f) => f(null)); return {}; }),
      onAuth: (f) => { authListeners.add(f); return () => authListeners.delete(f); },
    };
  }

  /** The mock's users: name -> {password, admin, grants}. */
  const MOCK_USERS = {
    admin: { password: 'admin', admin: true, grants: {} },
    reader: { password: 'reader', admin: false, grants: { social: 'read' } },
  };

  const api = { create, parsePattern, checkFilter, test, plain, valueOf, kindOf, SourceError, MOCK_USERS };
  root.IW = root.IW || {}; root.IW.mock = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
