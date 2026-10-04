// Loads the console's sample data (the mock's namespaces) into a running iwdb-server over REST (step 16a), so the
// console has something to explore there:
//
//   node console/tools/seed.mjs                     # http://127.0.0.1:7600
//   node console/tools/seed.mjs http://host:7600 --replace
//
// Each namespace is created, filled with commits of at most 500 mutations, and given the mock's indexes and
// unique constraints. A namespace that exists is skipped, or dropped first with --replace.
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const mock = require('../src/mock.js');

const args = process.argv.slice(2);
const base = (args.find((a) => !a.startsWith('--')) || process.env.IWDB_URL || 'http://127.0.0.1:7600').replace(/\/$/, '');
const replace = args.includes('--replace');
const BATCH = 500;

async function call(method, path, body) {
  const r = await fetch(base + path, { method, headers: body ? { 'content-type': 'application/json' } : {}, body: body ? JSON.stringify(body) : undefined });
  const text = await r.text(); const json = text ? JSON.parse(text) : {};
  if (!r.ok) { const e = new Error(`${method} ${path}: ${r.status} ${json.code}: ${json.message}`); e.code = json.code; throw e; }
  return json;
}

const src = mock.create({ latency: [0, 0] });
const have = new Set(((await call('GET', '/v1/namespaces')).namespaces || []).map((n) => n.name));
for (const { name } of await src.namespaces()) {
  if (have.has(name)) {
    if (!replace) { console.log(`${name}: exists, skipped (--replace drops it first)`); continue; }
    await call('DELETE', `/v1/namespaces/${name}`);
  }
  await call('PUT', `/v1/namespaces/${name}`);
  const nodes = (await src.find(name, { Const: true }, { limit: 10000 })).nodes;
  const edges = (await src.subgraph(name, nodes.map((n) => n.id))).edges;
  const muts = nodes.map((n) => ({ upsertNode: { id: n.id, labels: n.labels, attr: n.attr } }))
    .concat(edges.map((e) => ({ addEdge: { from: e.from, to: e.to, type: e.type, attr: e.attr } })));
  let seq = 0;
  for (let k = 0; k < muts.length; k += BATCH) seq = (await call('POST', `/v1/namespaces/${name}/commit`, { mutations: muts.slice(k, k + BATCH) })).result.seq;
  const st = await src.namespaceStatus(name); const sc = await src.schema(name);
  for (const c of sc.constraints) await call('POST', `/v1/namespaces/${name}/catalog`, { change: { addConstraint: { kind: 'CONSTRAINT_KIND_UNIQUE', label: c.label, path: { keys: c.path } } } });
  for (const ix of st.indexes.filter((x) => x.declared)) await call('POST', `/v1/namespaces/${name}/catalog`, { change: { createIndex: { path: { keys: ix.path } } } });
  console.log(`${name}: ${nodes.length} nodes, ${edges.length} edges, ${st.indexes.filter((x) => x.declared).length} indexes, ${sc.constraints.length} constraints (seq ${seq})`);
}
