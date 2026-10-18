# Audio output volume differential fixture

The unchanged `upstream.sh` is Omarchy's
[`bin/omarchy-audio-output-volume`](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-audio-output-volume)
at commit `c668141e9c42b13c80c9ca4ea108e11708c5e8a5`, SHA-256
`08611ec849c020625b784b360a84aed0bc43b93ba82c7f02616bf5e1917d8c80`.
Omarchy is MIT licensed; see `examples/omarchy-rewrites/UPSTREAM-LICENSE`.

Run from the Execute repository:

```sh
python3 -m unittest discover -s tests/omarchy_rewrites -p test_09_audio_volume.py -v
```

`EXECUTE_TEST_BINARY` overrides the frozen default
`work/omarchy-rewrites-2026-10-15/execute-base` in the outer workspace.
`OMARCHY_REWRITE_EVIDENCE_DIR` saves one paired JSON observation per scenario.

Each side receives the same isolated PATH with real jq, awk, test, printf, and
false, plus response-only date, pactl, sink resolver, and OSD fixture endpoints.
The endpoint Python only records argv and runtime bytes visible at each call,
then emits configured stdout, stderr, and status. It does not implement volume
logic and never invokes audio services. Each invocation gets a freshly reset
owned HOME and XDG_RUNTIME_DIR, including paths with spaces; no test writes the
real `/tmp` debounce file. Shell metacharacters in the sink and arguments remain
literal native argv. An assertion detects any unexpected `unsafe` file.

The `.program` owns empty/unknown-action rejection, sink checking, raise/lower
mapping, custom-step dispatch, missing-volume handling, direction-specific
clamps, unmute/set ordering, timestamp persistence, debounce branching, muted
and zero-volume icons, and OSD calls. Same-file Execute programs share percent
parsing, adjustment, and OSD behavior. Generic integer parsing and arithmetic
applets handle volume steps/current values, directional comparisons, and
clamps; jq retains regular-expression extraction and elapsed-time JSON
arithmetic, while awk retains the upstream first-percentage-on-first-line
parser. Leading-zero steps preserve Bash's octal interpretation. Debounce
timestamps use `fs.snapshot` and compare-and-swap `fs.write_atomic` with an
explicit symlink policy. A snapshot/write failure emits a warning and still
allows the mute action to proceed. Every hardware call is direct argv; there
is no Bash/Python/JavaScript application wrapper in the rewrite.

Cases cover default and custom steps, literal ignored extras, zero step,
0/100 clamps, one-sided clamp behavior outside the usual range, sink errors,
unreadable current volume, first-line parsing, muted/zero OSD, trailing-LF sink
normalization, failed query with usable output, both setter failures, toggle and
OSD failures, missing/empty/whitespace timestamp data, 0/1/249/250/251 ms
debounce boundaries, and future timestamps. Exact endpoint order, argv,
stdout/status, and runtime contents at each endpoint are compared, with the
following differences explicitly asserted or bounded.

Known limits:

- Native stdout streams as it does upstream and is kept on an unhandled native
  failure. Usage and unknown-action messages, the pactl setter output and a
  failed OSD's partial stdout all appear in upstream order. Failure statuses add
  Execute's contextual diagnostics.
- Quoted direct native commands can discard only their own stderr with
  `and discard stderr`; the two pactl queries use it to match upstream's
  `2>/dev/null`. Upstream custom error messages are emitted to stderr with
  native awk and preserved.
- Malformed current-volume identifiers and invalid octal steps are explicitly
  rejected and reported before audio writes. The Bash oracle treats some such
  strings as arithmetic variable names and continues; that behavior is a
  deliberate safety improvement. General shell arithmetic, integer overflow,
  and integers beyond the applet's supported range are not certified.
- Atomic writes retain a stable lock sidecar, preserve file permissions, and
  follow an existing timestamp symlink. A failed read/write is reported, with
  unreadable debounce state treated as zero so the mute action can continue.
  Tests filter the lock sidecar from the Bash-versus-Execute state comparison
  and assert that the lock exists on successful persistence.
- The host is macOS with Bash 3.2 and BSD date: native `date +%s%3N` prints a
  literal `3N` suffix rather than GNU milliseconds. Tests use a deterministic
  date endpoint to exercise the actual unmodified upstream Bash debounce flow.
  This validates the 250 ms algorithm, not GNU date integration or live Linux
  audio. No packages were installed. The host probe is retained in task evidence.
- Real sink resolution is a separate Omarchy script and remains an endpoint
  boundary, as do PulseAudio, OSD delivery and concurrent debounce races. NUL
  text, binary endpoint output and real `/tmp` fallback writes are not certified.

These are bounded differential results, not full Bash equivalence or a live
desktop test.
