'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 30; i++) await Promise.resolve(); };

// Run the actual Explorer component with hooks and Source calls under our control.
function harness(overrides = {}, uiOverrides = {}) {
  const states = []; const effects = []; const refs = []; const calls = []; const timers = new Map();
  let cursor = 0, tree, component, mounted = true, lateWrites = 0, nextTimer = 0;
  const source = new Proxy({ kind: 'mock', log: () => [], onLog: () => () => {}, server: () => new Promise(() => {}), namespaces: () => Promise.resolve([]), ...overrides }, {
    get(target, key) { const method = target[key]; return typeof method === 'function' ? (...args) => { calls.push([key, ...args]); return method(...args); } : method || (() => { throw Error('Unexpected Source read: ' + key); }); },
  });
  const React = {
    createElement: (type, props, ...children) => ({ type, props: props || {}, children }), Fragment: 'Fragment',
    useState(initial) { const index = cursor++; if (!(index in states)) states[index] = initial; return [states[index], (value) => { if (!mounted) lateWrites++; states[index] = typeof value === 'function' ? value(states[index]) : value; }]; },
    useRef(initial) { const index = cursor++; return refs[index] || (refs[index] = { current: initial }); },
    useCallback(fn) { cursor++; return fn; }, useMemo(fn) { cursor++; return fn(); },
    useEffect(fn, deps) { const index = cursor++; const old = effects[index]; if (!old || !deps || deps.some((v, i) => !Object.is(v, old.deps[i]))) effects[index] = { fn, deps, cleanup: old && old.cleanup, pending: true }; },
  };
  const U = new Proxy({ source: () => source, params: () => new URLSearchParams('ns=missing'), commonItems: () => [], problems: () => [], labelStyles: () => [], layout: () => new Map(), ...uiOverrides }, { get: (target, key) => target[key] || (() => '') });
  const I = new Proxy({ setLabels: () => {} }, { get: (target, key) => target[key] || key });
  const window = { React, IronWeaver: I, IW: { ui: U, query: { parse: () => ({ cmd: 'match', opts: {}, pattern: '(a)-[e]->(b)' }) } }, location: { href: 'https://example.test/' }, history: { replaceState() {} }, addEventListener() {}, removeEventListener() {} };
  const context = { window, ReactDOM: { createRoot: () => ({ render: (node) => { component = node.children[0].type; } }) }, IW: window.IW, document: { getElementById() {} }, URL, performance, setTimeout, clearTimeout, setInterval: (fn) => { const id = ++nextTimer; timers.set(id, fn); return id; }, clearInterval: (id) => timers.delete(id) };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../src/explorer.js'), 'utf8'), context);
  function render() { cursor = 0; tree = component(); for (const effect of effects) if (effect && effect.pending) { if (effect.cleanup) effect.cleanup(); effect.cleanup = effect.fn(); effect.pending = false; } return tree; }
  return { source, calls, render, timers, get lateWrites() { return lateWrites; }, unmount() { mounted = false; for (const effect of effects) if (effect && effect.cleanup) effect.cleanup(); }, nodes() { const all = []; const walk = (n) => { if (!n || typeof n !== 'object') return; all.push(n); (n.children || []).flat(Infinity).forEach(walk); }; walk(tree); return all; } };
}

test('an empty visible namespace list displays an empty state and issues no namespace reads', async () => {
  const h = harness(); h.render(); await settle(); h.render();
  assert.ok(h.nodes().some((n) => n.type === 'EmptyState' && n.props.title === 'No visible namespaces.'));
  assert.deepEqual(h.calls.filter(([method]) => !['server', 'namespaces', 'log', 'onLog'].includes(method)), []);
  h.unmount();
});

test('initial namespace-list errors appear in the alert and log', async () => {
  const error = Error('Access denied while listing namespaces');
  const h = harness({ namespaces: () => Promise.reject(error) }); h.render(); await settle(); h.render();
  assert.ok(h.nodes().some((n) => n.props.role === 'alert'));
  const ticker = h.nodes().find((n) => n.type === 'LogTicker');
  assert.ok(ticker.props.entries.some((e) => e.level === 'ERROR' && e.msg.includes(error.message)));
  h.unmount();
});

for (const reject of [false, true]) test(`namespace and server ${reject ? 'errors' : 'responses'} after unmount do not update state`, async () => {
  const namespaces = deferred(), server = deferred();
  const h = harness({ namespaces: () => namespaces.promise, server: () => server.promise }); h.render(); h.unmount();
  if (reject) { namespaces.reject(Error('offline')); server.reject(Error('offline')); }
  else { namespaces.resolve([{ name: 'social' }]); server.resolve({ ready: true }); }
  await settle(); assert.equal(h.lateWrites, 0); assert.equal(h.timers.size, 0);
});

test('initial schema results after unmount cannot start the first query or update state', async () => {
  const schema = deferred(), status = deferred();
  const h = harness({ namespaces: () => Promise.resolve([{ name: 'social' }]), schema: () => schema.promise, namespaceStatus: () => status.promise });
  h.render(); await settle(); h.render(); h.unmount();
  schema.resolve({ labels: [] }); status.resolve({}); await settle();
  assert.equal(h.lateWrites, 0); assert.ok(!h.calls.some(([method]) => method === 'matchPattern'));
});

test('initial query responses after unmount cannot update the result or cache', async () => {
  const match = deferred();
  const h = harness({ namespaces: () => Promise.resolve([{ name: 'social' }]), schema: () => Promise.resolve({ labels: [], types: [], constraints: [] }), namespaceStatus: () => Promise.resolve({ indexes: [] }), matchPattern: () => match.promise, getNodes: () => Promise.resolve({ nodes: [] }), getEdges: () => Promise.resolve({ edges: [] }) });
  h.render(); await settle(); h.render(); await settle(); h.unmount();
  match.resolve({ rows: [], columns: [], meta: { work: { visited: 0, edges: 0 }, seq: 1 } }); await settle();
  assert.equal(h.lateWrites, 0);
});


test('invalid property edits surface an error without staging a mutation', async () => {
  const node = { id: 'n1', labels: [], attr: { age: { Int: 1 } }, version: 1 };
  const h = harness({ namespaces: () => Promise.resolve([{ name: 'social' }]), schema: () => Promise.resolve({ labels: [], types: [], constraints: [] }), namespaceStatus: () => Promise.resolve({ indexes: [] }), matchPattern: () => Promise.resolve({ rows: [{ nodes: ['n1'], edges: [] }], columns: [{ name: 'a', kind: 'node' }], meta: { work: { visited: 1, edges: 0 }, seq: 1 } }), getNodes: () => Promise.resolve({ nodes: [node] }), getEdges: () => Promise.resolve({ edges: [] }), neighbours: () => Promise.resolve({ nodes: [], edges: [] }) }, { parseValue: () => { throw new RangeError('Integer exceeds the safe range'); }, kindOf: () => 'Int', TYPE: { Int: 'INTEGER' } });
  h.render(); await settle(); h.render(); await settle(); h.render();
  assert.ok(h.nodes().find((n) => n.type === 'GraphCanvas'), JSON.stringify(h.nodes().find((n) => n.props.role === 'alert')));
  h.nodes().find((n) => n.type === 'GraphCanvas').props.onSelect(node); h.render();
  h.nodes().find((n) => n.type === 'Inspector').props.onSave(node, 'age', '9007199254740993'); h.render();
  assert.ok(h.nodes().some((n) => n.props.role === 'alert'));
  assert.equal(h.nodes().find((n) => n.type === 'StatusRail').props.txOpen, 0);
  h.unmount(); await settle();
});

test('initial namespace schema errors are visible', async () => {
  const h = harness({ namespaces: () => Promise.resolve([{ name: 'social' }]), schema: () => Promise.reject(Error('Schema unavailable')), namespaceStatus: () => Promise.resolve({}) });
  h.render(); await settle(); h.render(); await settle(); h.render();
  assert.ok(h.nodes().some((n) => n.props.role === 'alert'));
  assert.ok(!h.calls.some(([method]) => method === 'matchPattern'));
  h.unmount();
});

test('initial query failures are logged once', async () => {
  const h = harness({ namespaces: () => Promise.resolve([{ name: 'social' }]), schema: () => Promise.resolve({ labels: [], types: [], constraints: [] }), namespaceStatus: () => Promise.resolve({ indexes: [] }), matchPattern: () => Promise.reject(Error('Query unavailable')) });
  h.render(); await settle(); h.render(); await settle(); h.render();
  assert.ok(h.nodes().some((n) => n.props.role === 'alert'));
  const logs = h.nodes().find((n) => n.type === 'LogTicker').props.entries;
  assert.equal(logs.filter((e) => e.level === 'ERROR' && e.msg.includes('Query unavailable')).length, 1);
  h.unmount();
});
