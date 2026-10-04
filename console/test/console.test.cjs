// Tests of the operator console's logic (step 16a): the mock Source, the query line and the shared helpers.
// Run with `npm test` (node --test); no dependencies.
'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const mock = require('../src/mock.js');
const Q = require('../src/query.js');
const U = require('../src/shared.js');
const { METHODS } = require('../src/source.js');

const fresh = (o = {}) => mock.create({ latency: [0, 0], now: Date.UTC(2026, 9, 4, 12), ...o });
const rejects = async (p, code) => { await assert.rejects(p, (e) => { assert.equal(e.code, code, e.message); return true; }); };

test('the mock implements every method of the Source contract, and nothing the contract lacks', () => {
  const s = fresh();
  for (const m of METHODS) assert.equal(typeof s[m], 'function', m);
  const extra = Object.keys(s).filter((k) => typeof s[k] === 'function' && !METHODS.includes(k));
  assert.deepEqual(extra, []);
});

test('the pages call the Source only through methods of the contract', () => {
  for (const f of ['explorer.js', 'status.js']) {
    const text = fs.readFileSync(path.join(__dirname, '..', 'src', f), 'utf8');
    const used = [...text.matchAll(/\bsrc\.([A-Za-z]+)\(/g)].map((m) => m[1]);
    assert.ok(used.length > 3, f);
    for (const m of used) assert.ok(METHODS.includes(m), `${f} calls src.${m}, which is not in source.js`);
  }
});

test('every file a page loads exists', () => {
  for (const page of ['index.html', 'status.html']) {
    const html = fs.readFileSync(path.join(__dirname, '..', page), 'utf8');
    const refs = [...html.matchAll(/(?:src|href)="([^"]+)"/g)].map((m) => m[1]);
    assert.ok(refs.length >= 8, page);
    for (const r of refs) assert.ok(fs.existsSync(path.join(__dirname, '..', r)), `${page} loads ${r}`);
  }
});

test('the same data on every load', async () => {
  const a = await fresh().find('social', { Label: 'Person' }, { limit: 5 });
  const b = await fresh().find('social', { Label: 'Person' }, { limit: 5 });
  assert.deepEqual(a.nodes, b.nodes);
});

test('filters: the core\'s Expr JSON', async () => {
  const s = fresh();
  const all = (await s.find('social', { Label: 'Person' }, { limit: 1000 })).nodes;
  const old = await s.find('social', { And: [{ Label: 'Person' }, { Compare: { path: ['age'], op: 'Ge', value: { Int: 60 } } }] }, { limit: 1000 });
  assert.equal(old.total, all.filter((n) => n.attr.age.Int >= 60).length);
  assert.ok(old.nodes.every((n) => n.attr.age.Int >= 60));
  const noMail = await s.find('social', { And: [{ Label: 'Person' }, { Not: { Exists: { path: ['email'] } } }] }, { limit: 1000 });
  assert.equal(noMail.total, all.filter((n) => !n.attr.email).length);
  // Int and Float compare as numbers
  const f = await s.find('social', { Compare: { path: ['age'], op: 'Gt', value: { Float: 59.5 } } }, { limit: 1000 });
  assert.equal(f.total, all.filter((n) => n.attr.age.Int >= 60).length);
  await rejects(s.find('social', { Bogus: 1 }), 'invalid_argument');
  await rejects(s.find('social', { Compare: { path: ['age'], op: 'Like', value: { Int: 1 } } }), 'invalid_argument');
  await rejects(s.find('nowhere', { Label: 'Person' }), 'not_found');
});

test('find pages with cursors and sees every node once', async () => {
  const s = fresh(); const seen = []; let cursor = '';
  for (let k = 0; k < 20; k++) {
    const p = await s.find('social', { Label: 'Person' }, { limit: 10, cursor });
    seen.push(...p.nodes.map((n) => n.id)); cursor = p.meta.next; if (!cursor) break;
  }
  assert.equal(seen.length, 64); assert.equal(new Set(seen).size, 64);
  assert.deepEqual(seen, seen.slice().sort());
  await rejects(s.find('social', { Label: 'Person' }, { cursor: '%%' }), 'invalid_argument');
});

test('explain picks the label index, a property index, a union or a scan', async () => {
  const s = fresh();
  const plan = async (f) => (await s.explain('social', f, { analyze: true })).explain;
  assert.deepEqual((await plan({ Label: 'City' })).plan, { label: { label: 'City' } });
  assert.deepEqual((await plan({ Compare: { path: ['age'], op: 'Eq', value: { Int: 30 } } })).plan, { index: { path: ['age'], op: 'Eq' } });
  assert.ok((await plan({ Or: [{ Label: 'City' }, { Label: 'Company' }] })).plan.union);
  const scan = await plan({ Compare: { path: ['score'], op: 'Gt', value: { Float: 50 } } });
  assert.ok(scan.plan.scan); assert.equal(scan.candidates, scan.nodes);
  // An index being built isn't used, and explain says so
  const b = await plan({ Compare: { path: ['joined'], op: 'Eq', value: { Date: '2024-01-10' } } });
  assert.ok(b.plan.scan); assert.deepEqual(b.building, [['joined']]);
});

test('patterns: the core\'s text, chains with labels, properties, types, directions and lengths', async () => {
  const s = fresh();
  const knows = await s.matchPattern('social', '(a:Person)-[k:KNOWS]->(b)', { limit: 10000 });
  assert.deepEqual(knows.columns, [{ name: 'a', kind: 'node' }, { name: 'b', kind: 'node' }, { name: 'k', kind: 'edge' }]);
  const st = await s.namespaceStatus('social'); const sc = await s.schema('social');
  assert.equal(knows.rows.length, sc.types.find((t) => t.name === 'KNOWS').count);
  const back = await s.matchPattern('social', '(b)<-[:KNOWS]-(a:Person)', { limit: 10000 });
  assert.equal(back.rows.length, knows.rows.length);
  const any = await s.matchPattern('social', '(a)-[e]-(b)', { limit: 100000 });
  assert.equal(any.rows.length, 2 * st.edges);
  const two = await s.matchPattern('social', "(a:Person {name: 'Ann Okafor'})-[:KNOWS*1..2]->(b)", { limit: 1000 });
  assert.ok(two.rows.every((r) => r.edges[0].length >= 1 && r.edges[0].length <= 2));
  const lim = await s.matchPattern('social', '(a)-[e]->(b)', { limit: 7 });
  assert.equal(lim.rows.length, 7); assert.equal(lim.meta.truncated, true);
  for (const bad of ['(a', '(a)-[x]->(b),(c)', '(a)-[:KNOWS*1..9]->(b)', '(a)-[e]->(a)', '(a {age: })']) await rejects(s.matchPattern('social', bad), 'invalid_argument');
});

test('commits are all or nothing, keep unique constraints and are refused on a read-only namespace', async () => {
  const s = fresh();
  const seq = (await s.namespaceStatus('social')).seq;
  const r = await s.commit('social', [{ setAttr: { target: { node: 'bob' }, key: 'age', value: { Int: 99 } } }, { addEdge: { from: 'bob', to: 'carol', type: 'KNOWS' } }]);
  assert.equal(r.seq, seq + 1); assert.equal(r.edgeIds.length, 1);
  assert.deepEqual((await s.getNodes('social', ['bob'])).nodes[0].attr.age, { Int: 99 });
  const taken = (await s.getNodes('social', ['carol'])).nodes[0].attr.email;
  assert.ok(taken, 'carol has an email');
  await rejects(s.commit('social', [{ setAttr: { target: { node: 'bob' }, key: 'age', value: { Int: 1 } } }, { setAttr: { target: { node: 'bob' }, key: 'email', value: taken } }]), 'constraint_violation');
  assert.deepEqual((await s.getNodes('social', ['bob'])).nodes[0].attr.age, { Int: 99 }, 'nothing of a refused commit is applied');
  assert.equal((await s.namespaceStatus('social')).seq, seq + 1);
  await rejects(s.commit('social', [{ deleteEdge: { id: 999999 } }]), 'not_found');
  await rejects(s.commit('social', []), 'invalid_argument');
  const d = fresh({ scenario: 'degraded' });
  await rejects(d.commit('archive_2025', [{ deleteNode: { id: 'evt-1' } }]), 'read_only');
  await rejects(d.createIndex('archive_2025', ['kind']), 'read_only');
});

test('neighbours: both directions, bounded, with the edges to them', async () => {
  const s = fresh();
  const r = await s.neighbours('social', 'ann', { limit: 3 });
  assert.ok(r.nodes.length <= 3);
  assert.ok(r.edges.every((e) => (e.from === 'ann' || e.to === 'ann') && r.nodes.some((n) => n.id === (e.from === 'ann' ? e.to : e.from))));
  await rejects(s.neighbours('social', 'nobody'), 'not_found');
});

test('an index is built online and becomes ready', async () => {
  const s = fresh();
  await s.createIndex('inventory', ['weight_g']);
  await rejects(s.createIndex('inventory', ['weight_g']), 'conflict');
  const building = (await s.namespaceStatus('inventory')).indexes.find((x) => x.path[0] === 'weight_g');
  assert.ok(building.building && !building.size);
  for (let k = 0; k < 3 * 200; k++) s.tick();
  const ready = (await s.namespaceStatus('inventory')).indexes.find((x) => x.path[0] === 'weight_g');
  assert.ok(ready.ready && ready.size.entries > 0);
});

test('the server status: health, series, problems in the degraded scenario', async () => {
  const calm = await fresh().server();
  assert.equal(calm.health, 'ok'); assert.equal(calm.ready, true);
  assert.equal(calm.series.commitsPerSec.length, 90);
  assert.ok(calm.namespaces.every((n) => !n.readOnly && !n.checkpointFailure));
  const s = fresh({ scenario: 'degraded' }); const d = await s.server();
  assert.equal(d.health, 'warn');
  assert.ok(d.namespaces.find((n) => n.name === 'archive_2025').readOnly);
  assert.ok(d.namespaces.find((n) => n.name === 'orders').checkpointFailure);
  assert.ok(d.memory.usedBytes / d.memory.limitBytes >= d.memory.warnAt);
  // A failed checkpoint stops checkpoints: the lag grows
  const lag0 = d.namespaces.find((n) => n.name === 'orders'); for (let k = 0; k < 30; k++) s.tick();
  const lag1 = (await s.server()).namespaces.find((n) => n.name === 'orders');
  assert.ok(lag1.seq - lag1.checkpoint > lag0.seq - lag0.checkpoint);
});

test('cancel ends an active request and is logged', async () => {
  const s = fresh(); const seen = []; s.onLog((e) => seen.push(e));
  const { active } = await s.server(); await s.cancel(active[0].id);
  assert.ok(!(await s.server()).active.some((q) => q.id === active[0].id));
  assert.ok(seen.some((e) => e.msg.includes(active[0].id)));
  await rejects(s.cancel('req-0'), 'not_found');
});

test('the query line parses the shell\'s commands', () => {
  assert.deepEqual(Q.parse('-- hi\nmatch (a)-[e]->(b)\n\\limit 40'), { cmd: 'match', pattern: '(a)-[e]->(b)', opts: { limit: 40 } });
  assert.deepEqual(Q.parse('find {"Label":\n "Person"}'), { cmd: 'find', filter: { Label: 'Person' }, opts: {} });
  assert.deepEqual(Q.parse('node a b'), { cmd: 'node', ids: ['a', 'b'], opts: {} });
  assert.deepEqual(Q.parse('neighbors ann'), { cmd: 'neighbours', id: 'ann', opts: {} });
  assert.equal(Q.parse('\\limit off\nfind {"Const": true}').opts.limit, null);
  for (const bad of ['', '-- only a comment', 'frobnicate x', 'find {nope', 'match', 'node', 'neighbours a b', '\\timeout 3\nnode a', '\\limit 0\nnode a']) {
    assert.throws(() => Q.parse(bad), (e) => e.code === 'invalid_argument', bad);
  }
});

test('an explain becomes plan rows, a scan warns', () => {
  const rows = Q.planRows({ plan: { scan: {} }, estimatedCandidates: 82, candidates: 82, matched: 9, nodes: 82, building: [] }, { Compare: { path: ['score'], op: 'Gt', value: { Float: 50 } } }, 3);
  assert.deepEqual(rows.map((r) => r.op), ['Find', 'Filter', 'Scan']);
  assert.equal(rows[0].detail, 'score > 50'); assert.ok(rows[2].hot && rows[2].warn.includes('[score]'));
  const u = Q.planRows({ plan: { union: { plans: [{ label: { label: 'A' } }, { index: { path: ['x'], op: 'Eq' } }] } }, estimatedCandidates: 4, candidates: 3, matched: 3, nodes: 9, building: [] }, { Or: [{ Label: 'A' }, { Compare: { path: ['x'], op: 'Eq', value: { Int: 1 } } }] });
  assert.deepEqual(u.map((r) => [r.op, r.depth]), [['Find', 0], ['Filter', 1], ['Union', 2], ['LabelIndex', 3], ['PropertyIndex', 3]]);
  assert.deepEqual(Q.firstPath({ And: [{ Label: 'A' }, { Not: { Exists: { path: ['p', 'q'] } } }] }), ['p', 'q']);
});

test('values as text and back', () => {
  assert.equal(U.text({ List: [{ Int: 1 }, { String: 'a' }] }), '[1, a]');
  assert.equal(U.text({ Dict: { a: { Bool: true } } }), '{a: true}');
  assert.equal(U.text('None'), 'None');
  assert.deepEqual(U.parseValue('42', 'Int'), { Int: 42 });
  assert.deepEqual(U.parseValue('4.5', 'Int'), { Float: 4.5 });
  assert.deepEqual(U.parseValue('42', 'String'), { String: '42' });
  assert.deepEqual(U.parseValue('true', null), { Bool: true });
  assert.deepEqual(U.parseValue('2026-01-02', 'Date'), { Date: '2026-01-02' });
  assert.deepEqual(U.parseValue('["a", 1]', 'String'), { List: [{ String: 'a' }, { Int: 1 }] });
  assert.deepEqual(U.parseValue('{"b": 1, "a": 2.5}', null), { Dict: { a: { Float: 2.5 }, b: { Int: 1 } } });
  assert.deepEqual(U.parseValue('hello', 'Int'), { String: 'hello' });
});

test('numbers, sizes and durations in the design system\'s voice', () => {
  assert.equal(U.num(1204331), '1 204 331'); assert.equal(U.num(1234.567, 2), '1 234.57'); assert.equal(U.num(-5), '−5'); assert.equal(U.num(null), '—');
  assert.equal(U.bytes(512), '512 B'); assert.equal(U.bytes(1.5 * 2 ** 30), '1.5 GB'); assert.equal(U.bytes(212 * 2 ** 30), '212 GB');
  assert.equal(U.ms(2.44), '2.4 ms'); assert.equal(U.ms(62.4), '62 ms'); assert.equal(U.ms(1500), '1.5 s');
  assert.equal(U.span(3 * 86400 + 4 * 3600 + 60), '3 d 4 h'); assert.equal(U.span(75), '1 min 15 s');
  assert.equal(U.pct(0.873), '87 %'); assert.equal(U.pct(0.012), '1.2 %');
});

test('labels keep one colour and shape; the primary label is the biggest', () => {
  const st = U.labelStyles(['Person', 'City', 'Order', 'Part', 'A', 'B', 'C']);
  assert.equal(st.A.color, 'lb-1'); assert.equal(st.Person.color, 'lb-6'); assert.equal(st.Order.shape, 'diamond'); assert.equal(st.Part.shape, 'square');
  assert.deepEqual(U.labelStyles(['City', 'Person']), U.labelStyles(['Person', 'City']));
  assert.equal(U.primaryLabel({ labels: ['Admin', 'Person'] }, { Admin: 1, Person: 64 }), 'Person');
  assert.equal(U.caption({ id: 'x', attr: { name: { String: 'Ann' } } }), 'Ann');
  assert.equal(U.caption({ id: 'x', attr: { age: { Int: 3 } } }), 'x');
});

test('the layout is deterministic and keeps nodes apart', () => {
  const nodes = Array.from({ length: 30 }, (_, i) => ({ id: 'n' + i }));
  const edges = nodes.slice(1).map((n, i) => ({ s: 'n' + i, t: n.id }));
  const a = U.layout(nodes, edges), b = U.layout(nodes, edges);
  assert.deepEqual([...a], [...b]);
  const p = [...a.values()]; let close = 0;
  for (let i = 0; i < p.length; i++) for (let j = i + 1; j < p.length; j++) if (Math.hypot(p[i].x - p[j].x, p[i].y - p[j].y) < 20) close++;
  assert.equal(close, 0);
});

/* ------------------------------------------------------------------ the REST Source, on a fake server */
const rest = require('../src/rest.js');
function fakeServer(routes) {
  const calls = [];
  const fetch = async (url, init) => {
    calls.push({ url, method: init.method, body: init.body && JSON.parse(init.body), headers: init.headers });
    const key = init.method + ' ' + url; const r = routes[key];
    if (!r) return { ok: false, status: 404, text: async () => JSON.stringify({ code: 'invalid_argument', message: 'no route ' + key }) };
    const [status, body] = typeof r === 'function' ? r(calls[calls.length - 1].body) : r;
    return { ok: status < 300, status, text: async () => JSON.stringify(body), json: async () => body };
  };
  return { fetch, calls };
}

test('the REST Source implements the contract', () => {
  const s = rest.create({ fetch: async () => ({ ok: true, status: 200, text: async () => '{}' }) });
  for (const m of METHODS) assert.equal(typeof s[m], 'function', m);
});

test('REST answers become the contract\'s shapes: numbers, absent defaults, paths', async () => {
  const { fetch, calls } = fakeServer({
    'GET /v1/namespaces/s': [200, { status: { id: '2', name: 's', createdMicros: '17', seq: '5', syncedSeq: '5', nodes: '2', edges: '1', memoryBytes: '300', indexes: [{ path: { keys: ['age'] }, declared: true, size: { entries: '1', distinctKeys: '1', memoryBytes: '9' }, ready: {} }, { path: { keys: ['x'] }, building: { scanned: '1', total: '2' } }], constraints: '1', recovery: {} } }],
    'POST /v1/namespaces/s/subgraph': (b) => [200, { nodes: [{ id: 'ann', labels: ['P'], version: '1' }, { id: 'bob', version: '1' }], edges: [{ from: 'ann', to: 'bob', type: 'K', version: '1' }, { id: '3', from: 'bob', to: 'ann', version: '1' }], meta: { seq: '5' } }],
    'POST /v1/namespaces/s/match': [200, { rows: [{ nodes: ['ann', 'bob'], edges: [{ ids: ['0'] }] }], meta: { seq: '5', next: 'c' } }],
    'POST /v1/namespaces/s/explain': [200, { explain: { plan: { index: { path: { keys: ['age'] }, lookup: 'LOOKUP_RANGE' } }, estimatedCandidates: '4', nodes: '9' } }],
    'POST /v1/namespaces/s/find': (b) => [400, { code: 'invalid_argument', message: 'bad filter' }],
  });
  const s = rest.create({ fetch });
  const st = await s.namespaceStatus('s');
  assert.equal(st.seq, 5); assert.equal(st.checkpoint, null); assert.deepEqual(st.indexes[0].path, ['age']); assert.equal(st.indexes[0].size.entries, 1);
  assert.deepEqual(st.indexes[1].building, { scanned: 1, total: 2 }); assert.equal(st.recovery.checkpoint, null);
  const nb = await s.neighbours('s', 'ann');
  assert.deepEqual(nb.nodes.map((x) => x.id), ['bob']); assert.deepEqual(nb.edges.map((e) => e.id), [0, 3], 'an absent id is edge 0');
  assert.deepEqual(calls.at(-1).body, { seeds: ['ann'], depth: 1, direction: 'DIRECTION_BOTH', options: { limits: { maxVisited: 5000 }, partial: true } });
  const m = await s.matchPattern('s', '(a)-[k:K]->(b)', { limit: 1 });
  assert.deepEqual(m.columns.map((c) => c.name), ['a', 'b', 'k']); assert.deepEqual(m.rows[0].edges, [[0]]); assert.equal(m.meta.truncated, true);
  const ex = (await s.explain('s', { Label: 'P' }, { analyze: true })).explain;
  assert.deepEqual(ex.plan, { index: { path: ['age'], op: 'Range' } }); assert.equal(ex.candidates, undefined);
  await rejects(s.find('s', { Bogus: 1 }), 'invalid_argument');
  assert.equal(calls.find((c) => c.method === 'POST').headers['content-type'], 'application/json', 'bodies are JSON, or the server refuses them (415)');
  assert.ok(s.log().some((e) => e.level === 'WARN' && e.msg.includes('400')));
  await rejects(s.cancel('req-1'), 'unavailable');
});

test('the REST Source reports the server\'s readiness (step 16b)', async () => {
  let ready = false;
  const { fetch } = fakeServer({
    'GET /v1/health/ready': () => (ready ? [200, { state: 'HEALTH_STATE_READY', ready: true }] : [503, { state: 'HEALTH_STATE_RECOVERING' }]),
    'GET /v1/namespaces': [200, { namespaces: [] }],
  });
  const s = rest.create({ fetch });
  const recovering = await s.server();
  assert.equal(recovering.ready, false); assert.equal(recovering.health, 'warn');
  ready = true;
  const up = await s.server();
  assert.equal(up.ready, true); assert.equal(up.health, 'ok');
});

test('a server that doesn\'t answer is unavailable', async () => {
  const s = rest.create({ fetch: async () => { throw new Error('connection refused'); } });
  await rejects(s.namespaces(), 'unavailable');
  const p = rest.create({ fetch: async () => ({ ok: false, status: 502, text: async () => '<html>bad gateway</html>' }) });
  await rejects(p.namespaces(), 'unavailable');
});

/* ------------------------------------------------------------------ logging in (step 15a) */
test('the REST Source logs in with the session cookie and sends the CSRF header', async () => {
  let loggedIn = false;
  const { fetch, calls } = fakeServer({
    'GET /v1/auth/whoami': () => (loggedIn ? [200, { user: { name: 'ann', grants: { social: 'ROLE_WRITE' } }, authEnabled: true }] : [401, { code: 'unauthenticated', message: 'this server needs credentials' }]),
    'POST /v1/auth/login': (b) => (b.password === 'right' ? ((loggedIn = true), [200, { user: { name: 'ann', grants: { social: 'ROLE_WRITE' } }, expiresMs: '9' }]) : [401, { code: 'unauthenticated', message: 'wrong user or password' }]),
    'POST /v1/auth/logout': () => ((loggedIn = false), [200, {}]),
    'GET /v1/namespaces': () => (loggedIn ? [200, { namespaces: [] }] : [401, { code: 'unauthenticated', message: 'the token is unknown, expired or revoked' }]),
  });
  const s = rest.create({ fetch });
  const ended = []; const off = s.onAuth((x) => ended.push(x));
  await rejects(s.session(), 'unauthenticated');
  assert.deepEqual(ended, [null], 'a 401 tells the pages to log in again');
  await rejects(s.login('ann', 'wrong'), 'unauthenticated');
  assert.equal(ended.length, 1, 'a failed login is not a lost session');
  const session = await s.login('ann', 'right');
  assert.deepEqual(session, { authEnabled: true, user: { name: 'ann', admin: false, grants: { social: 'write' } } });
  assert.deepEqual(calls.filter((c) => c.url === '/v1/auth/login').at(-1).body, { user: 'ann', password: 'right', cookie: true }, 'the token goes into the HttpOnly cookie, not to this script');
  assert.deepEqual((await s.session()).user.name, 'ann');
  assert.deepEqual(await s.namespaces(), []);
  for (const c of calls) assert.equal(c.headers['x-iwdb-csrf'], '1', `${c.method} ${c.url} carries the CSRF header`);
  await s.logout();
  await rejects(s.namespaces(), 'unauthenticated');
  assert.equal(ended.length, 3);
  off();
  assert.ok(!s.log().some((e) => e.msg.includes('right')), 'no password in the log');
});

test('the mock has a login too, per tab', async () => {
  const kept = new Map(); const storage = { getItem: (k) => kept.get(k) ?? null, setItem: (k, v) => kept.set(k, v), removeItem: (k) => kept.delete(k) };
  const s = fresh({ storage });
  await rejects(s.session(), 'unauthenticated');
  await rejects(s.login('admin', 'nope'), 'unauthenticated');
  const session = await s.login('admin', 'admin');
  assert.equal(session.user.admin, true);
  // Another page of the same tab knows it
  const other = fresh({ storage });
  assert.equal((await other.session()).user.name, 'admin');
  const ended = []; other.onAuth((x) => ended.push(x));
  await other.logout();
  assert.deepEqual(ended, [null]);
  await rejects(s.session(), 'unauthenticated');
  const reader = await s.login('reader', 'reader');
  assert.deepEqual(reader.user.grants, { social: 'read' });
});
