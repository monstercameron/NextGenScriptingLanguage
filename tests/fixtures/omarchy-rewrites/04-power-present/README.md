# Power-present differential fixture

`upstream.sh` is the unchanged `bin/omarchy-power-present` from official
[omacom/omarchy](https://github.com/omacom/omarchy), pinned to commit
`c668141e9c42b13c80c9ca4ea108e11708c5e8a5`:
[source at that commit](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-power-present).
Its SHA-256 is `4691bfec9ec4123d4e5caa29e32c9b2dcbf72f2a39bdeb81121edfcad3e9a1c1`.
The adjacent `LICENSE` preserves the upstream MIT permission notice and
copyright attribution to David Heinemeier Hansson.

The rewrite uses real native `find`, `jq`, `printf`, and `test`. Its `.program`
owns the iteration, readable-file checks, Mains/USB selection, online comparison,
and result status. `jq` only transports paths and trims trailing newlines, plus
normalizes the input's empty-string default. There are no mock executables,
shell adapters, Python controllers, or calls to the original script in the
rewrite. Python is only the differential test harness.

Tests create owned temporary fake power-supply trees. Every execution supplies
`OMARCHY_POWER_SUPPLY_PATH` or an explicit `power_supply_path` input pointing
inside that temporary root. They never inspect or change actual sysfs or
hardware. File content, modes, names, symlinks, and FIFO identity are compared
before and after both executions. The test is safe to run as an ordinary user;
permission cases skip if the current account bypasses mode bits.

From the repository root:

```sh
EXECUTE_TEST_BINARY=/absolute/path/to/execute python3 -m unittest discover -s tests/omarchy_rewrites -p test_04_power_present.py -v
```

`EXECUTE_TEST_BINARY` overrides the local pinned `execute-base` default.
Set `OMARCHY_REWRITE_EVIDENCE_DIR` to retain JSON observations under its
`04-power-present` subdirectory.

Known differences and limits:

- Success returns 0 with empty stdout/stderr. No power returns 1 with empty
  stdout; Execute additionally emits its native-command failure context for the
  final `test`. Upstream's ordinary no-power result has empty stderr.
- Execute has no `break`, `continue`, or explicit exit statement. A `found`
  guard prevents reads after the first success, but remaining list entries still
  enter guarded iterations. No early exit is claimed. Execute's shared 100,000
  iteration bound can reject extremely large trees even after an early match.
- Native `find` discovery is buffered as a JSON list and subject to Execute's
  output limit. The root changes only the child runtime's working directory.
  The caller and fixtures are unchanged.
- Enumeration is sorted as Unicode text, matching Bash glob order in the tested
  C locale for ordinary ASCII supply names. Other locale collation, non-UTF-8
  paths/content, embedded NUL content, and concurrent filesystem mutation are
  not claimed equivalent. Bash strips NUL bytes in command substitution while
  `jq` preserves them; such files are outside tested sysfs text contracts.
- A missing, unreadable, or non-directory root yields the ordinary false result.
  Errors arising after preflight (for example a root removed during `find`, a
  readable directory in place of a scalar file, or a read race) can emit native
  utility/Execute diagnostics different from Bash. Native spawn and I/O errors
  remain Execute failures. Missing dependencies are not simulated.
- The program preserves unset/empty environment fallback to
  `/sys/class/power_supply`; the fallback is documented and statically checked
  only. Tests deliberately never execute against that actual system path.
- Extra positional arguments are accepted and ignored, matching upstream.

The upstream `AGENTS.md` was read. Its referenced
`agents/skills/command-metadata.md` does not exist in the supplied pinned
read-only snapshot. No upstream files are changed.
