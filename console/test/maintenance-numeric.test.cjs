'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const U = require('../src/shared.js');

test('integer edits reject rounding instead of serializing a changed number', () => {
  for (const kind of [null, 'Int']) {
    for (const s of ['9007199254740992', '9007199254740993', '-9007199254740993', '9223372036854775807', '-9223372036854775808']) {
      assert.throws(() => U.parseValue(s, kind), /Integer edits must be between/);
    }
    for (const n of [Number.MIN_SAFE_INTEGER, -1, 0, Number.MAX_SAFE_INTEGER]) {
      assert.deepEqual(U.parseValue(String(n), kind), { Int: n });
    }
  }
});

test('float overflow and explicit non-finite numeric edits are actionable errors', () => {
  for (const kind of [null, 'Int', 'Float']) {
    for (const s of ['1e309', '-1e309', 'Infinity', '-Infinity', 'NaN']) {
      assert.throws(() => U.parseValue(s, kind), /finite/);
    }
  }
  assert.deepEqual(U.parseValue('1e308', 'Float'), { Float: 1e308 });
  assert.deepEqual(U.parseValue('1.25', 'Int'), { Float: 1.25 });
  assert.deepEqual(U.parseValue('9007199254740993', 'String'), { String: '9007199254740993' });
});

test('nested numeric edits fail before staging rather than becoming strings', () => {
  for (const s of ['[9007199254740993]', '{"nested":[-9007199254740993]}', '[1e309]', '{"n":1e20}']) {
    assert.throws(() => U.parseValue(s), RangeError);
  }
  assert.deepEqual(U.parseValue('{"9007199254740993":["9007199254740993",1.5,2]}'), {
    Dict: { '9007199254740993': { List: [{ String: '9007199254740993' }, { Float: 1.5 }, { Int: 2 }] } },
  });
  assert.deepEqual(U.parseValue('[broken'), { String: '[broken' });
  assert.throws(() => U.plainToValue({ nested: [Infinity] }), /finite/);
  assert.throws(() => U.plainToValue([Number.MAX_SAFE_INTEGER + 1]), /Integer edits/);
});
