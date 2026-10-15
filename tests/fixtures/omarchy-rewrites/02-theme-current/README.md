# Current-theme rewrite fixture

`upstream.sh` is the unmodified 12-line `bin/omarchy-theme-current` from
[omacom/omarchy at c668141e9c42b13c80c9ca4ea108e11708c5e8a5](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-theme-current).
`provenance.json` records its SHA-256 and Git blob identity. The test verifies
the SHA-256 of this frozen oracle. The original checkout is
read-only; this snapshot permits running the same test after moving the repo.
Root-level rewrite documentation supplies the upstream MIT license notice.

The candidate declares `home` from `HOME` (empty default when unset, as in Bash)
and an `arguments` list for the positional parameters upstream accepts and
ignores. Explicit `--input home=...` overrides `HOME`. It constructs the path
with native `printf`, captures native `test -f` status, and selects the actual
sed or missing-file branch in Execute. It never invokes Bash or this oracle.
The path-producing printf has no newline, preserving spaces and literal shell
metacharacters in one subsequent argv item. The sed expression is unchanged.

Run from the Execute repo root:

```console
python3 -m unittest discover -s tests/omarchy_rewrites -p test_02_theme_current.py -v
```

`EXECUTE_TEST_BINARY` can select an existing Execute build. The default is the
task's pinned `work/omarchy-rewrites-2026-10-15/execute-base`; tests never build.
Each paired run gets a temporary HOME, cwd, theme file, and restricted PATH.
Test-only `argv_probe.py` records native argv before `exec` of the real host
`printf`, `test`, or `sed`; it performs no theme business logic. Bash's `[[ ]]`
and `echo` remain builtins. Cases assert stdout, stderr, exit status, exact
argv, and absence of command-substitution marker files. No desktop service,
provider, network, privilege, or real user theme is accessed.

`oracles.json` preserves explicit GNU and BSD output expectations for ordinary
names, missing files, directories, spaces, repeated/trailing hyphens, literal
metacharacters in both HOME and file contents, empty/unterminated/multiline
files, and a symlink to an owned regular file. Additional tests exercise ignored
literal positional arguments, environment override, static validation, and
machine-readable input discovery.

The source assumes GNU sed's `\u` uppercase replacement. macOS BSD sed instead
inserts a literal `u`, so `rose-pine` becomes `urose upine`. The rewrite preserves
this host-dependent behavior honestly: it launches the identical sed argv. The
test recognizes these dialects and checks the corresponding frozen output; it
does not simulate GNU sed or install tools. Initial verification used macOS
26.6.2 arm64 with BSD sed, so GNU expected outputs are recorded but remain
unverified here. This is filesystem-fixture parity, not Linux desktop proof.

Limits: permission/read races, non-UTF-8 HOME, command spawn failures, and
resource limits are not exercised. Execute adds its own diagnostics after
unhandled native failures; exact failure-stderr parity is not claimed. Its
finite output bound also differs from Bash's unbounded streaming. Tests do not
run with empty or unset HOME because those resolve outside the owned fixture.
