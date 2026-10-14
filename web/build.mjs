import { build } from 'esbuild';
import { readFile, writeFile, copyFile, readdir, mkdir } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const root = fileURLToPath(new URL('../', import.meta.url));
const out = path.join(root, 'docs/playground');
await mkdir(out, { recursive: true });
for (const entry of ['app', 'worker']) {
  await build({
    entryPoints: [path.join(root, `web/${entry}.mjs`)],
    outfile: path.join(out, `${entry}.js`),
    bundle: true,
    format: 'esm',
    minify: true,
    target: 'es2022',
    legalComments: 'linked',
  });
}
await copyFile(path.join(root, 'web/style.css'), path.join(out, 'style.css'));
await copyFile(
  path.join(root, 'web/node_modules/@xterm/xterm/css/xterm.css'),
  path.join(out, 'xterm.css'),
);
await copyFile(
  path.join(root, 'browser/target/wasm32-unknown-unknown/release/execute_browser.wasm'),
  path.join(out, 'execute.wasm'),
);
const inputs = [];
async function collect(folder) {
  for (const entry of (await readdir(path.join(root, folder), { withFileTypes: true })).sort(
    (a, b) => a.name.localeCompare(b.name),
  )) {
    const file = path.posix.join(folder, entry.name);
    if (entry.isDirectory()) await collect(file);
    else if (entry.name.endsWith('.rs') || entry.name.endsWith('.json')) inputs.push(file);
  }
}
await collect('src');
await collect('manifests');
for (const entry of (await readdir(path.join(root, 'web'))).sort())
  if (/\.(mjs|json|css)$/.test(entry)) inputs.push('web/' + entry);
inputs.push(
  'browser/.cargo/config.toml',
  'docs/playground/index.html',
  'tools/build_playground.py',
);
inputs.push(
  'Cargo.toml',
  'Cargo.lock',
  'browser/Cargo.toml',
  'browser/Cargo.lock',
  'browser/src/lib.rs',
  'web/package-lock.json',
);
const hashes = {};
for (const file of inputs)
  hashes[file] = createHash('sha256')
    .update(await readFile(path.join(root, file)))
    .digest('hex');
const wasm = await readFile(path.join(out, 'execute.wasm'));
await writeFile(
  path.join(out, 'build.json'),
  JSON.stringify(
    {
      schema: 1,
      engine: 'Rust / wasm32-unknown-unknown',
      version: (await readFile(path.join(root, 'Cargo.toml'), 'utf8')).match(
        /^version = "([^"]+)"/m,
      )[1],
      bytes: wasm.length,
      wasm_sha256: createHash('sha256').update(wasm).digest('hex'),
      source_sha256: hashes,
    },
    null,
    2,
  ) + '\n',
);
console.log(`Built playground: ${wasm.length} WASM bytes; no runtime CDN dependencies.`);
