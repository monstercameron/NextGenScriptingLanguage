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
  const line = 'run sample with flags ["-n", 0, -42, +7, true, false, name]';
  scope(line, '[', 'punctuation.section.brackets.begin');
  scope(line, ',', 'punctuation.separator.sequence');
  scope(line, ']', 'punctuation.section.brackets.end');
  for (const value of ['0', '-42', '+7']) scope(line, value, 'constant.numeric.integer');
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
test('unsupported control blocks are not advertised as implemented syntax', () => {
  for (const keyword of ['else', 'for', 'while', 'loop']) {
    scope(`${keyword} something`, keyword, 'invalid.illegal.unsupported-control');
    scope(`run ${keyword}`, keyword, 'entity.name.function.applet');
  }
});
test('editor configuration uses two-space blocks and only real syntactic brackets', () => {
  assert.equal(configuration.comments.lineComment, '#');
  assert.deepEqual(configuration.brackets, [['[', ']']]);
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
  return body.join('\n').replace(/\$\{\d+:([^}]*)\}/g, '$1').replace(/\$\d+/g, '') + '\n';
}
test('complete snippet defaults pass the actual execute check without running commands', () => {
  const snippets = JSON.parse(fs.readFileSync(path.join(extension, 'snippets/execute.code-snippets'), 'utf8'));
  assert.equal(Object.keys(snippets).length, 26);
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'execute-vscode-'));
  const binary = process.env.EXECUTE_TEST_BINARY || path.join(project, 'target/release', process.platform === 'win32' ? 'execute.exe' : 'execute');
  assert.ok(fs.existsSync(binary), 'Build the current runtime with cargo build --locked --release, or set EXECUTE_TEST_BINARY');
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
    assert.equal(checked, 18);
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
