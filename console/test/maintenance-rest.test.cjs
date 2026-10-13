'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const { create, SourceError } = require('../src/rest.js');

const response = (status, text) => ({ ok: status >= 200 && status < 300, status, text: async () => text });
const checkError = (code, message) => (e) => {
  assert.ok(e instanceof SourceError);
  assert.equal(e.code, code);
  assert.match(e.message, message);
  return true;
};

test('malformed successful JSON yields a typed error and an error log', async () => {
  for (const body of ['<html>proxy page</html>', '{"namespaces":']) {
    const src = create({ fetch: async () => response(200, body) });
    await assert.rejects(src.namespaces(), checkError('internal', /invalid JSON response.*\/v1\/namespaces.*200/));
    assert.equal(src.log().at(-1).level, 'ERROR');
    assert.match(src.log().at(-1).msg, /invalid JSON response/);
  }
});

test('fetch and response-body transport failures share unavailable errors and logging', async () => {
  for (const fetch of [
    async () => { throw new Error('connection lost'); },
    async () => ({ ...response(200, ''), text: async () => { throw new Error('connection lost'); } }),
  ]) {
    const src = create({ base: 'https://db.example', fetch });
    await assert.rejects(src.namespaces(), checkError('unavailable', /no answer from https:\/\/db.example\/v1\/namespaces: connection lost/));
    assert.equal(src.log().at(-1).level, 'ERROR');
    assert.match(src.log().at(-1).msg, /no answer: connection lost/);
  }
});

test('empty successes and valid JSON retain their existing results', async () => {
  assert.deepEqual(await create({ fetch: async () => response(204, '') }).namespaces(), []);
  assert.deepEqual(await create({ fetch: async () => response(200, '{"namespaces":[{"id":"7","name":"graph"}]}') }).namespaces(), [
    { id: 7, name: 'graph', createdMicros: 0, createdSeq: 0 },
  ]);
  const src = create({ fetch: async () => response(204, '') });
  const auth = []; src.onAuth((u) => auth.push(u));
  assert.deepEqual(await src.logout(), {});
  assert.deepEqual(auth, [null]);
});

test('HTTP errors retain structured details and proxy fallback codes', async () => {
  for (const [status, body, code, message] of [
    [403, '{"code":"permission_denied","message":"admin required"}', 'permission_denied', /admin required/],
    [502, '<html>bad gateway</html>', 'unavailable', /HTTP 502/],
    [504, '<html>gateway timeout</html>', 'unavailable', /HTTP 504/],
    [500, '<html>server failure</html>', 'internal', /HTTP 500/],
  ]) {
    await assert.rejects(create({ fetch: async () => response(status, body) }).namespaces(), checkError(code, message));
  }
});

test('malformed HTTP 401 bodies still expire sessions, except login failures', async () => {
  const src = create({ fetch: async () => response(401, '<html>unauthorized</html>') });
  const auth = []; src.onAuth((u) => auth.push(u));
  await assert.rejects(src.session(), checkError('unauthenticated', /HTTP 401/));
  assert.deepEqual(auth, [null]);
  await assert.rejects(src.login('ann', 'wrong'), checkError('unauthenticated', /HTTP 401/));
  assert.deepEqual(auth, [null]);
});
