import { connect } from './abi.mjs';
let invoke;
const ready = (async () => {
  const response = await fetch(new URL('./execute.wasm', import.meta.url));
  if (!response.ok) throw new Error(`WASM download failed: HTTP ${response.status}`);
  const bytes = await response.arrayBuffer();
  const { instance } = await WebAssembly.instantiate(bytes, {});
  invoke = connect(instance);
  postMessage({ ready: true, bytes: bytes.byteLength });
})();
self.onmessage = async ({ data }) => {
  try {
    await ready;
    const start = performance.now();
    const result = invoke(data.request);
    postMessage({ id: data.id, result, elapsed: performance.now() - start });
  } catch (error) {
    postMessage({
      id: data.id,
      error: error.message,
      restart: error instanceof WebAssembly.RuntimeError,
    });
  }
};
ready.catch((error) => postMessage({ fatal: error.message }));
