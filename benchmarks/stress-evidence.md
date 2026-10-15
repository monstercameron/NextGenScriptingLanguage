# Graph, remote, resource, and fault workloads

These are opt-in measurement harnesses, not retained performance results.
No graph stress run, remote endpoint call, model trial, or Arch/Omarchy
certification has been performed by adding these files.

The graph lifecycle target builds a real typed graph with 2,048 timer workers
by default, one bounded event queue per timer, and a merged event collector.
It records a quiet interval, repeatedly cancels and reconfigures the graph,
consumes outputs from every generation, checks generation/revision identity,
samples Rust process RSS and thread count, captures Linux context switches and
CPU time, and injects an executor failure that must be joined. The harness is
parameterized but enforces at least 1,000 timers. RSS is sampled and may miss
brief peaks; it is the Rust process only. Its polling and metrics
instrumentation affect the workload. It does not stand in for thousands of
IPC services, timers managed by systemd, or a power measurement.

The remote applet target uses the repository's RemoteClient to discover one
explicitly configured endpoint and invoke one read-only or idempotent operation
once per sample. It records discovery duration, request/response sizes,
per-call latency, validated response status, output hashes, transport failures,
and this Linux client's sampled RSS. The client does not retry invocations.
The operator must declare whether the target is loopback, LAN, WAN, or unknown;
only a separately verified remote machine can support a cross-machine latency
claim. Server resources are not visible from this client. Supply an exact
expected-output map to make the operation's functional result checkable;
without one, the harness reports protocol validity only. Never use a repeated
non-idempotent operation.

The graph workload addresses part of P03/P04/P06/P08 and the graph worker
failure slice of S13. The remote workload can produce P07 network samples and
the client-resource slice of P04. Neither closes P02: a generic Linux host is
not an actual Arch/Omarchy session. Neither certifies disk exhaustion,
filesystem permission failures, process crashes, WAN outage recovery, service
restarts, power use, or complete process-tree resource limits. Those require
additional platform-specific fault fixtures and independent retained results.

The mixed graph target exercises actual `RuntimeGraphExecutor` paths in one
bounded graph. Its default is a 32-actor smoke run. The explicit 1,000-actor
layout is 847 timers, 64 finite command applets, 16 distinct registry-owned
JSON-lines workers, 32 in-process applet calls, 32 scheduled `.program`
services, 8 loopback SSE listeners, and one typed timer collector. Timer edges
are lossless with capacity one; the collector consumes them. The probe does
not claim those queues saturated; it does count distinct timer source nodes
and requires each source to emit at least once. A separate
`cancelling_a_service_blocked_by_a_full_lossless_edge_queue_joins_it`
integration regression holds a capacity-one queue full and checks that a
fourth handler process did not launch before cancellation. The mixed graph
admits exactly its actor count through `max_nodes` and `max_parallel`; its
service fixture only encodes in-process and launches no child processes.
Native invocations are capped at 64, worker identities at the default pool's
16, and remote listener/readers at 8. Graph metrics and worker PID markers
record process starts and registry-drop reaping. RSS and thread receipts cover
only the Rust test process; child RSS, peak concurrent process-tree size, and
remote fixture resources are not measured. A mixed report is lifecycle/resource
evidence, not a throughput or whole-host capacity claim.
The graph admits at most the requested number of actor tasks; remote event
readers add at most eight tasks, and the probe itself adds three helper threads.
Runtime command I/O helpers are sampled but have no separate probe-wide thread
cap, so the harness does not assert a global thread ceiling.
The worker PID receipt covers ordinary applet workers retained by the shared
registry pool and reaped when its final owner drops. The separate
`graph_service_cancellation_reaps_only_its_retained_json_lines_worker`
regression covers a graph-owned service worker and cancellation join behavior.

Build the ignored mixed target in a separate quiet release window, then run its
prebuilt test binary with the wrapper. The wrapper defaults to 32 actors; use
`--nodes 1000` only when the machine and release gate are ready. A failure
checkpoint is retained as `<report-stem>-failure.json` with `status` set to
`failed_unverified`; it is never imported as a successful measurement. The
external watchdog runs the test in a process group and terminates that group
on timeout. Fixture worker PIDs are recorded under the wrapper's private
temporary directory; the watchdog validates those marker files and terminates
the workers' separately isolated process groups before retaining the failure
sidecar.

    cargo test --release --test graph_mixed_lifecycle_bench --no-run
    python3 benchmarks/stress_evidence.py mixed \
      --test-binary target/release/deps/graph_mixed_lifecycle_bench-<hash> \
      --report benchmarks/graph-mixed-lifecycle-linux-smoke-2026-10.json \
      --final-release-build-confirmed --nodes 32

After reviewing the smoke report and confirming the runner budget, the same
command can be repeated with a fresh report name and `--nodes 1000`. Do not
overlap the build, the mixed run, or the existing graph stress measurement.
The manual `Mixed runtime lifecycle probe` workflow follows that order: it
always requires the 32-actor smoke first, and its boolean `run_1000` input is
the only path to one capped 1,000-actor run. Both successful raw reports and
failure checkpoints are uploaded even when a step fails. The workflow runs on
Ubuntu CI only; it does not infer desktop or Omarchy capacity.

Compile each ignored release test target in a separate, quiet build window:

    cargo test --release --test graph_lifecycle_bench --no-run
    cargo test --release --test remote_applet_network_bench --no-run

Do not overlap the build, measurements, or ordinary tests. Then pass the
matching prebuilt executable to the wrapper. The wrapper contains no Cargo
call, requires explicit final-release confirmation, accepts only a fresh
named report under benchmarks/, and retains the raw sample plus hashes for
the test binary and relevant source.

    python3 benchmarks/stress_evidence.py graph \
      --test-binary target/release/deps/graph_lifecycle_bench-<hash> \
      --report benchmarks/graph-lifecycle-arch-2026-10.json \
      --final-release-build-confirmed \
      --nodes 2048 --reconfigurations 4

For the remote workload, put the endpoint and, when needed, credential source
name/path in the environment so a bearer token never appears in command
arguments. Give the endpoint operation only if repeated invocation is
read-only or idempotent:

    export REMOTE_STRESS_ENDPOINT='https://service.example'
    export REMOTE_STRESS_OPERATION='status.read'
    python3 benchmarks/stress_evidence.py remote \
      --test-binary target/release/deps/remote_applet_network_bench-<hash> \
      --report benchmarks/remote-applet-network-lan-2026-10.json \
      --final-release-build-confirmed \
      --read-only-or-idempotent-confirmed \
      --network-scope lan --samples 40 --warmups 5

The wrapper records an endpoint hash rather than its URL. REMOTE_STRESS_AUTH_ENV
names an environment variable that contains the bearer token; the token value
is not copied into the report. A private bearer file can be selected with
REMOTE_STRESS_AUTH_FILE. Optional arguments, input, and expected output are
serialized only into local process memory; the report retains their hashes and
never their values.
