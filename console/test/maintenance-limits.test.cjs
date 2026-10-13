'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const Q = require('../src/query.js');

test('query limits preserve off, omitted limits and positive safe integer boundaries', () => {
  assert.deepEqual(Q.parse('node a').opts, {});
  assert.equal(Q.parse('\\limit off\nnode a').opts.limit, null);
  for (const [input, expected] of [['1', 1], ['00040', 40], ['9007199254740990', 9007199254740990], ['9007199254740991', Number.MAX_SAFE_INTEGER]]) {
    assert.equal(Q.parse(`\\limit ${input}\nnode a`).opts.limit, expected, input);
  }
});

test('query limits reject unsafe and overflowing decimal integers before execution', () => {
  for (const input of ['0', '000', '9007199254740992', '9007199254740993', '18446744073709551615', '9'.repeat(400)]) {
    assert.throws(() => Q.parse(`\\limit ${input}\nfind {"Const":true}`), (error) => {
      assert.ok(error instanceof Q.QueryError);
      assert.equal(error.code, 'invalid_argument');
      assert.match(error.message, /positive safe integer/);
      return true;
    }, input);
  }
});

test('query limits continue to reject non-integer and non-finite spellings', () => {
  for (const input of ['-1', '1.5', '1e3', 'Infinity', 'NaN', '+1', '0x10']) {
    assert.throws(() => Q.parse(`\\limit ${input}\nnode a`), (error) => error instanceof Q.QueryError && error.code === 'invalid_argument', input);
  }
});
