'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function harness(kind = 'rest') {
  const pending = [], updates = [], intervals = new Map(), effects = [], refs = [];
  let live = true, tickCount = 0, logKind = 'requests', log = [], component, stateIndex = 0, refIndex = 0, timerId = 0;
  const source = {
    kind, log: () => log, logKind: () => logKind,
    onLog: () => () => {}, tick: () => tickCount++,
    server: () => new Promise((resolve, reject) => pending.push({ resolve, reject })),
  };
  const React = {
    createElement: (type, props, ...children) => ({ type, props, children }),
    useState: (initial) => {
      const index = stateIndex++;
      return [index === 2 ? live : initial, (value) => updates.push({ index, value })];
    },
    useEffect: (fn) => effects.push(fn),
    useRef: (value) => { const index = refIndex++; return refs[index] || (refs[index] = { current: value }); },
    useMemo: (fn) => fn(),
  };
  const ui = { source: () => source, problems: () => [] };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../src/status.js'), 'utf8'), {
    window: { React, IronWeaver: {}, IW: { ui } }, IW: { ui },
    ReactDOM: { createRoot: () => ({ render: (element) => { component = element.children[0].type; } }) },
    document: { getElementById: () => ({}) },
    setInterval: (fn, ms) => { const id = ++timerId; intervals.set(id, { fn, ms }); return id; },
    clearInterval: (id) => intervals.delete(id),
  });
  const start = (nextLive = true) => {
    live = nextLive; stateIndex = refIndex = 0; effects.length = 0; component();
    return effects[1]();
  };
  return {
    start, pending, updates, intervals,
    tick: () => { for (const timer of intervals.values()) timer.fn(); },
    tickCount: () => tickCount,
    serverLog: (entries) => { logKind = 'server'; log = entries; },
  };
}
const flush = () => new Promise((resolve) => setImmediate(resolve));

test('status polls immediately, skips overlapping reads, and retries on the existing cadence', async () => {
  for (const [kind, cadence] of [['rest', 3000], ['mock', 1000]]) {
    const h = harness(kind); const stop = h.start();
    assert.equal(h.pending.length, 1);
    assert.equal([...h.intervals.values()][0].ms, cadence);
    h.tick(); h.tick();
    assert.equal(h.pending.length, 1);
    assert.equal(h.tickCount(), 0);
    h.pending[0].resolve({ version: 'first' }); await flush();
    assert.equal(h.updates.filter((x) => x.index === 0).length, 1);
    h.tick(); assert.equal(h.pending.length, 2); assert.equal(h.tickCount(), 1);
    h.pending[1].reject(new Error('offline')); await flush();
    h.tick(); assert.equal(h.pending.length, 3); assert.equal(h.tickCount(), 2);
    stop(); h.pending[2].resolve({ version: 'retired' }); await flush();
    assert.equal(h.updates.filter((x) => x.index === 0).length, 1);
    assert.equal(h.intervals.size, 0);
  }
});

test('pause and resume retire pending snapshots and preserve the server-log transition', async () => {
  const h = harness(); const stopLive = h.start(); stopLive();
  const stopPaused = h.start(false);
  assert.equal(h.pending.length, 2, 'pause preserves the immediate read');
  assert.equal(h.intervals.size, 0);
  stopPaused(); const stopResumed = h.start(true);
  assert.equal(h.pending.length, 3);
  h.serverLog([{ message: 'server entry' }]);
  h.pending[2].resolve({ version: 'current' }); await flush();
  const currentUpdates = h.updates.length;
  h.pending[1].resolve({ version: 'paused' });
  h.pending[0].resolve({ version: 'old live' }); await flush();
  assert.equal(h.updates.length, currentUpdates, 'retired effects cannot update status, time or logs');
  assert.equal(h.updates.find((x) => x.index === 0).value.version, 'current');
  assert.equal(h.updates.find((x) => x.index === 1).value[0].message, 'server entry');
  h.tick(); stopResumed();
  h.pending[3].resolve({ version: 'after unmount' }); await flush();
  assert.equal(h.updates.length, currentUpdates);
});
