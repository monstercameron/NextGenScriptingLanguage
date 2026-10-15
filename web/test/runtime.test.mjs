import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { connect } from '../abi.mjs';
const root = fileURLToPath(new URL('../../', import.meta.url));
const bytes = await readFile(path.join(root, 'docs/playground/execute.wasm'));
const module = await WebAssembly.compile(bytes);
const examples = JSON.parse(await readFile(new URL('../examples.json', import.meta.url)));
const fixtures = JSON.parse(await readFile(new URL('../fixtures.json', import.meta.url)));
async function session() {
  return connect(await WebAssembly.instantiate(module, {}));
}
async function seeded() {
  const call = await session();
  for (const [path, content] of Object.entries(fixtures))
    assert.equal(call({ action: 'write', path, content }).ok, true);
  return call;
}
const run = (call, source, extra = {}) => call({ action: 'run', source, ...extra });
function good(response) {
  assert.equal(response.ok, true, response.error);
  return response;
}
function bad(response, pattern) {
  assert.equal(response.ok, false);
  assert.match(response.error, pattern);
  return response;
}

test('WASM is a real import-free executable module, with owned-buffer ABI', () => {
  assert.equal(bytes.subarray(0, 4).toString('hex'), '0061736d');
  assert.deepEqual(WebAssembly.Module.imports(module), []);
  for (const name of ['allocate', 'execute', 'release', 'memory'])
    assert.ok(WebAssembly.Module.exports(module).some((item) => item.name === name));
});
test('write receipts distinguish this execution from retained artifacts, including identical rewrites', async () => {
  const call = await session();
  const source = 'run "printf" with arguments ["dashboard\\n"]\nthen save to "dashboard.json"';
  const first = good(run(call, source));
  assert.deepEqual(first.written_files, ['/workspace/dashboard.json']);
  const second = good(run(call, source));
  assert.deepEqual(second.written_files, ['/workspace/dashboard.json']);
  const withoutWrite = good(run(call, 'run "true"'));
  assert.equal(withoutWrite.files['/workspace/dashboard.json'], 'dashboard\n');
  assert.deepEqual(withoutWrite.written_files, []);
  const failed = bad(
    run(call, 'run "printf" with arguments ["x"]\nthen save to "/missing/out"'),
    /parent directory/,
  );
  assert.deepEqual(failed.written_files, []);
});
for (const example of examples) {
  test(`WASM example: ${example.id}`, async () => {
    const call = await seeded();
    const result = good(run(call, example.source, { stdin: example.stdin ?? '' }));
    assert.equal(result.stdout, example.expected);
    for (const [file, content] of Object.entries(example.expectedFiles ?? {}))
      assert.equal(result.files['/workspace/' + file], content, `${example.id}: ${file}`);
  });
  test(`native/WASM exact output parity: ${example.id}`, async () => {
    const binary = process.env.EXECUTE_TEST_BINARY ?? path.join(root, 'target/release/execute');
    const folder = await mkdtemp(path.join(tmpdir(), 'execute-wasm-parity-'));
    try {
      for (const [file, content] of Object.entries(fixtures))
        await writeFile(path.join(folder, file), content);
      await writeFile(path.join(folder, example.file), example.source);
      const result = spawnSync(
        binary,
        [example.file, '--manifest-dir', path.join(root, 'manifests')],
        {
          cwd: folder,
          input: example.stdin ?? '',
          encoding: 'utf8',
          timeout: 5000,
          env: { ...process.env, LC_ALL: 'C' },
        },
      );
      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.stdout, example.expected);
      for (const [file, content] of Object.entries(example.expectedFiles ?? {}))
        assert.equal(
          await readFile(path.join(folder, file), 'utf8'),
          content,
          `${example.id}: ${file}`,
        );
    } finally {
      await rm(folder, { recursive: true, force: true });
    }
  });
}
test('incident dashboard responds to edited evidence with independently counted metrics', async () => {
  const call = await seeded();
  const example = examples.find((item) => item.id === 'incident-command');
  const added = fixtures['incident-eu-west.log'] + 'ERROR eu-west cache synthetic outage\n';
  good(call({ action: 'write', path: 'incident-eu-west.log', content: added }));
  const result = good(run(call, example.source));
  const all = [fixtures['incident-us-east.log'], added, fixtures['incident-ap-south.log']].join('');
  const lines = all.trimEnd().split('\n');
  const dashboard = JSON.parse(result.files['/workspace/' + example.previewFile]);
  assert.deepEqual(
    dashboard.metrics.map((item) => item.value),
    [
      lines.length,
      lines.filter((line) => line.includes('ERROR')).length,
      lines.filter((line) => line.includes('WARN')).length,
    ],
  );
  assert.equal(result.files['/workspace/incident-region-eu-west.txt'], '11 | eu-west\n');
});
test('launch gate changes every regional checklist and the dashboard from hold to canary', async () => {
  const call = await seeded();
  const example = examples.find((item) => item.id === 'launch-control');
  const candidate = JSON.parse(fixtures['launch-candidate.json']);
  candidate.checks_passed = true;
  candidate.tests_failed = 0;
  good(
    call({ action: 'write', path: 'launch-candidate.json', content: JSON.stringify(candidate) }),
  );
  const result = good(run(call, example.source));
  const dashboard = JSON.parse(result.files['/workspace/' + example.previewFile]);
  assert.equal(dashboard.status, 'READY FOR CANARY');
  assert.equal(dashboard.metrics[1].value, 0);
  for (const file of candidate.plan_files)
    assert.match(result.files['/workspace/' + file], /Decision: READY FOR CANARY/);
});
test('removing priority requests updates support routing and leaves an empty urgent artifact', async () => {
  const call = await seeded();
  const example = examples.find((item) => item.id === 'support-signal');
  const lines = fixtures['support-inbox.txt']
    .trimEnd()
    .split('\n')
    .filter((line) => !line.includes('P1'));
  good(call({ action: 'write', path: 'support-inbox.txt', content: lines.join('\n') + '\n' }));
  const result = good(run(call, example.source));
  const dashboard = JSON.parse(result.files['/workspace/' + example.previewFile]);
  assert.equal(dashboard.status, 'ROUTINE');
  assert.deepEqual(
    dashboard.metrics.map((item) => item.value),
    [
      lines.length,
      0,
      lines.filter((line) => line.includes('SECURITY')).length,
      lines.filter((line) => line.includes('BILLING')).length,
    ],
  );
  assert.equal(result.files['/workspace/support-urgent.txt'], '');
});
test('typed CLI values control branch; invalid input fails before effects', async () => {
  const call = await seeded();
  const source =
    'accept count as integer\nif count is 2\n  run printf with message "two"\notherwise\n  run printf with message "other"';
  assert.equal(good(run(call, source, { inputs: { count: '2' } })).stdout, 'two\n');
  assert.equal(good(run(call, source, { inputs: { count: '3' } })).stdout, 'other\n');
  bad(run(call, source, { inputs: { count: '2x' } }), /integer/);
  bad(run(call, source, { inputs: { unknown: '2' } }), /unknown/);
});

test('loops preserve zero input, update typed status and nest through calls', async () => {
  const call = await seeded();
  assert.equal(
    good(run(call, 'repeat 0 times\n  run "false"\nthen "printf" with arguments ["empty"]')).stdout,
    'empty',
  );
  const source =
    'run "true" and keep status as code\nwhile code is 0\n  run "false" and keep status as code\nthen encode code as json';
  assert.equal(good(run(call, source)).stdout, '1');
  const nested =
    'define a program named emit\n  repeat 2 times\n    then encode 42 as json\nrepeat 3 times\n  run program emit';
  assert.equal(good(run(call, nested)).stdout, '42');
  const batch = good(run(call, examples.find((item) => item.id === 'loops').source));
  assert.equal(batch.files['/workspace/tasks-one.txt'], 'alpha\nzulu\n');
  assert.equal(batch.files['/workspace/tasks-two.txt'], 'bravo\ndelta\n');
});

test('loop bodies and known invalid counts fail before virtual file effects', async () => {
  const call = await seeded();
  const prefix = 'run "printf" with arguments ["forbidden"]\nthen save to "forbidden.txt"\n';
  for (const body of [
    'repeat 0 times\n  run nonexistent_applet',
    'repeat -1 times\n  run "true"',
    'for each file in [1]\n  run "true"',
  ]) {
    bad(run(call, prefix + body), /count|text|unknown/);
    assert.ok(!good(call({ action: 'inspect' })).files['/workspace/forbidden.txt']);
  }
  bad(
    run(call, 'accept count as integer\n' + prefix + 'repeat count times\n  run "true"', {
      inputs: { count: '-1' },
    }),
    /count/,
  );
  assert.ok(!good(call({ action: 'inspect' })).files['/workspace/forbidden.txt']);
});

test('loop trace stays bounded and iteration-budget errors preserve worker usability', async () => {
  const call = await seeded();
  const complete = good(run(call, 'repeat 100000 times\n  run "true"'));
  assert.equal(complete.applet_calls, 100000);
  assert.ok(complete.trace.length <= 4096);
  assert.equal(complete.trace.at(-1).operation, 'trace truncated');
  assert.ok(Buffer.byteLength(JSON.stringify(complete.trace)) <= 1024 * 1024);
  const oversized = good(
    run(
      call,
      'accept message as text\nrepeat 100 times\n  run "printf" with arguments ["%s", message]',
      { inputs: { message: 'x'.repeat(64 * 1024) } },
    ),
  );
  assert.ok(Buffer.byteLength(JSON.stringify(oversized.trace)) <= 1024 * 1024);
  assert.equal(oversized.trace.at(-1).operation, 'trace truncated');
  bad(run(call, 'while true is true\n  run "true"'), /iteration budget/);
  assert.equal(good(run(call, 'then encode 7 as json')).stdout, '7');
});
test('trace byte budget preserves exact UTF8 and escaped JSON boundaries per request', async () => {
  const call = await session();
  const source =
    'accept message as text\nrepeat 200 times\n  run "printf" with arguments ["%s", message]';
  for (const message of ['small', '雪'.repeat(4000), '"\\\u0001\n\r\t\b\f'.repeat(2000)]) {
    for (let repeat = 0; repeat < 2; repeat++) {
      const response = good(run(call, source, { inputs: { message } }));
      assert.equal(response.stdout, message);
      assert.equal(response.applet_calls, 200);
      assert.equal(response.trace[0].argv[1], message);
      const entryBytes = Buffer.byteLength(JSON.stringify(response.trace[0]));
      const accepted = Math.min(200, 4095, Math.floor((1024 * 1024 - 1024) / (entryBytes + 1)));
      assert.equal(response.trace.length, accepted + (accepted < 200 ? 1 : 0));
      if (accepted < 200) assert.equal(response.trace.at(-1).operation, 'trace truncated');
      assert.ok(Buffer.byteLength(JSON.stringify(response.trace)) <= 1024 * 1024);
    }
  }
});
test('validation rejects undefined names, branch-dependent bindings and type mismatch', async () => {
  const call = await seeded();
  for (const source of [
    'run printf with message missing',
    'if true is true\n  run printf with message "yes" as result\nrun printf with message result',
    'if true is 2\n  run pwd',
  ])
    bad(run(call, source), /undefined|type|compares/);
});
test('unavailable commands and triggers fail before writes', async () => {
  const call = await seeded();
  const before = call({ action: 'inspect' }).files;
  bad(
    run(
      call,
      'run printf with message "effect"\nthen save to "effect.txt"\nrun "git" with arguments ["status"]',
    ),
    /unavailable/,
  );
  assert.deepEqual(call({ action: 'inspect' }).files, before);
  bad(run(call, 'every 1 second\n  run pwd'), /finite programs/);
  bad(run(call, 'listen to stdin\nwhenever a line arrives as event\n  run pwd'), /finite programs/);
});
test('virtual files, literal argv, Unicode, no command substitution, no host escape', async () => {
  const call = await session();
  good(call({ action: 'write', path: 'café.txt', content: 'héllo\n' }));
  assert.equal(good(run(call, 'run cat with path "café.txt"')).stdout, 'héllo\n');
  assert.equal(
    good(run(call, 'run "echo" with arguments ["$(rm -rf /)", ";", "*.txt"]')).stdout,
    '$(rm -rf /) ; *.txt\n',
  );
  bad(call({ action: 'write', path: '../../host', content: 'no' }), /escapes/);
  bad(call({ action: 'command', command: 'cat /etc/passwd' }), /does not exist/);
  bad(call({ action: 'write', path: '/workspace', content: 'no' }), /directory/);
});
test('cache hits persist across runs and invalidate on changed input', async () => {
  const call = await seeded();
  const source = examples.find((item) => item.id === 'cache').source;
  assert.equal(good(run(call, source)).cache_hits, 0);
  assert.equal(good(run(call, source)).cache_hits, 1);
  good(call({ action: 'write', path: 'app.log', content: 'changed\n' }));
  const changed = good(run(call, source));
  assert.equal(changed.cache_hits, 0);
  assert.equal(changed.stdout, 'changed\n');
  good(call({ action: 'reset' }));
  assert.deepEqual(call({ action: 'inspect' }).files, {});
});
test('reusable calls restore lexical bindings and working directory', async () => {
  const call = await seeded();
  const source =
    'define a program named inner\n  accept message as text\n  run cd with path "/"\n  run printf with message message\naccept message as text with default "parent"\nrun program inner with message "child" as child\nrun printf with message message';
  assert.equal(good(run(call, source)).stdout, 'parent\n');
  assert.equal(good(run(call, 'run pwd')).stdout, '/workspace\n');
});
test('JSON preserves types, escapes pointers and rejects absent fields', async () => {
  const call = await session();
  good(call({ action: 'write', path: 'value.json', content: '{"a/b":{"~x":42},"fraction":1.25}' }));
  const prefix = 'read "value.json"\nthen parse json as document\n';
  assert.equal(
    good(
      run(call, prefix + 'take integer at "/a~1b/~0x" from document as n\nthen encode n as json'),
    ).stdout,
    '42',
  );
  bad(run(call, prefix + 'take integer at "/fraction" from document as n'), /integer/);
  bad(run(call, prefix + 'take text at "/missing" from document as n'), /does not exist/);
});
test('browser flag subsets are explicit and failed statuses are capturable', async () => {
  const call = await seeded();
  bad(run(call, 'read "app.log"\nthen grep with pattern "ERROR"'), /requires -F/);
  bad(run(call, 'read "app.log"\nthen sort with flags ["-n"]'), /supports/);
  assert.equal(
    good(run(call, 'run "false" and keep status as code\nthen encode code as json')).stdout,
    '1',
  );
  bad(run(call, 'run "false"'), /status 1/);
});
test('terminal quoting, malformed commands, paths and cd persistence', async () => {
  const call = await session();
  assert.equal(
    good(call({ action: 'command', command: 'echo "two words" \'雪\'' })).stdout,
    'two words 雪\n',
  );
  bad(call({ action: 'command', command: 'echo "open' }), /unterminated/);
  good(call({ action: 'command', command: 'mkdir -p work/nested' }));
  good(call({ action: 'command', command: 'cd work/nested' }));
  assert.equal(
    good(call({ action: 'command', command: 'pwd' })).stdout,
    '/workspace/work/nested\n',
  );
});
test('terminal execute loads virtual program and accepts typed inputs', async () => {
  const call = await session();
  good(
    call({
      action: 'write',
      path: 'hello.program',
      content: 'accept name as text\nrun printf with message name',
    }),
  );
  assert.equal(
    good(call({ action: 'command', command: 'execute hello.program --input "name=Ada Lovelace"' }))
      .stdout,
    'Ada Lovelace\n',
  );
  assert.match(
    good(call({ action: 'command', command: 'execute check hello.program' })).stdout,
    /checks passed/,
  );
  assert.match(
    good(call({ action: 'command', command: 'execute explain hello.program' })).stdout,
    /accept name/,
  );
  bad(
    call({ action: 'command', command: 'execute hello.program --input name=a --input name=b' }),
    /duplicate/,
  );
});
test('size limits and parse errors remain recoverable; empty output is valid', async () => {
  const call = await session();
  bad(call({ action: 'write', path: 'large', content: 'a'.repeat(1048577) }), /limit/);
  bad(run(call, '#'.repeat(65537)), /64 KiB/);
  bad(run(call, 'run "printf" with arguments ["%999999999s", "a"]'), /supports/);
  bad(run(call, 'not valid syntax'), /line/);
  assert.equal(good(run(call, 'run "true"')).stdout, '');
});
test('head zero, no final newline, uniq sentinel and binary-safe cat', async () => {
  const call = await session();
  assert.equal(
    good(run(call, 'read from stdin\nthen head with lines 0', { stdin: 'hi\n' })).stdout,
    '',
  );
  assert.equal(
    good(run(call, 'read from stdin\nthen uniq', { stdin: 'a\n\u0000sentinel' })).stdout,
    'a\n\u0000sentinel\n',
  );
  assert.equal(
    good(run(call, 'read from stdin\nthen "cat"', { stdin: 'a\u0000b' })).stdout,
    'a\u0000b',
  );
});
test('repeated ABI calls release memory instead of retaining response buffers', async () => {
  const instance = await WebAssembly.instantiate(module, {});
  const call = connect(instance);
  for (let at = 0; at < 1500; at++) good(run(call, 'run printf with message "hello"'));
  assert.ok(instance.exports.memory.buffer.byteLength < 16 * 1024 * 1024);
});

test('WASM memory is capped and direct cd cannot mutate terminal state', async () => {
  const instance = await WebAssembly.instantiate(module, {});
  assert.throws(() => instance.exports.memory.grow(1025), RangeError);
  const call = connect(instance);
  bad(run(call, 'run "cd" with arguments ["/"]'), /interpreter builtin/);
  assert.equal(good(call({ action: 'command', command: 'pwd' })).stdout, '/workspace\n');
});

test('build receipt binds source and WASM; local assets and full licenses ship together', async () => {
  const { createHash } = await import('node:crypto');
  const receipt = JSON.parse(await readFile(path.join(root, 'docs/playground/build.json'), 'utf8'));
  assert.equal(createHash('sha256').update(bytes).digest('hex'), receipt.wasm_sha256);
  for (const [name, digest] of Object.entries(receipt.source_sha256))
    assert.equal(
      createHash('sha256')
        .update(await readFile(path.join(root, name)))
        .digest('hex'),
      digest,
      name,
    );
  const notices = await readFile(path.join(root, 'docs/playground/THIRD_PARTY.txt'), 'utf8');
  for (const text of [
    '@xterm/xterm 6.0.0',
    '@xterm/addon-fit 0.11.0',
    'Permission is hereby granted',
    'serde',
  ])
    assert.ok(notices.includes(text));
  const html = await readFile(path.join(root, 'docs/playground/index.html'), 'utf8');
  for (const match of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
    const url = match[1];
    if (!url.startsWith('#') && !url.includes('://')) {
      const target = new URL(url, new URL('../../docs/playground/index.html', import.meta.url));
      if (target.pathname.endsWith('/')) target.pathname += 'index.html';
      await readFile(fileURLToPath(target));
    }
  }
});
