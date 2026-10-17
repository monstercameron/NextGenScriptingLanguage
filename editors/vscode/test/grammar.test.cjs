'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { test, before } = require('node:test');
const { Registry, parseRawGrammar, INITIAL } = require('vscode-textmate');
const onig = require('vscode-oniguruma');
const extension = path.resolve(__dirname, '..');
const project = path.resolve(extension, '../..');
const manifest = JSON.parse(fs.readFileSync(path.join(extension, 'package.json'), 'utf8'));
const configuration = JSON.parse(fs.readFileSync(path.join(extension, 'language-configuration.json'), 'utf8'));
let grammar;
before(async () => {
  const wasm = fs.readFileSync(require.resolve('vscode-oniguruma/release/onig.wasm'));
  await onig.loadWASM(wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength));
  const registry = new Registry({
    onigLib: Promise.resolve({ createOnigScanner: sources => new onig.OnigScanner(sources), createOnigString: value => new onig.OnigString(value) }),
    loadGrammar: async scope => scope === 'source.execute'
      ? parseRawGrammar(fs.readFileSync(path.join(extension, 'syntaxes/execute.tmLanguage.json'), 'utf8'), 'execute.tmLanguage.json') : null
  });
  grammar = await registry.loadGrammar('source.execute');
  assert.ok(grammar);
});
function tokenize(line, state = INITIAL) {
  return grammar.tokenizeLine(line, state, 1000);
}
function scopesAt(line, text, occurrence = 0) {
  let offset = -1;
  for (let i = 0; i <= occurrence; i++) offset = line.indexOf(text, offset + 1);
  assert.notEqual(offset, -1, `Missing ${JSON.stringify(text)} in ${JSON.stringify(line)}`);
  const result = tokenize(line);
  const token = result.tokens.find(token => token.startIndex <= offset && token.endIndex > offset);
  assert.ok(token, `No token at ${offset}`);
  return token.scopes;
}
function scope(line, text, expected, occurrence = 0) {
  const scopes = scopesAt(line, text, occurrence);
  assert.ok(scopes.includes(expected + '.execute'), `${JSON.stringify(text)} in ${line}: ${scopes.join(', ')}`);
}
function walk(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const child = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(child) : [child];
  });
}
test('registers a declarative .program language without an activation entrypoint', () => {
  assert.equal(manifest.contributes.languages[0].id, 'execute-program');
  assert.deepEqual(manifest.contributes.languages[0].extensions, ['.program']);
  assert.equal(manifest.contributes.grammars[0].scopeName, 'source.execute');
  assert.equal(manifest.main, undefined);
  assert.equal(manifest.browser, undefined);
  assert.equal(manifest.activationEvents, undefined);
  assert.equal(manifest.contributes.commands, undefined);
});
test('highlights native stages, dotted and multiword applets, parameters and bindings', () => {
  for (const [line, applet] of [
    ['run git.commit with message "ship"', 'git.commit'],
    ['pipe to wc with flags ["-l"]', 'wc'],
    ['listen to tail with path "events.log"', 'tail'],
    ['stream from cat with path "events.log"', 'cat'],
    ['run count lines as total', 'count lines'],
    ['uppercase', 'uppercase']
  ]) scope(line, applet, 'entity.name.function.applet');
  scope('run printf with message "hello" as output', 'message', 'variable.parameter');
  scope('run printf with message "hello" as output', 'output', 'variable.other.definition');
  scope('then tr with from "a-z" and to "A-Z" and cache', 'cache', 'storage.modifier.cache');
});
test('quoted executable calls preserve function, string, parameter and binding scopes', () => {
  for (const prefix of ['run', 'then', 'pipe to', 'listen to', 'stream from']) {
    const line = `${prefix} "./tool with spaces" with arguments ["# literal", message] as output`;
    scope(line, './tool', 'entity.name.function.applet');
    scope(line, './tool', 'string.quoted.double');
    scope(line, 'arguments', 'variable.parameter');
    scope(line, '# literal', 'string.quoted.double');
    scope(line, 'message', 'variable.other.readwrite');
    scope(line, 'output', 'variable.other.definition');
  }
});
test('keyword-shaped applets, parameter names and references retain their role', () => {
  scope('then false', 'false', 'entity.name.function.applet');
  scope('run as with path "input.s" and cache false as result', 'as', 'entity.name.function.applet');
  scope('run as with path "input.s" and cache false as result', 'cache', 'variable.parameter');
  scope('run as with path "input.s" and cache false as result', 'false', 'constant.language.boolean');
  scope('run worker with first with and second 2', 'with', 'variable.other.readwrite', 1);
  scope('run worker with first and and second 2', 'and', 'variable.other.readwrite');
  scope('run worker with first as and second cache', 'as', 'variable.other.readwrite');
});
test('top-level program inputs distinguish types, defaults and values from component ports', () => {
  for (const type of ['text', 'integer', 'boolean', 'list of text', 'json']) {
    const line = `accept setting as ${type} with default []`;
    scope(line, 'accept', 'keyword.declaration.input');
    scope(line, 'setting', 'variable.parameter.input');
    scope(line, type, 'support.type.input');
    scope(line, 'default', 'keyword.operator.default');
  }
  scope('  accept incoming as text', 'incoming', 'variable.parameter.port');
});
test('strings are literal and JSON escapes are distinct from comments and keywords', () => {
  const line = String.raw`run printf with message "# then $HOME $(pwd) \"quoted\" \\ \n \u0041" # actual comment`;
  scope(line, '# then', 'string.quoted.double');
  scope(line, '$HOME', 'string.quoted.double');
  scope(line, '$(pwd)', 'string.quoted.double');
  scope(line, String.raw`\u0041`, 'constant.character.escape');
  scope(line, '# actual', 'comment.line.number-sign');
  scope(String.raw`run printf with message "bad\q"`, String.raw`\q`, 'invalid.illegal.escape');
});
test('comments and malformed single-line strings never leak into subsequent code', () => {
  for (const line of ['# "open string', 'run printf with message "unterminated', 'run cat with files ["unterminated', 'read "trailing\\']) {
    const first = tokenize(line);
    const second = tokenize('run pwd', first.ruleStack);
    assert.ok(second.tokens.some(token => token.scopes.includes('entity.name.function.applet.execute')));
    assert.ok(second.tokens.every(token => !token.scopes.some(name => /^(string|comment)\./.test(name))));
  }
});
test('numbers, booleans and list punctuation use distinct scopes', () => {
  const line = 'run sample with flags ["-n", 0, -42, 7, true, false, name]';
  scope(line, '[', 'punctuation.section.brackets.begin');
  scope(line, ',', 'punctuation.separator.sequence');
  scope(line, ']', 'punctuation.section.brackets.end');
  for (const value of ['0', '-42', '7']) scope(line, value, 'constant.numeric.integer');
  for (const value of ['true', 'false']) scope(line, value, 'constant.language.boolean');
  scope(line, 'name', 'variable.other.readwrite');
});
test('highlights lifetime declarations and stream directives', () => {
  scope('listen to stdin', 'stdin', 'constant.language.stdin');
  scope('whenever a line arrives as message', 'message', 'variable.other.definition');
  scope('every 30 seconds', 'every', 'keyword.control.timer');
  scope('every 30 seconds', 'seconds', 'support.type.duration');
  scope('on schedule "0 9 * * 1-5"', 'on schedule', 'keyword.control.schedule');
  scope('take the first 3 lines', 'take', 'keyword.control.limit');
  scope('show each line', 'show each line', 'keyword.control.output');
});
test('highlights finite result-event graph projections', () => {
  const line = 'emit result from normalized to changed';
  scope(line, 'emit', 'keyword.control.graph');
  scope(line, 'result', 'keyword.other.result');
  scope(line, 'normalized', 'variable.other.graph-node');
  scope(line, 'changed', 'variable.other.graph-port');
});
test('highlights components, all nine widget types, ports, and UI event actions', () => {
  scope('define a program named monitor', 'monitor', 'entity.name.type.component');
  for (const type of ['heading', 'text field', 'list', 'table', 'text panel', 'badge', 'button', 'line chart', 'bar chart']) {
    scope(`  show a ${type} named status`, type, 'support.type.widget');
    scope(`  show a ${type} named status`, 'status', 'entity.name.tag.widget');
  }
  scope('  accept repository as text', 'repository', 'variable.parameter.port');
  scope('  offer repository as text', 'text', 'support.type.text');
  scope('connect processes from monitor to processes in details', 'monitor', 'variable.other.component');
  scope('place monitor on the left', 'left', 'constant.language.position');
  scope('  whenever refresh is pressed', 'is pressed', 'keyword.control.event');
  scope('    then show the result as rows in records', 'show', 'keyword.control.action');
  scope('    then show the result as rows in records', 'the result', 'keyword.other.action');
  scope('    run ps with flags ["-A"]', 'ps', 'entity.name.function.applet');
});
test('only unsupported block headers are marked invalid; while is supported', () => {
  for (const keyword of ['else', 'for', 'loop']) {
    scope(`${keyword} something`, keyword, 'invalid.illegal.unsupported-control');
    scope(`run ${keyword}`, keyword, 'entity.name.function.applet');
  }
  scope('while status is 0', 'while', 'keyword.control.condition');
  scope('run while', 'while', 'entity.name.function.applet');
});
test('editor configuration uses two-space blocks and list/object delimiters', () => {
  assert.equal(configuration.comments.lineComment, '#');
  assert.deepEqual(configuration.brackets, [['[', ']'], ['{', '}']]);
  assert.equal(configuration.folding.offSide, true);
  assert.equal(manifest.contributes.configurationDefaults['[execute-program]']['editor.tabSize'], 2);
  const indent = new RegExp(configuration.onEnterRules[0].beforeText);
  for (const line of ['define a program named app', '  whenever refresh is pressed', 'every 30 seconds', '  show a list named files', '  show a table named rows', '  show a bar chart named cpu']) assert.ok(indent.test(line), line);
  for (const line of ['run pwd', '# whenever refresh is pressed', 'run printf with message "every 30 seconds"', '  show a text panel named output']) assert.ok(!indent.test(line), line);
});
test('all real .program files tokenize with bounded lines and closed line states', () => {
  const files = walk(path.join(project, 'examples')).filter(file => file.endsWith('.program'));
  assert.ok(files.length >= 150);
  let lines = 0;
  for (const file of files) {
    let state = INITIAL;
    for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
      const result = tokenize(line, state);
      assert.ok(!result.stoppedEarly, `Tokenization timeout in ${file}`);
      assert.deepEqual(result.tokens, tokenize(line).tokens, `Unexpected line-state carry in ${file}`);
      assert.ok(result.tokens.length <= line.length * 2 + 4);
      state = result.ruleStack;
      lines++;
    }
  }
  console.log(`Tokenized ${files.length} real programs / ${lines} lines`);
});
function expand(body) {
  const defaults = new Map();
  const source = body.join('\n').replace(/\$\{(\d+):([^}]*)\}/g, (_, index, value) => {
    if (!defaults.has(index)) defaults.set(index, value);
    return value;
  });
  return source.replace(/\$(\d+)\b/g, (_, index) => defaults.get(index) || '') + '\n';
}
test('complete snippet defaults pass the actual execute check without running commands', () => {
  const snippets = JSON.parse(fs.readFileSync(path.join(extension, 'snippets/execute.code-snippets'), 'utf8'));
  assert.equal(Object.keys(snippets).length, 35);
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'execute-vscode-'));
  const executable = process.platform === 'win32' ? 'execute.exe' : 'execute';
  const debugBinary = path.join(project, 'target/debug', executable);
  const releaseBinary = path.join(project, 'target/release', executable);
  const binary = process.env.EXECUTE_TEST_BINARY || (fs.existsSync(debugBinary) ? debugBinary : releaseBinary);
  assert.ok(fs.existsSync(binary), 'Build the current runtime with cargo build, or set EXECUTE_TEST_BINARY');
  try {
    let checked = 0;
    for (const [name, snippet] of Object.entries(snippets)) {
      assert.ok(snippet.description && snippet.prefix);
      const source = expand(snippet.body);
      assert.ok(!source.includes('${'), `Unexpanded snippet: ${name}`);
      if (!snippet.isFileTemplate) continue;
      const filename = path.join(temp, 'snippet.program');
      fs.writeFileSync(filename, source);
      const result = spawnSync(binary, ['check', filename, '--manifest-dir', path.join(project, 'manifests')], { cwd: temp, encoding: 'utf8', timeout: 5000 });
      assert.equal(result.status, 0, `${name}: ${result.stderr || result.error}`);
      checked++;
    }
    assert.equal(checked, 27);
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
});

test('large single-line strings and malformed lists recover within the tokenization budget', () => {
  for (const line of [
    'run jq with filter "' + 'x'.repeat(65536) + '"',
    'run cat with files [' + '"path", '.repeat(4096),
    'run ' + 'word '.repeat(8192) + 'with message "done"'
  ]) {
    const result = tokenize(line);
    assert.ok(!result.stoppedEarly, 'Tokenization exceeded one-second safety budget');
    const next = grammar.tokenizeLine('run pwd', result.ruleStack, 1000);
    assert.ok(next.tokens.some(token => token.scopes.includes('entity.name.function.applet.execute')));
  }
});

test('finite stdin is an I/O directive and keeps its optional result binding', () => {
  scope('read from stdin as message', 'read from', 'keyword.control.io');
  scope('read from stdin as message', 'stdin', 'constant.language.stdin');
  scope('read from stdin as message', 'message', 'variable.other.definition');
  scope('read stdin', 'stdin', 'variable.other.readwrite');
});

test('native status capture distinguishes stdout, integer status and keep parameters', () => {
  for (const line of [
    'run "grep" with arguments ["needle", "file"] as matches and keep status as code',
    'run false as matches and keep status as code',
    'false as matches and keep status as code'
  ]) {
    scope(line, 'matches', 'variable.other.definition');
    scope(line, 'keep status', 'storage.modifier.status');
    scope(line, 'code', 'variable.other.definition');
  }
  scope('run false and keep status as code', 'false', 'entity.name.function.applet');
  scope('run worker with value 1 and keep 2', 'keep', 'variable.parameter');
});

test('direct native stderr discard keeps modifier and result binding scopes distinct from applet names and parameters', () => {
  const direct = 'run "./renderer" with text label and discard stderr and keep status as code and keep result as execution';
  scope(direct, './renderer', 'entity.name.function.applet');
  scope(direct, 'text', 'variable.parameter');
  scope(direct, 'label', 'variable.other.readwrite');
  scope(direct, 'and', 'keyword.operator.parameter');
  scope(direct, 'discard', 'storage.modifier.stderr');
  scope(direct, 'stderr', 'storage.modifier.stderr');
  scope(direct, 'keep status', 'storage.modifier.status');
  scope(direct, 'code', 'variable.other.definition');
  scope(direct, 'keep result', 'storage.modifier.result');
  scope(direct, 'execution', 'variable.other.definition');

  const namedParameters = 'run renderer with text label and discard "stderr"';
  scope(namedParameters, 'renderer', 'entity.name.function.applet');
  scope(namedParameters, 'text', 'variable.parameter');
  scope(namedParameters, 'label', 'variable.other.readwrite');
  scope(namedParameters, 'discard', 'variable.parameter');
  scope(namedParameters, 'stderr', 'string.quoted.double');
  assert.ok(!scopesAt(namedParameters, 'discard').includes('storage.modifier.stderr.execute'));
  assert.ok(!scopesAt(namedParameters, 'stderr').includes('storage.modifier.stderr.execute'));

  const namedOperation = 'run renderer and discard stderr';
  scope(namedOperation, 'renderer and discard stderr', 'entity.name.function.applet');
  assert.ok(!scopesAt(namedOperation, 'discard').includes('storage.modifier.stderr.execute'));

  const namedResult = 'run renderer and keep result as execution';
  scope(namedResult, 'renderer', 'entity.name.function.applet');
  scope(namedResult, 'keep result', 'storage.modifier.result');
  scope(namedResult, 'execution', 'variable.other.definition');
});

test('scalar conditions preserve operand roles and indent their branches', () => {
  for (const line of ['if code is 0', '  if enabled is not false', 'if "a b" is "c"', 'if is is true']) {
    scope(line, 'if', 'keyword.control.condition');
  }
  scope('if code is 0', 'code', 'variable.other.readwrite');
  scope('if code is 0', 'is', 'keyword.operator.comparison');
  scope('if code is 0', '0', 'constant.numeric.integer');
  scope('if enabled is not false', 'not', 'keyword.operator.comparison');
  scope('if enabled is not false', 'false', 'constant.language.boolean');
  scope('if is is true', 'is', 'variable.other.readwrite');
  scope('if is is true', 'is', 'keyword.operator.comparison', 1);
  scope('otherwise # fallback', 'otherwise', 'keyword.control.condition');
  const indent = new RegExp(configuration.onEnterRules[0].beforeText);
  assert.ok(indent.test('if code is 0')); assert.ok(indent.test('  otherwise'));
});

test('repeat, foreach and while distinguish headers and indent bodies', () => {
  scope('repeat count times', 'repeat', 'keyword.control.loop');
  scope('repeat count times', 'count', 'variable.other.readwrite');
  scope('repeat count times', 'times', 'keyword.control.loop');
  scope('for each file in files', 'for each', 'keyword.control.loop');
  scope('for each file in files', 'file', 'variable.other.definition');
  scope('for each file in files', 'in', 'keyword.operator.loop');
  scope('for each file in files', 'files', 'variable.other.readwrite');
  scope('while status is not 0', 'while', 'keyword.control.condition');
  scope('while status is not 0', 'status', 'variable.other.readwrite');
  scope('while status is not 0', 'not', 'keyword.operator.comparison');
  const indent = new RegExp(configuration.onEnterRules[0].beforeText);
  for (const line of ['repeat 3 times', 'repeat count times', 'for each file in files', 'while status is 0']) assert.ok(indent.test(line), line);
  assert.ok(!indent.test('run while'));
});

test('structured JSON operations highlight types, pointers, sources and bindings', () => {
  scope('then parse json as document', 'parse', 'keyword.control.structured');
  scope('then parse json as document', 'json', 'support.type.input');
  scope('then parse json as document', 'document', 'variable.other.definition');
  scope('take integer at "/items/0/count" from document as count', 'take', 'keyword.control.structured');
  scope('take integer at "/items/0/count" from document as count', 'integer', 'support.type.input');
  scope('take integer at "/items/0/count" from document as count', '/items/0/count', 'string.quoted.double');
  scope('take integer at "/items/0/count" from document as count', 'document', 'variable.other.readwrite');
  scope('take integer at "/items/0/count" from document as count', 'count', 'variable.other.definition', 1);
  scope('then encode document as json', 'encode', 'keyword.control.structured');
  scope('then encode document as json', 'document', 'variable.other.readwrite');
  scope('then encode document as json', 'json', 'support.type.input');
});

test('reusable program calls distinguish the program and its explicit parameters', () => {
  for (const prefix of ['run', 'then']) {
    const line = `${prefix} program render with name user as result`;
    scope(line, 'program', 'keyword.control.program');
    scope(line, 'render', 'entity.name.function.program');
    scope(line, 'name', 'variable.parameter');
    scope(line, 'user', 'variable.other.readwrite');
    scope(line, 'result', 'variable.other.definition');
  }
  scope('  accept active as boolean with default true', 'boolean', 'support.type.input');
  scope('  accept data as json', 'data', 'variable.parameter.input');
});

test('process input sources distinguish environment and positional argv', () => {
  for (const source of ['environment "OMARCHY_PATH"', 'argument 1', 'arguments']) {
    const line = `accept setting as text from ${source}`;
    scope(line, 'from', 'keyword.operator.source');
    scope(line, source.split(' ')[0], 'support.type.input-source');
  }
});

test('try clauses, lexical transfers, and bounded task groups match current source', () => {
  scope('try', 'try', 'keyword.control.exception');
  scope('catch failure', 'catch', 'keyword.control.exception');
  scope('catch failure', 'failure', 'variable.other.definition');
  scope('finally', 'finally', 'keyword.control.exception');
  for (const transfer of ['break', 'continue', 'return']) {
    scope(transfer, transfer, 'keyword.control.transfer');
  }
  scope('exit with status 2', 'exit', 'keyword.control.transfer');
  scope('fail with message "x"', 'fail', 'keyword.control.transfer');
  const header = 'parallel at most 3 as results';
  scope(header, 'parallel', 'keyword.control.task-group');
  scope(header, 'at most', 'keyword.other.task-group');
  scope(header, '3', 'constant.numeric.integer');
  scope(header, 'as', 'keyword.operator.binding');
  scope(header, 'results', 'variable.other.definition');
  scope('race at most 2 as winner', 'race', 'keyword.control.task-group');
  scope('  run number.add with left 1 and right 2 as sum', 'number.add', 'entity.name.function.applet');
  const indent = new RegExp(configuration.onEnterRules[0].beforeText);
  for (const line of ['try', 'catch failure', 'finally', 'parallel at most 2 as results', 'race at most 2 as winner']) {
    assert.ok(indent.test(line), line);
  }
});

test('named finite jobs distinguish controls, job references and result bindings', () => {
  const spawn = 'spawn as build # a named finite job';
  scope(spawn, 'spawn', 'keyword.control.job');
  scope(spawn, 'as', 'keyword.operator.binding');
  scope(spawn, 'build', 'variable.other.definition.job');
  scope(spawn, '# a named', 'comment.line.number-sign');

  const stage = '  run "printf" with arguments ["%s\\n", "await cancel"]';
  scope(stage, 'printf', 'entity.name.function.applet');
  scope(stage, '%s\\n', 'string.quoted.double');
  scope(stage, 'await cancel', 'string.quoted.double');

  const inspect = 'inspect build as build_state';
  scope(inspect, 'inspect', 'keyword.control.job');
  scope(inspect, 'build', 'variable.other.job');
  scope(inspect, 'as', 'keyword.operator.binding');
  scope(inspect, 'build_state', 'variable.other.definition');

  const awaitJob = 'await build and keep status as code and keep result as record';
  scope(awaitJob, 'await', 'keyword.control.job');
  scope(awaitJob, 'build', 'variable.other.job');
  scope(awaitJob, 'keep status', 'storage.modifier.status');
  scope(awaitJob, 'code', 'variable.other.definition');
  scope(awaitJob, 'keep result', 'storage.modifier.result');
  scope(awaitJob, 'record', 'variable.other.definition');
  scope('await build and keep result as record', 'keep result', 'storage.modifier.result');

  scope('cancel build', 'cancel', 'keyword.control.job');
  scope('cancel build', 'build', 'variable.other.job');
  const commented = 'await build # await ignored';
  scope(commented, 'await', 'keyword.control.job');
  scope(commented, 'await', 'comment.line.number-sign', 1);
  scope('run "await" with message "cancel job"', 'await', 'string.quoted.double');
  scope('run "await" with message "cancel job"', 'cancel job', 'string.quoted.double');
});

test('JSON values highlight null, signed decimals, nested objects and dynamic pointers', () => {
  const line = 'then encode {"name": "Ada", "profile": {"nickname": "Ada"}, "score": -1.25, "ratio": 6e+4, "items": [true, null, 07]} as json';
  scope(line, '{', 'punctuation.section.mapping.begin');
  scope(line, '{', 'punctuation.section.mapping.begin', 1);
  scope(line, ':', 'punctuation.separator.key-value');
  scope(line, '-1.25', 'constant.numeric.float');
  scope(line, '6e+4', 'constant.numeric.float');
  scope(line, 'null', 'constant.language.null');
  scope(line, '07', 'constant.numeric.integer');
  scope(line, '}', 'punctuation.section.mapping.end');
  scope(line, '}', 'punctuation.section.mapping.end', 1);
  assert.ok(!scopesAt('run sample with value +7', '7').some(name => name.startsWith('constant.numeric.')));
  const pointer = 'take text at pointer from document as selected';
  scope(pointer, 'take', 'keyword.control.structured');
  scope(pointer, 'at', 'keyword.operator.structured');
  scope(pointer, 'pointer', 'variable.other.readwrite');
  scope(pointer, 'document', 'variable.other.readwrite');
  scope(pointer, 'selected', 'variable.other.definition');
});

test('graph declarations, typed node bindings and queue policies have dedicated scopes', () => {
  scope('graph greeting', 'graph', 'keyword.declaration.graph');
  scope('graph greeting', 'greeting', 'entity.name.type.graph');
  scope('  every 10 seconds as ticker', 'every', 'keyword.control.timer');
  scope('  every 10 seconds as ticker', 'seconds', 'support.type.duration');
  scope('  every 10 seconds as ticker', 'ticker', 'variable.other.graph-node');
  scope('  listen to stdin as lines', 'listen to', 'keyword.control.listen');
  scope('  listen to stdin as lines', 'stdin', 'constant.language.stdin');
  scope('  listen to stdin as lines', 'lines', 'variable.other.graph-node');
  scope('  render tui:text with properties {"text": "Ready"} as output', 'render', 'keyword.control.graph');
  scope('  render tui:text with properties {"text": "Ready"} as output', 'tui', 'support.type.renderer');
  scope('  render tui:text with properties {"text": "Ready"} as output', 'text', 'support.type.renderer-component');
  const edge = '  bind joined.value to cleaned.text with capacity 1 drop oldest';
  scope(edge, 'bind', 'keyword.control.graph');
  scope(edge, 'joined', 'variable.other.graph-node');
  scope(edge, 'value', 'variable.other.graph-port');
  scope(edge, 'cleaned', 'variable.other.graph-node');
  scope(edge, 'text', 'variable.other.graph-port');
  scope(edge, 'drop oldest', 'constant.language.graph-policy');
  scope('end graph', 'end', 'keyword.control.graph');
  scope('end graph', 'graph', 'keyword.control.graph');
  const indent = new RegExp(configuration.onEnterRules[0].beforeText);
  assert.ok(indent.test('graph greeting'));
});
