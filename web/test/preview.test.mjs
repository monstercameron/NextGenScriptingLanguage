import test from 'node:test';
import assert from 'node:assert/strict';
import { decodePreview } from '../preview.mjs';

test('dashboard shows bidi controls as escapes without changing the saved data', () => {
  const original = { title: 'safe\u202eevil', status: '\u2066OK\u2069', notes: ['\u061cvisible'] };
  const raw = JSON.stringify(original);
  assert.deepEqual(decodePreview(raw), {
    title: 'safe\\u202eevil',
    status: '\\u2066OK\\u2069',
    notes: ['\\u061cvisible'],
    metrics: [],
  });
  assert.deepEqual(JSON.parse(raw), original);
});

test('dashboard accepts derived counts and preserves literal text without interpreting markup', () => {
  const model = {
    title: '<img src=x onerror=alert(1)>',
    status: 'HOLD',
    metrics: [
      { label: 'api errors', value: 17 },
      { label: 'workers', value: 0 },
    ],
    notes: ['Change the input and rerun.'],
  };
  assert.deepEqual(decodePreview(JSON.stringify(model)), model);
});
test('dashboard rejects excessive, fractional, negative or nonnumeric data and text', () => {
  const base = { title: 'Operations', status: 'READY' };
  for (const value of [-1, 1.5, '2', null, 1000000001])
    assert.throws(
      () => decodePreview(JSON.stringify({ ...base, metrics: [{ label: 'count', value }] })),
      /metrics/,
    );
  assert.throws(
    () =>
      decodePreview(
        JSON.stringify({ ...base, metrics: Array(9).fill({ label: 'count', value: 1 }) }),
      ),
    /metrics/,
  );
  assert.throws(
    () => decodePreview(JSON.stringify({ ...base, notes: ['x'.repeat(201)] })),
    /notes/,
  );
  assert.throws(() => decodePreview(JSON.stringify({ ...base, title: 'x'.repeat(121) })), /title/);
  assert.throws(
    () => decodePreview(JSON.stringify({ ...base, extra: 'x'.repeat(16384) })),
    /16 KiB/,
  );
  assert.throws(
    () => decodePreview(JSON.stringify({ ...base, extra: '😀'.repeat(5000) })),
    /16 KiB/,
  );
  assert.throws(() => decodePreview('null'), /title/);
});
