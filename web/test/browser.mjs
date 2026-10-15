import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { measureWorker } from '../benchmark.mjs';
const root = fileURLToPath(new URL('../../', import.meta.url));
const evidence = path.join(root, 'target/playground');
await mkdir(evidence, { recursive: true });
const mime = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.css': 'text/css',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
};
let server;
let base = process.env.PLAYGROUND_BASE_URL;
if (!base) {
  server = createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://localhost');
      let relative = decodeURIComponent(url.pathname).replace(/^\/NextGenScriptingLanguage\//, '');
      if (relative.endsWith('/')) relative += 'index.html';
      const file = path.resolve(root, 'docs', relative);
      if (!file.startsWith(path.join(root, 'docs') + path.sep)) {
        res.writeHead(403).end();
        return;
      }
      const bytes = await readFile(file);
      res.writeHead(200, {
        'Content-Type': mime[path.extname(file)] ?? 'application/octet-stream',
      });
      res.end(bytes);
    } catch {
      res.writeHead(404).end();
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  base = `http://127.0.0.1:${server.address().port}/NextGenScriptingLanguage/playground/`;
}
const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({ viewport: { width: 1440, height: 1120 } });
const page = await context.newPage();
const errors = [];
const requests = [];
const checks = [];
page.on('pageerror', (error) => errors.push(error.message));
page.on('console', (message) => {
  if (message.type() === 'error') errors.push(message.text());
});
page.on('request', (request) => requests.push(request.url()));
async function check(name, body) {
  await body();
  checks.push(name);
  console.log(`PASS ${name}`);
}
async function finished() {
  await page.waitForFunction(
    () =>
      /^(Completed|Failed)/.test(document.getElementById('run-status').textContent) &&
      !document.getElementById('run').disabled,
  );
}
try {
  await page.goto(base);
  await page.waitForFunction(() =>
    document.getElementById('runtime-status').textContent.includes('ready'),
  );
  await page.waitForFunction(() => !document.getElementById('run').disabled);
  await check('actual module boots under the GitHub Pages repository subpath', async () => {
    assert.match(await page.locator('#runtime-status').textContent(), /Rust\/WASM ready/);
    assert.ok(requests.some((url) => url.endsWith('/execute.wasm')));
  });
  const examples = JSON.parse(await readFile(new URL('../examples.json', import.meta.url)));
  await check(
    'initial file listing contains only seeded files, without phantom directories',
    async () => {
      const fixtures = JSON.parse(await readFile(new URL('../fixtures.json', import.meta.url)));
      const expected = [...Object.keys(fixtures), ...examples.map((item) => item.file)].sort();
      await page.locator('.xterm-helper-textarea').focus();
      await page.keyboard.type('ls -1');
      await page.keyboard.press('Enter');
      await finished();
      assert.equal(
        await page.locator('#accessible-output').textContent(),
        expected.join('\n') + '\n',
      );
    },
  );
  for (const example of examples)
    await check(`interactive example ${example.id}`, async () => {
      await page.locator(`[data-id="${example.id}"]`).click();
      await page.locator('#run').click();
      await finished();
      assert.match(await page.locator('#run-status').textContent(), /^Completed/);
      assert.equal(await page.locator('#accessible-output').textContent(), example.expected);
      if (example.previewFile) {
        assert.equal(await page.locator('#artifact-panel').isVisible(), true);
        const expected = JSON.parse(example.expectedFiles[example.previewFile]);
        assert.equal(await page.locator('#artifact-title').textContent(), expected.title);
        assert.equal(await page.locator('#artifact-status').textContent(), expected.status);
        assert.deepEqual(
          await page.locator('.artifact-metric strong').allTextContents(),
          expected.metrics.map((item) => item.value.toLocaleString()),
        );
      }
      for (const [file, content] of Object.entries(example.expectedFiles ?? {})) {
        const literal = file.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
        await page
          .locator('.file span')
          .filter({ hasText: new RegExp('^' + literal + '$') })
          .click();
        assert.equal(
          await page.locator('#file-content').inputValue(),
          content,
          `${example.id}: ${file}`,
        );
        await page.getByRole('button', { name: 'Close file editor', exact: true }).click();
      }
    });
  await check('dashboard only presents artifacts written by the current execution', async () => {
    await page.locator('[data-id="incident-command"]').click();
    await page.locator('#run').click();
    await finished();
    assert.equal(await page.locator('#artifact-panel').isVisible(), true);
    await page.locator('#run').click();
    await finished();
    assert.equal(await page.locator('#artifact-panel').isVisible(), true);
    await page.locator('#source').fill('run "true"');
    await page.locator('#run').click();
    await finished();
    assert.equal(await page.locator('#artifact-panel').isVisible(), false);
  });
  await check('warm cache hit appears in trace', async () => {
    await page.locator('[data-id="cache"]').click();
    await page.locator('#run').click();
    await finished();
    await page.locator('#run').click();
    await finished();
    assert.match(await page.locator('#metrics').textContent(), /1 cache hits/);
    assert.equal(await page.locator('.hit').count(), 1);
  });
  await check('typed inputs change the result', async () => {
    await page.locator('[data-id="shortlist"]').click();
    await page.locator('summary').click();
    await page.locator('#inputs').fill('{"limit":"1"}');
    await page.locator('#run').click();
    await finished();
    assert.equal(await page.locator('#accessible-output').textContent(), 'Ada\n');
  });
  await check('generated files can be opened, edited and reused', async () => {
    await page.locator('.file').filter({ hasText: 'release.json' }).click();
    await page.locator('#file-content').fill('{"checks_passed":false}');
    await page.locator('#save-file').click();
    await page.locator('[data-id="release-gate"]').click();
    await page.locator('#run').click();
    await finished();
    assert.equal(
      await page.locator('#accessible-output').textContent(),
      'Hold: checks need attention\n',
    );
  });
  await check('check and explain run the Rust validator', async () => {
    await page.locator('#check').click();
    await finished();
    assert.match(await page.locator('#accessible-output').textContent(), /checks passed/);
    await page.locator('#explain').click();
    await finished();
    assert.match(
      await page.locator('#accessible-output').textContent(),
      /parse bounded finite output/,
    );
  });
  await check('terminal launches programs, quoted inputs and command history', async () => {
    const input = page.locator('.xterm-helper-textarea');
    await input.focus();
    await page.keyboard.type('execute shortlist.program --input limit=2');
    await page.keyboard.press('Enter');
    await finished();
    assert.equal(await page.locator('#accessible-output').textContent(), 'Ada\nGrace\n');
    await input.focus();
    await page.keyboard.press('ArrowUp');
    await page.keyboard.press('Enter');
    await finished();
    assert.equal(await page.locator('#accessible-output').textContent(), 'Ada\nGrace\n');
  });
  await check('unavailable native commands fail clearly and worker stays usable', async () => {
    await page.locator('#source').fill('run "git" with arguments ["status"]');
    await page.locator('#run').click();
    await finished();
    assert.match(
      await page.locator('#accessible-output').textContent(),
      /unavailable in the browser/,
    );
    await page.locator('[data-id="log-triage"]').click();
    await page.locator('#source').fill(examples[0].source);
    await page.locator('#run').click();
    await finished();
    assert.match(await page.locator('#run-status').textContent(), /^Completed/);
  });
  await check(
    'imports are local virtual files and downloads contain the actual script',
    async () => {
      await page.locator('#file-upload').setInputFiles({
        name: 'imported.txt',
        mimeType: 'text/plain',
        buffer: Buffer.from('local import\n'),
      });
      await page.waitForFunction(() =>
        document.getElementById('run-status').textContent.includes('Imported'),
      );
      assert.equal(await page.locator('.file').filter({ hasText: 'imported.txt' }).count(), 1);
      const downloadPromise = page.waitForEvent('download');
      await page.locator('#download').click();
      const download = await downloadPromise;
      assert.match(download.suggestedFilename(), /\.program$/);
      assert.equal(
        await readFile(await download.path(), 'utf8'),
        await page.locator('#source').inputValue(),
      );
    },
  );
  await check('Stop kills worker and restores the last completed filesystem snapshot', async () => {
    // Hold only the run request in the test harness; exercise the real Stop/restart UI.
    await page.evaluate(() => {
      window.originalWorkerPost = Worker.prototype.postMessage;
      Worker.prototype.postMessage = function (message, ...rest) {
        if (message.request?.action === 'run') return;
        return window.originalWorkerPost.call(this, message, ...rest);
      };
    });
    await page.locator('#run').click();
    await page.waitForFunction(() => !document.getElementById('stop').disabled);
    await page.locator('#stop').click();
    await page.waitForFunction(() =>
      document.getElementById('run-status').textContent.startsWith('Stopped ·'),
    );
    await page.evaluate(() => {
      Worker.prototype.postMessage = window.originalWorkerPost;
    });
    assert.equal(await page.locator('.file').filter({ hasText: 'imported.txt' }).count(), 1);
    await page.locator('#run').click();
    await finished();
    assert.match(await page.locator('#run-status').textContent(), /^Completed/);
  });
  await check('no external runtime/CDN requests and no browser or CSP errors', async () => {
    const origin = new URL(base).origin;
    assert.deepEqual(
      requests.filter((url) => new URL(url).origin !== origin),
      [],
    );
    assert.deepEqual(errors, []);
  });
  await page.screenshot({ path: path.join(evidence, 'desktop.png'), fullPage: true });
  await check('mobile layout fits without horizontal overflow', async () => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForTimeout(200);
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1));
    await page.screenshot({ path: path.join(evidence, 'mobile.png'), fullPage: true });
  });
  await check('shared source URLs load without auto-executing', async () => {
    const source = 'run printf with message "Shared snow: 雪"';
    await page.goto(
      base + '?shared=1#' + new URLSearchParams({ code: Buffer.from(source).toString('base64') }),
    );
    await page.waitForFunction(() => !document.getElementById('run').disabled);
    assert.equal(await page.locator('#source').inputValue(), source);
    assert.match(await page.locator('#run-status').textContent(), /^Type help/);
    await page.locator('#run').click();
    await finished();
    assert.equal(await page.locator('#accessible-output').textContent(), 'Shared snow: 雪\n');
  });
  await check('actual worker latency samples verify every output', async () => {
    const fixtures = JSON.parse(await readFile(new URL('../fixtures.json', import.meta.url)));
    const measurements = await measureWorker(page, examples, fixtures);
    await writeFile(
      path.join(evidence, 'latency.json'),
      JSON.stringify(measurements, null, 2) + '\n',
    );
  });
  await writeFile(
    path.join(evidence, 'browser.json'),
    JSON.stringify(
      {
        status: 'passed',
        url: base,
        browser: browser.version(),
        checks,
        errors,
        network_requests: requests,
      },
      null,
      2,
    ) + '\n',
  );
  console.log(`${checks.length} browser checks passed.`);
} finally {
  await browser.close();
  if (server) await new Promise((resolve) => server.close(resolve));
}
