import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import examples from './examples.json';
import fixtures from './fixtures.json';
import { decodePreview } from './preview.mjs';

const $ = (id) => document.getElementById(id);
const terminal = new Terminal({
  convertEol: true,
  cursorBlink: true,
  fontFamily: 'ui-monospace, SFMono-Regular, Consolas, monospace',
  fontSize: 12,
  scrollback: 1200,
  theme: {
    background: '#14221e',
    foreground: '#dcebdc',
    cursor: '#d5ff5f',
    selectionBackground: '#47613c',
    green: '#d5ff5f',
    red: '#ff9a8c',
    blue: '#a6baff',
  },
  allowProposedApi: false,
});
const fit = new FitAddon();
terminal.loadAddon(fit);
terminal.open($('terminal'));
new ResizeObserver(() => {
  try {
    fit.fit();
  } catch {
    /* hidden or not sized yet */
  }
}).observe($('terminal'));
const clean = (value) => String(value).replace(/[\x00-\x08\x0b-\x1f\x7f-\x9f]/g, '');
const print = (value) => terminal.write(clean(String(value).slice(0, 131072)));
let worker,
  nextId = 0,
  busy = false,
  ready = false,
  active = examples[0],
  snapshot = {},
  lastResponse,
  line = '',
  cursor = 0,
  history = [],
  historyAt = 0;
const pending = new Map();
const seed = {
  ...fixtures,
  ...Object.fromEntries(examples.map((item) => [item.file, item.source])),
};
const timing = (value) => (value > 0 ? `${value.toFixed(2)} ms` : '<0.1 ms (timer resolution)');
const status = (text) => {
  $('run-status').textContent = text;
};
function controls() {
  for (const id of ['run', 'check', 'explain', 'reset', 'new-file', 'upload'])
    $(id).disabled = busy || !ready;
  $('stop').disabled = !busy;
}
function prompt() {
  terminal.write(`\r\n\x1b[38;2;213;255;95m${clean(lastResponse?.cwd ?? '/workspace')}\x1b[0m $ `);
  line = '';
  cursor = 0;
}
function spawn() {
  ready = false;
  controls();
  worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
  return new Promise((resolve, reject) => {
    const boot = setTimeout(
      () => reject(new Error('Runtime load timed out. Check your connection and reload.')),
      20000,
    );
    worker.onmessage = ({ data }) => {
      if (data.ready) {
        clearTimeout(boot);
        $('runtime-status').textContent =
          `● Rust/WASM ready · ${Math.round(data.bytes / 1024)} KiB`;
        resolve();
      } else if (data.fatal) {
        clearTimeout(boot);
        reject(new Error(data.fatal));
      } else {
        const waiting = pending.get(data.id);
        if (!waiting) return;
        pending.delete(data.id);
        clearTimeout(waiting.timer);
        if (data.error) {
          waiting.reject(
            new Error(data.restart ? 'Stopped after a WASM trap; restoring files.' : data.error),
          );
          if (data.restart) void stop();
        } else waiting.resolve(data);
      }
    };
    worker.onerror = (event) => {
      clearTimeout(boot);
      reject(new Error(event.message || 'Runtime worker failed'));
      if (busy) void stop();
    };
  });
}
function request(value) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error('Execution exceeded 8 seconds; worker stopped.'));
      void stop();
    }, 8000);
    pending.set(id, { resolve, reject, timer });
    worker.postMessage({ id, request: value });
  }).then((data) => {
    if (data.result.files) {
      snapshot = data.result.files;
      lastResponse = data.result;
      renderFiles();
    }
    return data;
  });
}
async function write(path, content) {
  const { result } = await request({ action: 'write', path, content });
  if (!result.ok) throw new Error(result.error);
}
async function initialize(files = seed, directories = [], cwd = '/workspace') {
  await spawn();
  const quoted = (value) => "'" + value.replaceAll("'", "'\\''") + "'";
  for (const directory of directories)
    if (directory !== '/' && directory !== '/workspace')
      await request({ action: 'command', command: `mkdir -p ${quoted(directory)}` });
  for (const [path, content] of Object.entries(files)) {
    // Restore nested directories before restoring a stopped worker's snapshot.
    const separator = path.lastIndexOf('/');
    const parent = separator > 0 ? path.slice(0, separator) : '';
    if (parent && parent !== '/workspace')
      await request({
        action: 'command',
        command: `mkdir -p '${parent.replaceAll("'", "'\\''")}'`,
      });
    await write(path, content);
  }
  if (cwd !== '/workspace') await request({ action: 'command', command: `cd ${quoted(cwd)}` });
  ready = true;
  controls();
}
async function stop() {
  if (!busy) return;
  worker.terminate();
  for (const wait of pending.values()) {
    clearTimeout(wait.timer);
    wait.reject(new Error('Stopped. Restoring the last completed request.'));
  }
  pending.clear();
  busy = false;
  ready = false;
  controls();
  $('runtime-status').textContent = '◌ Restarting worker…';
  try {
    await initialize(snapshot, lastResponse?.directories ?? [], lastResponse?.cwd ?? '/workspace');
    status('Stopped · restored last completed request (cache cleared)');
    prompt();
  } catch (error) {
    failure(error);
  }
}
function failure(error) {
  print(`\r\nError: ${error.message}\r\n`);
  status(error.message);
}
function renderFiles() {
  $('files').replaceChildren();
  for (const [path, content] of Object.entries(snapshot)) {
    const button = document.createElement('button');
    button.className =
      'file' +
      (path.endsWith('.program') ? ' program' : '') +
      (!Object.hasOwn(seed, path.replace(/^\/workspace\//, '')) ? ' generated' : '');
    const name = document.createElement('span');
    name.textContent = path.replace(/^\/workspace\//, '');
    const size = document.createElement('small');
    size.textContent = `${new TextEncoder().encode(content).length} B`;
    button.append(name, size);
    button.onclick = () => {
      if (busy || !ready) return;
      if (path.endsWith('.program')) {
        const known = examples.find((item) => '/workspace/' + item.file === path);
        active = { ...known, file: path, title: name.textContent };
        $('artifact-panel').hidden = true;
        $('filename').textContent = name.textContent;
        $('source').value = content;
        document
          .querySelectorAll('.example')
          .forEach((item) => item.setAttribute('aria-pressed', 'false'));
        $('source').focus();
      } else openFile(path, content);
    };
    $('files').append(button);
  }
}
function renderArtifact(response) {
  const panel = $('artifact-panel');
  panel.hidden = true;
  $('artifact-metrics').replaceChildren();
  $('artifact-notes').replaceChildren();
  if (!active.previewFile) return;
  const path = '/workspace/' + active.previewFile;
  if (!response.written_files?.includes(path)) return;
  const raw = snapshot[path];
  if (raw === undefined) return;
  try {
    const model = decodePreview(raw);
    $('artifact-title').textContent = model.title;
    $('artifact-status').textContent = model.status;
    $('artifact-source').textContent =
      `Saved artifact: ${path}. Every value comes from the .program output.`;
    const maximum = Math.max(1, ...model.metrics.map((item) => item.value));
    for (const metric of model.metrics) {
      const card = document.createElement('div');
      card.className = 'artifact-metric';
      const label = document.createElement('span');
      label.textContent = metric.label;
      const number = document.createElement('strong');
      number.textContent = metric.value.toLocaleString();
      const track = document.createElement('div');
      track.className = 'artifact-track';
      track.setAttribute('aria-hidden', 'true');
      const fill = document.createElement('div');
      fill.className = 'artifact-fill';
      fill.style.width = `${(metric.value / maximum) * 100}%`;
      track.append(fill);
      card.append(label, number, track);
      $('artifact-metrics').append(card);
    }
    for (const note of model.notes) {
      const item = document.createElement('li');
      item.textContent = note;
      $('artifact-notes').append(item);
    }
  } catch (error) {
    $('artifact-title').textContent = 'Preview unavailable';
    $('artifact-status').textContent = 'Check the generated JSON';
    $('artifact-source').textContent = error.message;
  }
  panel.hidden = false;
}
function renderTrace(response, elapsed) {
  $('metrics').textContent =
    `${timing(elapsed)} · ${response.applet_calls} applet calls · ${response.cache_hits} cache hits`;
  $('trace').replaceChildren();
  for (const step of response.trace ?? []) {
    const row = document.createElement('div');
    row.className = 'trace-step';
    const n = document.createElement('span');
    n.textContent = step.line;
    const label = document.createElement('div');
    label.textContent = step.operation;
    const argv = document.createElement('small');
    argv.textContent = step.argv
      ? JSON.stringify(step.argv)
      : (step.program ?? (step.matched === undefined ? '' : `matched: ${step.matched}`));
    label.append(argv);
    const detail = document.createElement('span');
    detail.textContent = step.cache_hit
      ? 'cache hit'
      : step.output_bytes === undefined
        ? ''
        : `${step.input_bytes} → ${step.output_bytes} B`;
    if (step.cache_hit) detail.className = 'hit';
    row.append(n, label, detail);
    $('trace').append(row);
  }
  if (!response.trace?.length) {
    const p = document.createElement('p');
    p.className = 'empty';
    p.textContent = response.ok
      ? 'No applet calls in this request.'
      : 'Execution stopped before the next applet.';
    $('trace').append(p);
  }
}
async function perform(action, command) {
  if (busy || !ready) return;
  busy = true;
  controls();
  $('artifact-panel').hidden = true;
  let interrupted = false;
  try {
    // Save the open editor before both button runs and terminal execute commands.
    await write(active.file, $('source').value);
    let value;
    if (command !== undefined) value = { action: 'command', command };
    else {
      const inputs = JSON.parse($('inputs').value);
      if (
        !inputs ||
        Array.isArray(inputs) ||
        typeof inputs !== 'object' ||
        Object.values(inputs).some((value) => typeof value !== 'string')
      )
        throw new Error('Inputs must be a JSON object of strings, for example {"limit":"2"}.');
      value = { action, source: $('source').value, inputs, stdin: $('stdin').value };
      print(`\r\n$ execute ${action === 'run' ? '' : action + ' '}${active.file}\r\n`);
    }
    status('Running in WebAssembly…');
    const { result, elapsed } = await request(value);
    $('accessible-output').textContent = result.ok ? result.stdout : result.error;
    print(result.stdout ?? '');
    if (!result.ok) print(`\r\nError: ${result.error}\r\n`);
    renderTrace(result, elapsed);
    if (result.ok && action === 'run') renderArtifact(result);
    status(
      `${result.ok ? 'Completed' : 'Failed'} · ${timing(elapsed)} inside worker (not a Bash benchmark)`,
    );
  } catch (error) {
    interrupted = /Stopped|exceeded 8 seconds/.test(error.message);
    if (!interrupted) failure(error);
  } finally {
    if (!interrupted) {
      busy = false;
      controls();
      prompt();
    }
  }
}
function choose(example) {
  if (busy) return;
  active = example;
  $('artifact-panel').hidden = true;
  $('example-hint').textContent =
    example.hint ?? 'Edit the source or a virtual file, then run again to see what changes.';
  $('filename').textContent = example.file;
  $('source').value = snapshot[`/workspace/${example.file}`] ?? example.source;
  $('stdin').value = example.stdin ?? '';
  $('inputs').value = '{}';
  document
    .querySelectorAll('.example')
    .forEach((button) =>
      button.setAttribute('aria-pressed', String(button.dataset.id === example.id)),
    );
}
for (const example of examples) {
  const button = document.createElement('button');
  button.className = 'example';
  button.dataset.id = example.id;
  button.setAttribute('aria-pressed', 'false');
  const title = document.createElement('b');
  title.textContent = example.title;
  const description = document.createElement('span');
  description.textContent = example.subtitle;
  button.append(title, description);
  button.onclick = () => choose(example);
  $('examples').append(button);
}
$('run').onclick = () => perform('run');
$('check').onclick = () => perform('check');
$('explain').onclick = () => perform('explain');
$('stop').onclick = stop;
$('clear').onclick = () => {
  terminal.clear();
};
$('reset').onclick = async () => {
  if (busy || !ready) return;
  busy = true;
  controls();
  try {
    await request({ action: 'reset' });
    for (const [path, content] of Object.entries(seed)) await write(path, content);
    busy = false;
    choose(examples[0]);
    $('artifact-panel').hidden = true;
    status('Examples and files reset · cache cleared');
  } catch (error) {
    failure(error);
  } finally {
    busy = false;
    controls();
  }
};
$('source').addEventListener('keydown', (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
    event.preventDefault();
    void perform('run');
  }
  if (event.key === 'Tab') {
    event.preventDefault();
    const el = event.target,
      start = el.selectionStart;
    el.setRangeText('  ', start, el.selectionEnd, 'end');
  }
});
$('download').onclick = () => {
  const url = URL.createObjectURL(
    new Blob([$('source').value], { type: 'text/plain;charset=utf-8' }),
  );
  const link = document.createElement('a');
  link.href = url;
  link.download = active.file.split('/').pop();
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
};
$('share').onclick = async () => {
  try {
    const bytes = new TextEncoder().encode($('source').value);
    if (bytes.length > 65536) throw new Error('Shared source exceeds 64 KiB');
    const code = btoa(String.fromCharCode(...bytes));
    const url = new URL(location.href);
    url.hash = new URLSearchParams({ code }).toString();
    await navigator.clipboard.writeText(url.href);
    status('Link copied · includes script source, not virtual files or inputs');
  } catch (error) {
    status(`Sharing failed: ${error.message}`);
  }
};
function openFile(path = '', content = '') {
  $('file-path').value = path;
  $('file-content').value = content;
  $('file-error').textContent = '';
  $('file-dialog').showModal();
}
$('new-file').onclick = () => openFile();
$('save-file').onclick = async () => {
  if (busy || !ready) return;
  try {
    await write($('file-path').value, $('file-content').value);
    $('file-dialog').close();
    status('Virtual file saved');
  } catch (error) {
    $('file-error').textContent = error.message;
  }
};
$('upload').onclick = () => $('file-upload').click();
$('file-upload').onchange = async (event) => {
  const file = event.target.files[0];
  if (!file) return;
  try {
    if (file.size > 1048576) throw new Error('Import limit is 1 MiB');
    await write(file.name, await file.text());
    status(`Imported ${file.name} into virtual memory`);
  } catch (error) {
    failure(error);
  }
  event.target.value = '';
};
function repaint() {
  terminal.write('\x1b[2K\r');
  terminal.write(
    `\x1b[38;2;213;255;95m${clean(lastResponse?.cwd ?? '/workspace')}\x1b[0m $ ${clean(line)}`,
  );
  if (cursor < line.length) terminal.write(`\x1b[${line.length - cursor}D`);
}
terminal.onData((data) => {
  if (data === '\x03') {
    if (busy) {
      void stop();
    } else {
      terminal.write('^C');
      prompt();
    }
    return;
  }
  if (busy || !ready) return;
  if (data === '\r') {
    const command = line.trim();
    terminal.write('\r\n');
    history.push(line);
    if (history.length > 100) history.shift();
    historyAt = history.length;
    if (!command) {
      prompt();
      return;
    }
    if (command === 'clear') {
      terminal.clear();
      prompt();
      return;
    }
    if (command === 'reset') {
      void $('reset').onclick();
      prompt();
      return;
    }
    void perform('command', command);
    return;
  }
  if (data === '\x7f') {
    if (cursor) {
      line = line.slice(0, cursor - 1) + line.slice(cursor);
      cursor--;
      repaint();
    }
    return;
  }
  if (data === '\x1b[D') {
    cursor = Math.max(0, cursor - 1);
    repaint();
    return;
  }
  if (data === '\x1b[C') {
    cursor = Math.min(line.length, cursor + 1);
    repaint();
    return;
  }
  if (data === '\x1b[A' || data === '\x1b[B') {
    historyAt = Math.max(0, Math.min(history.length, historyAt + (data === '\x1b[A' ? -1 : 1)));
    line = history[historyAt] ?? '';
    cursor = line.length;
    repaint();
    return;
  }
  if (data === '\x15') {
    line = '';
    cursor = 0;
    repaint();
    return;
  }
  if (data.startsWith('\x1b')) return;
  const insert = clean(data).replace(/[\r\n]/g, ' ');
  if (line.length + insert.length > 65536) return;
  line = line.slice(0, cursor) + insert + line.slice(cursor);
  cursor += insert.length;
  repaint();
});
function applyHash() {
  try {
    const params = new URLSearchParams(location.hash.slice(1));
    if (params.has('code')) {
      const code = params.get('code');
      if (code.length > 90000) throw new Error('Shared source too large');
      const source = new TextDecoder('utf-8', { fatal: true }).decode(
        Uint8Array.from(atob(code), (ch) => ch.charCodeAt(0)),
      );
      if (new TextEncoder().encode(source).length > 65536)
        throw new Error('Shared source exceeds 64 KiB');
      active = { file: 'shared.program' };
      $('filename').textContent = 'shared.program';
      $('source').value = source;
      document
        .querySelectorAll('.example')
        .forEach((button) => button.setAttribute('aria-pressed', 'false'));
    } else if (params.has('example')) {
      const example = examples.find((item) => item.id === params.get('example'));
      if (example) choose(example);
    }
  } catch (error) {
    failure(error);
  }
}
window.addEventListener('hashchange', () => {
  if (!busy && ready) applyHash();
});
choose(examples[0]);
try {
  await initialize();
  applyHash();
  print(
    'Execute / browser workbench\r\nReal Rust language core. Virtual applets. No host access.\r\n\r\nTry Run program, or type help.',
  );
  prompt();
} catch (error) {
  ready = false;
  controls();
  $('runtime-status').textContent = 'Runtime could not start';
  failure(error);
}
