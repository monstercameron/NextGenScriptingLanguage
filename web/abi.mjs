// No JavaScript parser or workflow evaluator: JSON crosses the owned-buffer Rust ABI.
export function connect(instance) {
  const { memory, allocate, execute, release } = instance.exports;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  return (request) => {
    const bytes = encoder.encode(JSON.stringify(request));
    if (bytes.length > 8 * 1024 * 1024) throw new Error('Request exceeds 8 MiB');
    const pointer = allocate(bytes.length);
    if (!pointer) throw new Error('WASM allocation refused');
    new Uint8Array(memory.buffer, pointer, bytes.length).set(bytes);
    const packed = execute(pointer, bytes.length); // Rust consumes the request allocation.
    const output = Number(packed >> 32n);
    const length = Number(packed & 0xffffffffn);
    try {
      return JSON.parse(decoder.decode(new Uint8Array(memory.buffer, output, length)));
    } finally {
      release(output, length);
    }
  };
}
