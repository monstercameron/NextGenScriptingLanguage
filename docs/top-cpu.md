# Top ten CPU processes

From the project directory:

```sh
bin/execute examples/top-cpu.program
```

The app shows ten horizontal bars labelled with PID and executable name, with
fractional CPU percentages and a matching process table. It refreshes every two
seconds. Ctrl-R refreshes manually; Ctrl-Q closes it. Use an 80×24 or larger
terminal. `ps` and `jq` must be available on PATH; both are installed on the tested
Mac. The supplied executable is macOS arm64; build the Rust project for Linux.

All event routing and applet sequencing are in the single `.program` file. The
monitor component runs the pipeline, updates its bar chart, and offers JSON records
through a text port. The details component accepts those records and displays them.
Rust supplies a reusable bar-chart widget; it does not choose CPU queries, rankings,
refresh times, keyboard targets, or result destinations.

```text
show a bar chart named usage
  label each bar from "label"
  size each bar from "cpu"
  label bar values with "%"

whenever the program opens
  press refresh

every 2 seconds
  press refresh

whenever refresh is pressed
  run ps with flags ["-A", "-o", "pid=", "-o", "pcpu=", "-o", "comm="]
  pipe to jq.ps with limit 10
  then show the result as bars in usage
  then send the result as processes
```

`ps` requests headerless PID, CPU percentage, and command-name fields. The reusable
`jq.ps` manifest invokes real `jq` to decode those fields, convert numbers, rank by
CPU descending (PID ascending for ties), and apply the script's limit. It accepts
0–256 records as the limit. Names with spaces, quotes, and Unicode are preserved;
decimal commas are converted before numeric sorting. All input is drained before
ranking and limiting, so this pipeline avoids early-reader SIGPIPE failures.
The adapter is in `manifests/jq.ps.json`; the general `jq` binding is also available.

Each bar shows the reported number, with length relative to the largest value in
the current sample. Values above 100% are supported. The app uses `ps` sampling
rules: Linux procps reports a lifetime CPU-time/elapsed-time ratio, rather than a
two-second utilization delta. The refresh interval controls how often the command
runs; it does not change how that command calculates percentages.
[procps ps manual](https://man7.org/linux/man-pages/man1/ps.1.html),
[jq manual](https://jqlang.org/manual/v1.7/).

Bar charts are generic: labels and sizes come from explicitly named JSON fields.
Sizes must be finite, nonnegative numbers, and at most 256 bars are retained. Long
labels are shortened by display columns. Failed or malformed samples leave the
last successful data visible and follow the script's error handler. A valid new
sample clears the previous validation diagnostic.

For one sample with JSON state and a rendered 100×30 frame:

```sh
bin/execute examples/top-cpu.program --headless
bin/execute check examples/top-cpu.program
bin/execute explain examples/top-cpu.program
```

The app only reads process data; it does not signal or change other processes.
Fixture tests replace `ps` and use real `jq`, covering ranking, limits, fractional
and multi-core values, malformed/empty/large output, and declared widget types.
PTY tests exercise the actual program's manual refresh, two-second timer, script
quit, and terminal restoration. Linux CI is configured; local verification was on
macOS.
