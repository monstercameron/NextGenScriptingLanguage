import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { connect } from '../abi.mjs';

const module = await WebAssembly.compile(
  await readFile(
    process.env.EXECUTE_TEST_WASM ?? new URL('../../docs/playground/execute.wasm', import.meta.url),
  ),
);
const session = async () => connect(await WebAssembly.instantiate(module, {}));
const run = (call, source) => {
  const response = call({ action: 'run', source });
  assert.equal(response.ok, true, response.error);
  return response;
};

test('G1 typed values and aliases bind without applet calls', async () => {
  const call = await session();
  const response = run(
    call,
    'use value 7 as count\nuse value {"count": count} as record\nuse value record as alias\nthen encode alias as json',
  );
  assert.equal(response.stdout, '{"count":7}');
  assert.equal(response.applet_calls, 0);
});

test('G1 bound native output and status stay silent, explicit reuse emits', async () => {
  const call = await session();
  assert.equal(run(call, 'run "printf" with arguments ["internal"] as name').stdout, '');
  assert.equal(
    run(call, 'run "printf" with arguments ["internal"] and keep status as code').stdout,
    '',
  );
  assert.equal(run(call, 'use value "visible" as name\nuse value name').stdout, 'visible');
});

test('G1 parsed JSON is internal while unconsumed block output streams', async () => {
  const call = await session();
  assert.equal(
    run(call, 'run "printf" with arguments ["{}"]\nthen parse json as record').stdout,
    '',
  );
  assert.equal(
    run(
      call,
      'if true is true\n  run "printf" with arguments ["visible"]\nuse value "internal" as name',
    ).stdout,
    'visible',
  );
});

test('G1 a reachable pipeline consumer still receives bound bytes', async () => {
  const call = await session();
  assert.equal(
    run(call, 'use value "abc" as name\npipe to "wc" with arguments ["-c"]').stdout.trim(),
    '3',
  );
  assert.equal(
    run(
      call,
      'if true is true\n  run "printf" with arguments ["abc"]\nthen "wc" with arguments ["-c"]',
    ).stdout.trim(),
    '3',
  );
});

test('G1 reusable returns distinguish captured results from native stdout', async () => {
  const call = await session();
  const definition = 'define a program named emit\n  run "printf" with arguments ["visible"]\n';
  assert.equal(run(call, definition + 'run program emit as captured').stdout, '');
  assert.equal(run(call, definition + 'run program emit').stdout, 'visible');
  assert.equal(
    run(call, definition + 'run program emit as captured\nuse value captured').stdout,
    'visible',
  );
});

test('G1 accumulated browser stdout remains bounded', async () => {
  const call = await session();
  const response = call({
    action: 'run',
    source: 'repeat 2000 times\n  run "printf" with arguments ["' + 'x'.repeat(1024) + '"]',
  });
  assert.equal(response.ok, false);
  assert.match(response.error, /stdout exceeds 1 MiB/);
  assert.ok(Buffer.byteLength(response.stdout) <= 1024 * 1024);
});
