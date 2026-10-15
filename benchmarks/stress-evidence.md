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
