import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { connect } from '../abi.mjs';
const root = fileURLToPath(new URL('../../', import.meta.url));
const module = await WebAssembly.compile(
  await readFile(process.env.EXECUTE_TEST_WASM ?? path.join(root, 'docs/playground/execute.wasm')),
);
const examples = JSON.parse(await readFile(new URL('../examples.json', import.meta.url)));
const fixtures = JSON.parse(await readFile(new URL('../fixtures.json', import.meta.url)));
const byId = Object.fromEntries(examples.map((item) => [item.id, item]));

async function seeded() {
  const call = connect(await WebAssembly.instantiate(module, {}));
  for (const [file, content] of Object.entries(fixtures))
    assert.equal(call({ action: 'write', path: file, content }).ok, true);
  return call;
}
const edit = (call, file, change) => {
  const next = change(fixtures[file]);
  assert.notEqual(next, fixtures[file], `edit of ${file} changed nothing`);
  assert.equal(call({ action: 'write', path: file, content: next }).ok, true);
};
function run(call, id, inputs = {}) {
  const result = call({ action: 'run', source: byId[id].source, inputs });
  assert.equal(result.ok, true, `${id}: ${result.error}`);
  return result;
}

test('the playground ships at least 25 demos, each described, grouped and unique', () => {
  assert.ok(examples.length >= 25, `only ${examples.length} demos`);
  assert.equal(new Set(examples.map((item) => item.id)).size, examples.length);
  assert.equal(new Set(examples.map((item) => item.file)).size, examples.length);
  assert.equal(new Set(examples.map((item) => item.title)).size, examples.length);
  for (const item of examples) {
    for (const key of ['group', 'title', 'subtitle', 'file', 'source', 'expected', 'hint'])
      assert.equal(typeof item[key], 'string', `${item.id}.${key}`);
    assert.ok(item.title.length <= 40, `${item.id}: title is too long for its button`);
    assert.ok(item.subtitle.length <= 70, `${item.id}: subtitle is too long for its button`);
    assert.ok(item.expected.length > 0, `${item.id}: expected output is empty`);
  }
});

test('every demo passes check and explain, and all of them fit in one session within the file limits', async () => {
  const call = await seeded();
  for (const item of examples) {
    for (const action of ['check', 'explain']) {
      const result = call({ action, source: item.source });
      assert.equal(result.ok, true, `${item.id} ${action}: ${result.error}`);
    }
    const result = call({ action: 'run', source: item.source, stdin: item.stdin ?? '' });
    assert.equal(result.ok, true, `${item.id}: ${result.error}`);
    assert.equal(result.stdout, item.expected, item.id);
    for (const [file, content] of Object.entries(item.expectedFiles ?? {}))
      assert.equal(result.files['/workspace/' + file], content, `${item.id}: ${file}`);
  }
  const state = call({ action: 'inspect' });
  // The page also seeds one program file per demo; the virtual filesystem allows 128 files.
  const total = Object.keys(state.files).length + examples.length;
  assert.ok(total <= 120, `${total} files after running every demo`);
});

test('every fixture is used by a demo and every demo file name is a distinct .program', () => {
  for (const item of examples) assert.match(item.file, /^[a-z0-9-]+\.program$/);
  const programs = examples.map((item) => item.source).join('\n');
  const orphans = Object.keys(fixtures).filter(
    (file) => !programs.includes(`"${file}"`) && !/^(README\.txt|app\.log)$/.test(file),
  );
  assert.deepEqual(orphans, []);
});

test('web traffic follows its inputs and the log', async () => {
  const call = await seeded();
  const log = fixtures['nginx-access.log'].trimEnd().split('\n');
  assert.equal(
    run(call, 'web-traffic', { top: '3' })
      .stdout.split('Status codes seen')[0]
      .split('\n')
      .filter((line) => /^ {6}\d+ /.test(line)).length,
    3,
  );
  edit(call, 'nginx-access.log', (text) => text + 'GET /api/orders 502\n');
  const count = log.filter((line) => line.split(' ').at(-1).startsWith('50')).length + 1;
  assert.match(run(call, 'web-traffic').stdout, new RegExp(`Server errors \\(5xx\\): ${count}\\n`));
});

test('ssh triage raises and clears the breach alert from the log alone', async () => {
  const call = await seeded();
  assert.match(
    run(call, 'ssh-failed-logins').stdout,
    /ALERT: 203\.0\.113\.\* guessed passwords 14 times/,
  );
  edit(call, 'auth.log', (text) =>
    text
      .split('\n')
      .filter((line) => !line.includes('Accepted password for deploy'))
      .join('\n'),
  );
  assert.doesNotMatch(run(call, 'ssh-failed-logins').stdout, /ALERT/);
});

test('hourly errors, listeners and sudo drift react to edits', async () => {
  const call = await seeded();
  edit(
    call,
    'app-errors.log',
    (text) => text + '2026-10-16T23:59:59Z ERROR auth token signature mismatch\n',
  );
  const hourly = run(call, 'errors-by-hour').stdout;
  assert.match(hourly, /\n23:00 {2}1\n/);
  assert.match(hourly, /Errors: 29 /);
  edit(call, 'allowed-ports.json', (text) => text.replace('":443 "', '":443 ", ":8080 "'));
  const ports = run(call, 'listening-ports').stdout;
  assert.match(ports, /2 unexpected/);
  assert.doesNotMatch(ports, /java/);
  assert.match(run(call, 'user-group-audit').stdout, /DRIFT in the sudo group/);
  assert.match(
    run(call, 'user-group-audit', { admins: 'ada,deploy,carol' }).stdout,
    /sudo group matches/,
  );
});

test('capacity demos: disk dashboard, process focus, updates, service status and certificates', async () => {
  const call = await seeded();
  edit(call, 'df.txt', (text) =>
    text.replace(' 97%  200G  193G  7.0G /var\n', ' 50%  200G  100G  100G /var\n'),
  );
  const disk = run(call, 'disk-usage');
  assert.equal(JSON.parse(disk.files['/workspace/disk-dashboard.json']).metrics[1].value, 2);
  assert.match(
    run(call, 'top-memory', { focus: 'deploy' }).stdout,
    /Largest three for deploy\n *2184320 .*java/,
  );
  edit(call, 'apt-upgradable.txt', (text) =>
    text
      .split('\n')
      .filter((line) => !line.includes('-security'))
      .join('\n'),
  );
  assert.match(run(call, 'package-updates').stdout, /No security updates are pending\./);
  edit(call, 'health.json', (text) => text.replace('"state": "down"', '"state": "up"'));
  assert.equal(
    JSON.parse(run(call, 'service-health').files['/workspace/health-dashboard.json']).status,
    'DEGRADED',
  );
  const later = run(call, 'cert-expiry', { today: '2026-12-01', warn_by: '2027-01-01' });
  const expected = JSON.parse(fixtures['certs.json']).certs.map((cert) =>
    cert.not_after < '2026-12-01' ? 'expired' : cert.not_after < '2027-01-01' ? 'soon' : 'ok',
  );
  const dashboard = JSON.parse(later.files['/workspace/cert-dashboard.json']);
  assert.deepEqual(
    dashboard.metrics.map((item) => item.value),
    [
      expected.length,
      expected.filter((item) => item === 'expired').length,
      expected.filter((item) => item === 'soon').length,
    ],
  );
});

test('config demos: cron findings, env rows, sshd verdict, CSV report and CI status', async () => {
  const call = await seeded();
  edit(call, 'crontab.txt', (text) =>
    text
      .split('\n')
      .filter((line) => !line.includes('curl'))
      .join('\n'),
  );
  assert.match(
    run(call, 'cron-timer-audit').stdout,
    /HIGH {2}downloads a script and pipes it to a shell\n\nMEDIUM/,
  );
  edit(call, 'app.env', (text) => text.replace('SMTP_HOST=\n', 'SMTP_HOST=smtp.mail.internal\n'));
  const env = run(call, 'env-validation').stdout;
  assert.match(env, /ok {11}SMTP_HOST\n/);
  assert.match(env, /INVALID: 3 keys need attention/);
  assert.doesNotMatch(env, /s3cret|smtp\.mail/);
  edit(call, 'sshd_config.proposed', (text) => text + 'ClientAliveCountMax 2\n');
  assert.match(run(call, 'sshd-config-review').stdout, /^APPROVED/);
  edit(call, 'inventory.csv', (text) => text.replace('web-07,,web', 'web-07,platform,web'));
  const report = JSON.parse(
    run(call, 'inventory-report').files['/workspace/inventory-report.json'],
  );
  assert.equal(report.incomplete_rows, 1);
  edit(call, 'ci-runs.json', (text) =>
    text.replace(
      /"conclusion": "(failure|timed_out)",\n(\s+)"minutes": (2|60)/g,
      '"conclusion": "success",\n$2"minutes": $3',
    ),
  );
  const ci = JSON.parse(run(call, 'ci-triage').files['/workspace/ci-dashboard.json']);
  assert.equal(ci.status, 'main is green');
});

test('file demos: rotation, rename plan, duplicates, git notes and open items', async () => {
  const call = await seeded();
  const rotation = run(call, 'backup-rotation', { keep: '3' }).stdout;
  assert.match(rotation, /keep the newest 3, keep monthly firsts, 13 to delete/);
  assert.equal((rotation.match(/^KEEP-RECENT/gm) ?? []).length, 3);
  edit(call, 'photos-existing.txt', (text) => text.replace('2026-10-02_IMG_0412.JPG\n', ''));
  const plan = run(call, 'rename-plan');
  assert.match(plan.stdout, /^8 renames planned, 1 skipped/);
  assert.match(
    plan.files['/workspace/rename-plan.sh'],
    /mv -- 'IMG_0412\.JPG' '2026-10-02_IMG_0412\.JPG'/,
  );
  const listing = JSON.parse(fixtures['cksums.json']);
  listing.files = listing.files.filter((file) => !file.path.includes('Desktop/team-photo'));
  assert.equal(
    call({ action: 'write', path: 'cksums.json', content: JSON.stringify(listing) }).ok,
    true,
  );
  assert.match(run(call, 'duplicate-files').stdout, /^5 duplicate files in 2 groups/);
  edit(call, 'git-log.txt', (text) => text + 'abc1234 fix: handle empty input\n');
  const git = run(call, 'git-summary');
  assert.match(git.stdout, /\nfix {2}12\n/);
  assert.match(git.files['/workspace/release-notes.txt'], /abc1234 fix: handle empty input/);
  edit(call, 'meeting-notes.txt', (text) =>
    text.replace('- [ ] (@ada) review', '- [x] (@ada) review'),
  );
  assert.match(run(call, 'todo-notes').stdout, /^6 open, 5 done\./);
});
