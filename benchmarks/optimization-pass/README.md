# Measured optimization pass, macOS arm64

The eight-hour optimization campaign was ended early at the user's request,
after roughly 2 hours 40 minutes of work. The next implemented item is the
[persistent stream mode](../../docs/streaming.md). This is not a claim that every
feasible optimization was exhausted.

The files here preserve measurements from the retained changes and the rejected
hex-formatting experiment. Builds and timing phases were serialized. Fresh native
commands, cache hits, durable checkpoints, and in-process operations are distinct
workloads; their costs must not be conflated. Background host activity remained
uncontrolled. Numbers below are approximate representative p50s or batch medians,
not universal performance guarantees or a fresh whole-release benchmark.

| Measured operation | Before | After | Qualification |
| --- | ---: | ---: | --- |
| Parse 1,000 statements | ~440 µs | ~203 µs | Original baseline and later parser trial; token arena and borrowed lexical text |
| Render 10,000-row table | ~6.2 ms | ~102 µs | Paired viewport benchmark; visible rows only |
| 32-widget literal dispatch | ~5.1 µs | ~3.9 µs | Three alternating paired trials |
| 64 KiB final JSON field projection | ~26.3 µs | ~23.3 µs | Three paired trials; move final owned field |
| Worker with 1 MiB captured stderr | ~680 ms | ~28 ms | Readiness polling removes drain stalls; includes worker startup |
| Interactive submit through printf/tr to render | ~22.5 ms | ~3.8 ms | Three paired 120-sample PTY trials |
| Fresh external printf, lazy CF experiment | ~8.68 ms | ~6.93 ms | Isolated full-interpreter source pair |
| PATH lookup after 64 misses | ~99 µs | ~89 µs | Warm metadata, same-process alternating microbenchmark |

Interactive completion wakeup adds a dedicated input reader. Measured quit time
rose from about 1–4 ms to 36–39 ms because the reader is joined before restoring
the terminal. Its 20 ms poll also adds an idle wake source. Startup fields in the
original PTY JSON include PTY setup before Popen despite their original name;
they are not pure loader timings. The current harness label is corrected.

The 8–64 KiB stdin prefill experiment reduced parent-process CPU by about 10–11%
and involuntary context switches by about 20%, with wall time essentially flat.
The 512-byte unchanged control remained flat. Parent rusage excludes child CPU.
Error paths retain detached writer semantics so escaped pipe readers cannot turn
error propagation into an unconditional join hang.

A custom fixed-table SHA-256 hex formatter measured slower (~57 ns versus ~53 ns)
and was removed. A global SIGCHLD notification experiment was removed after review
found that it changed an embedding host's signal behavior; macOS now uses a local
kqueue with a bounded poll fallback. No LLVM/JIT, CPU-specific compilation,
relaxed durability, cross-node binary memoization, or weakened validation was used.

Lazy CoreFoundation loading preserves timezone lookup output but can omit a
CoreFoundation-added environment variable until timezone lookup occurs. Cache
keys still cover the full effective environment, so old entries can miss when
that environment changes. The controlled-environment compatibility report verifies
old cache consumption and checkpoint recovery in both directions.

Durable checkpoint event time remained around 70 ms on this host; fsync dominates.
Native launch and applet buffering still dominate many real scripts. Intermediate
native pipes retain OS backpressure. Finite final results remain buffered; the
new explicit stream mode supplies a bounded line sink. Linux cross-compilation
is checked separately; these latency measurements are from macOS only.
