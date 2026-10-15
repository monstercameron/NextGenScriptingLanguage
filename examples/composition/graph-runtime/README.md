# Runtime graph composition proof

`graph.program` is a parsed runtime graph with four supported execution paths:

- `composition.remote_feed` supplies three typed records over a loopback SSE
  fixture.
- The interface-2 `composition.scheduled_status` module emits a typed record
  on its `updated` event.
- Both event sources feed the same `composition.worker_transform` JSON-lines
  applet. One retained worker process transforms all four records in a
  generation and numbers them in order.
- `composition.native_stream` supplies typed JSON-lines records. The worker
  records and native stream records reach the renderer through capacity-one,
  lossless queues.

Run the architecture proof with:

```sh
cargo test --test graph_composition_acceptance -- --test-threads=1
```

The test creates temporary manifests for the loopback endpoint and worker
marker files. Its native stream invokes `/bin/echo` once, emits one JSON record,
and exits; the remote feed, scheduled service, and worker keep each graph
generation active. It executes the checked-in graph
through `RuntimeGraphExecutor`; only `tui:text` is replaced by an injected
headless collector. The collector exercises the renderer's real typed input
ports but does not claim to launch a desktop UI or draw an actual terminal
surface.

The test waits for all remote records, one scheduled event, and one native
stream record in each generation. It checks transformed event numbering,
worker request markers, exact successful process-spawn metrics, socket closure,
and worker-process reaping across reconfiguration and cancellation. Each
generation uses a fresh `Registry` because ordinary JSON-lines applet workers
are owned by the registry's worker pool; dropping the old graph executor after
its tasks join closes that pool and its retained worker.

The scheduled service is periodic, so this example is a continuous graph rather
than a finite command. The test reconfigures after the first scheduled event in
each generation and cancels the final generation after the same evidence has
arrived.
