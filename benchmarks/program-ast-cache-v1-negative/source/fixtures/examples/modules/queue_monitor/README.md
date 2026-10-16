# Scheduled queue monitor module

This example exports a scheduled `.program` as a typed graph applet. The
service reads the queue file every second and emits one `updated` record with
the overdue count and configured threshold. The applet descriptor and package
manifest repeat the same interface, and the checked-in lock pins the package
manifest and source hashes.

From the repository root, run:

```sh
execute --manifest-dir examples/modules/queue_monitor \
  examples/modules/queue_monitor/queue_monitor.graph.program
```

The graph prints each typed event as JSON. Its sample queue is in
`data/queue.json`; change the path and threshold in `queue_monitor.graph.program`
to inspect another queue. Press Ctrl-C to cancel the graph and stop the
scheduled service.
