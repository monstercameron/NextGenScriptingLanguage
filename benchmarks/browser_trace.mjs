// Serial paired real-WASM measurements. Never run beside builds or other benchmarks.
import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { connect } from "../web/abi.mjs";

const options = Object.fromEntries(
  Array.from({ length: (process.argv.length - 2) / 2 }, (_, n) =>
    process.argv.slice(2 + n * 2, 4 + n * 2),
  ),
);
for (const flag of ["--baseline", "--candidate", "--report", "--host"])
  assert.ok(options[flag], `missing ${flag}`);
assert.ok(["node", "chromium"].includes(options["--host"]));
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const wasm = Object.fromEntries(
  await Promise.all(
    ["baseline", "candidate"].map(async (name) => [
      name,
      await readFile(options[`--${name}`]),
    ]),
  ),
);
const abi = await readFile(new URL("../web/abi.mjs", import.meta.url));
const worker = await readFile(new URL("../web/worker.mjs", import.meta.url));
const payload = '雪\n"\\\u0001'.repeat(3000);
const fixtures = [
  {
    name: "single_applet",
    source: 'run "printf" with arguments ["%s", "hello"]',
    output: "hello",
    calls: 1,
    trace: 1,
  },
  {
    name: "trace_1000",
    source: 'repeat 1000 times\n  run "true"',
    output: "",
    calls: 1000,
    trace: 1000,
  },
  {
    name: "trace_capped_5000",
    source: 'repeat 5000 times\n  run "true"',
    output: "",
    calls: 5000,
    trace: 4096,
    capped: true,
  },
  {
    name: "trace_escaped_budget",
    source:
      'accept message as text\nrepeat 200 times\n  run "printf" with arguments ["%s", message]',
    inputs: { message: payload },
    output: payload,
    calls: 200,
    capped: true,
  },
  {
    name: "pure_json_control",
    source: "repeat 1000 times\n  then encode 42 as json",
    output: "42",
    calls: 0,
    trace: 0,
  },
];
for (const fixture of fixtures) {
  fixture.request = {
    action: "run",
    source: fixture.source,
    inputs: fixture.inputs ?? {},
  };
  fixture.request_sha256 = sha(JSON.stringify(fixture.request));
}
const summary = (raw) => {
  const sorted = [...raw].sort((a, b) => a - b);
  return {
    p50_ms: (sorted[19] + sorted[20]) / 2,
    p95_ms: sorted[37],
    raw_batch_average_ms: raw,
  };
};

function verify(response, fixture) {
  assert.equal(response.ok, true, response.error);
  assert.equal(response.stdout, fixture.output);
  assert.equal(response.applet_calls, fixture.calls);
  assert.equal(response.cache_hits, 0);
  assert.deepEqual(response.files, {});
  assert.equal(response.cwd, "/workspace");
  assert.deepEqual(response.directories, ["/", "/workspace"]);
  if (fixture.trace !== undefined)
    assert.equal(response.trace.length, fixture.trace);
  assert.ok(Buffer.byteLength(JSON.stringify(response.trace)) <= 1024 * 1024);
  if (fixture.capped)
    assert.equal(response.trace.at(-1).operation, "trace truncated");
  if (fixture.name === "trace_escaped_budget") {
    const entryBytes = Buffer.byteLength(JSON.stringify(response.trace[0]));
    const expected = Math.min(
      4095,
      Math.floor((1024 * 1024 - 1024) / (entryBytes + 1)),
    );
    assert.equal(response.trace.length, expected + 1);
    assert.equal(response.trace[0].argv[1], payload);
  }
  return sha(JSON.stringify(response));
}

let browser, page, server;
const sessions = {};
try {
  if (options["--host"] === "node") {
    for (const name of ["baseline", "candidate"])
      sessions[name] = connect(
        await WebAssembly.instantiate(
          await WebAssembly.compile(wasm[name]),
          {},
        ),
      );
  } else {
    const { chromium } = createRequire(
      new URL("../web/package.json", import.meta.url),
    )("playwright");
    server = createServer((request, response) => {
      const route = request.url;
      const label = route.split("/")[1];
      if (route === "/")
        return response
          .writeHead(200, { "content-type": "text/html" })
          .end("<!doctype html><title>Owned WASM benchmark</title>");
      if (!Object.hasOwn(wasm, label)) return response.writeHead(404).end();
      const asset = route.endsWith("/execute.wasm")
        ? wasm[label]
        : route.endsWith("/worker.js")
          ? worker
          : route.endsWith("/abi.mjs")
            ? abi
            : null;
      if (!asset) return response.writeHead(404).end();
      response
        .writeHead(200, {
          "content-type": route.endsWith(".wasm")
            ? "application/wasm"
            : "text/javascript",
        })
        .end(asset);
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    browser = await chromium.launch({ headless: true });
    page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${server.address().port}/`);
    await page.evaluate(async () => {
      window.clients = {};
      for (const name of ["baseline", "candidate"]) {
        const worker = new Worker(`/${name}/worker.js`, { type: "module" });
        const pending = new Map();
        let id = 0;
        await new Promise((resolve, reject) => {
          worker.onmessage = ({ data }) => {
            if (data.ready) resolve();
            else if (data.fatal) reject(new Error(data.fatal));
            else {
              const waiter = pending.get(data.id);
              pending.delete(data.id);
              if (data.error) waiter.reject(new Error(data.error));
              else waiter.resolve(data);
            }
          };
          worker.onerror = (event) => reject(new Error(event.message));
        });
        window.clients[name] = {
          worker,
          call: (request) =>
            new Promise((resolve, reject) => {
              const key = ++id;
              pending.set(key, { resolve, reject });
              worker.postMessage({ id: key, request });
            }),
        };
      }
    });
  }
  const call = async (name, request) =>
    options["--host"] === "node"
      ? sessions[name](request)
      : (
          await page.evaluate(
            async ({ name, request }) =>
              await window.clients[name].call(request),
            { name, request },
          )
        ).result;
  const results = {};
  for (const fixture of fixtures) {
    const oracles = {};
    for (const name of ["baseline", "candidate"])
      oracles[name] = verify(await call(name, fixture.request), fixture);
    assert.equal(
      oracles.baseline,
      oracles.candidate,
      "entire response differs",
    );
    const rounds = [];
    for (let round = 0; round < 3; round++) {
      const order =
        round % 2 ? ["candidate", "baseline"] : ["baseline", "candidate"];
      const variants = {};
      for (const name of order) {
        if (options["--host"] === "node") {
          for (let n = 0; n < 20; n++)
            verify(sessions[name](fixture.request), fixture);
          const raw = [];
          for (let sample = 0; sample < 40; sample++) {
            const start = performance.now();
            let response;
            for (let n = 0; n < 5; n++)
              response = sessions[name](fixture.request);
            raw.push((performance.now() - start) / 5);
            assert.equal(verify(response, fixture), oracles[name]);
          }
          variants[name] = { operation: summary(raw) };
        } else {
          const raw = await page.evaluate(
            async ({ name, request }) => {
              const call = window.clients[name].call;
              for (let n = 0; n < 20; n++) await call(request);
              const roundtrip = [],
                worker = [],
                hashes = [];
              for (let sample = 0; sample < 40; sample++) {
                const start = performance.now();
                let response,
                  work = 0;
                for (let n = 0; n < 5; n++) {
                  response = await call(request);
                  work += response.elapsed;
                }
                roundtrip.push((performance.now() - start) / 5);
                worker.push(work / 5);
                const bytes = new TextEncoder().encode(
                  JSON.stringify(response.result),
                );
                hashes.push(
                  Array.from(
                    new Uint8Array(
                      await crypto.subtle.digest("SHA-256", bytes),
                    ),
                    (value) => value.toString(16).padStart(2, "0"),
                  ).join(""),
                );
              }
              return { roundtrip, worker, hashes };
            },
            { name, request: fixture.request },
          );
          for (const hash of raw.hashes) assert.equal(hash, oracles[name]);
          variants[name] = {
            operation: summary(raw.worker),
            roundtrip: summary(raw.roundtrip),
          };
        }
      }
      rounds.push({ order, variants });
    }
    results[fixture.name] = {
      request_sha256: fixture.request_sha256,
      expected_response_sha256: oracles.baseline,
      expected_stdout_sha256: sha(fixture.output),
      expected_applet_calls: fixture.calls,
      rounds,
    };
    console.log(
      fixture.name,
      rounds.map((round) =>
        Object.fromEntries(
          Object.entries(round.variants).map(([name, value]) => [
            name,
            value.operation.p50_ms,
          ]),
        ),
      ),
    );
  }
  let smallBrowserAggregate;
  if (page) {
    smallBrowserAggregate = await page.evaluate(async (request) => {
      const { connect } = await import("/baseline/abi.mjs");
      const clients = {};
      for (const label of ["baseline", "candidate"]) {
        const bytes = await (
          await fetch(`/${label}/execute.wasm`)
        ).arrayBuffer();
        clients[label] = connect(
          (await WebAssembly.instantiate(bytes, {})).instance,
        );
      }
      const rounds = [];
      for (let round = 0; round < 3; round++) {
        const order =
          round % 2 ? ["candidate", "baseline"] : ["baseline", "candidate"];
        const variants = {};
        for (const label of order) {
          const invoke = clients[label];
          for (let n = 0; n < 100; n++) invoke(request);
          const raw = [],
            responses = [];
          for (let batch = 0; batch < 40; batch++) {
            let response;
            const start = performance.now();
            for (let n = 0; n < 500; n++) response = invoke(request);
            raw.push((performance.now() - start) / 500);
            responses.push(JSON.stringify(response));
          }
          variants[label] = { raw, responses };
        }
        rounds.push({ order, variants });
      }
      return rounds;
    }, fixtures[0].request);
    for (const round of smallBrowserAggregate) {
      for (const [name, value] of Object.entries(round.variants)) {
        for (const text of value.responses)
          assert.equal(
            sha(text),
            results.single_applet.expected_response_sha256,
          );
        round.variants[name] = summary(value.raw);
      }
    }
  }
  const report = {
    recorded_at_utc: new Date().toISOString(),
    host: options["--host"],
    node: process.version,
    v8: process.versions.v8,
    platform: process.platform,
    architecture: process.arch,
    browser: browser?.version(),
    binaries_sha256: Object.fromEntries(
      Object.entries(wasm).map(([name, bytes]) => [name, sha(bytes)]),
    ),
    harness_sha256: sha(await readFile(new URL(import.meta.url))),
    abi_sha256: sha(abi),
    worker_sha256: sha(worker),
    method:
      "Actual preserved release WASM modules, instantiated before timing, serial 3 alternating rounds, 20 warmups then40batches of5 operations. Node operation includes JSON request encode/ABI/Rust/response JSON decode. Chromium operation is the existing worker elapsed clock; roundtrip includes postMessage and scheduling but excludes Playwright evaluation transport. Every batch final response checked against a complete cross-binary hash plus independent output/call-count/FS/trace-cap/UTF8-escaping byte-budget oracles; warmups and fixture/hash checking excluded from timing. Browser clock quantized; batch averages retained, not zero latency. No builds/tests concurrent. No native/OS commands/provider calls; not cold start, memory or Bash comparison.",
    small_browser_direct_aggregate: smallBrowserAggregate,
    small_browser_aggregate_method: page
      ? "Additional browser-main-thread ABI control, 100 warmups then40 batches of500 single-applet operations, 3 alternating rounds. Same actual WASM/ABI and exact response hashes. Resolves per-request worker clock quantization; excludes worker/RPC and setup. Not a worker roundtrip."
      : undefined,
    results,
  };
  await writeFile(options["--report"], JSON.stringify(report, null, 2) + "\n");
} finally {
  if (page)
    await page
      .evaluate(() =>
        Object.values(window.clients ?? {}).forEach((client) =>
          client.worker.terminate(),
        ),
      )
      .catch(() => {});
  await browser?.close();
  if (server) await new Promise((resolve) => server.close(resolve));
}
