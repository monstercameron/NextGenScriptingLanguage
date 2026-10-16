# Graph, remote, resource, and fault workloads

These opt-in measurement harnesses include retained lifecycle evidence: the mixed RuntimeGraphExecutor probe has a 32-actor smoke and an explicit 1,000-actor Linux run in [workflow 38833931575](https://github.com/monstercameron/NextGenScriptingLanguage/actions/runs/38833931575), built from [commit 58b42f6](https://github.com/monstercameron/NextGenScriptingLanguage/commit/58b42f65d8c4de171baf8f3049ab70cf37c51af4). The [retained reports and summary](mixed-runtime-ci-38833931575/README.md) include source-bound JSON and the earlier failed attempt.
A [separate fixed replication](mixed-runtime-ci-38835685785/README.md) retained the 32-actor smoke and all three declared 1,000-actor, 10-second trials. Every trial observed all 847 timer sources during its active window and reaped all 16 registry workers; measured Rust-process RSS peaks were 43,819,008–45,264,896 bytes. These remain bounded repetitions, with no process-tree resource or sustained-operation claim.

The separate graph lifecycle stress target below remains unrun. These receipts cover loopback remote fixtures only; no external remote endpoint, model trial, or actual Arch/Omarchy session is certified.

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
remote fixture resources are not measured. Before its active window, the probe
waits up to ten seconds for every expected terminal actor and every timer source
to emit once. The report records readiness status, exact missing source IDs,
and warm-up duration separately. A dedicated watchdog cancels the graph at the
readiness deadline even while terminal output is stalled; its final status and
join are recorded. Readiness claimed after the deadline fails and does not start
or validate an active window. The active window therefore starts after the
first observed output from every declared source. The report requires its
measured active duration to meet the requested window and remain within the
graph deadline budget plus an explicit one-second observation tolerance. The
Rust collector snapshots monotonic event counters at readiness, window start,
window end, and total completion; the wrapper checks nonnegative counter
receipts, exact deltas, and positive progress during the active window. While
the window is active, a bounded source-ID set records which timer nodes actually
delivered an event; every declared timer must progress during that window.
Events counted during readiness warm-up are reported separately and do not
satisfy active-window progress. This source tracking uses a mutex in the
collector and adds instrumentation overhead. A mixed report is lifecycle and
resource evidence, not a cross-revision throughput or whole-host capacity
claim.
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
`--nodes 1000` only when the machine and release gate are ready. The wrapper
accepts a bounded `--window-ms` from 100 through 300,000; the workflow offers
1,000, 10,000, and 300,000 ms choices. A failure
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
always runs the 32-actor, 1,000 ms correctness smoke first. Its boolean
`run_1000` input gates the optional 1,000-actor trials; the fixed choices are
one or three trials and 1,000, 10,000, or 300,000 ms active windows. When
`run_1000` is enabled, the 300,000 ms option requires exactly one trial, and
invalid 1,000-actor selections are rejected before checkout, build, or the
32-actor smoke. Defaults remain one trial and 1,000 ms; the 1,000-actor run
stays opt-in. Every declared trial runs even after a failure, reports have
unique run/trial names, all
failure sidecars are retained, and the job fails if any trial fails. A 10 s
window repeated three times is a bounded replication check under one Linux CI
environment. It is not a long-running or churn test, a process-tree resource
measurement, evidence for 1,000 independent capabilities, or a general
capacity guarantee. A single 300 s active window is still a bounded observation
of one fixed graph, not production sustained-availability, long-run reliability,
churn, or general-capacity evidence; RSS remains probe-process-local and no
process-tree measurement is added. Successful reports and failure checkpoints
are uploaded even when a step fails. The workflow runs on Ubuntu CI only; it
does not infer desktop or Omarchy capacity.

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
