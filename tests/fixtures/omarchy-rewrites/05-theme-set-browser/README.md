# 05: browser theme wrapper

Source: [Omarchy bin/omarchy-theme-set-browser at c668141e9c42b13c80c9ca4ea108e11708c5e8a5](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-theme-set-browser).
`provenance.json` records SHA-256 and immutable URLs for all three untouched source files. The two sourced helpers were recovered using read-only `git show` because this checkout sparsely materializes `bin/`.

The `.program` implements RGB validation/defaulting, policy invocation, all five browser refresh paths, and the stable Chrome failure fallback. Control flow resides in Execute; generic `fs.snapshot` and `color.parse_rgb` applets read/convert the theme bytes. RGB component trimming is ASCII-only to match the source's `LC_ALL=C` whitespace class. Read errors are routed to a visible warning and the original default color. `/usr/bin/env` launches the selected browser directly with literal argv. Its fixed refresh manifest discards browser stdout and stderr at the child boundary, so large refresh output is not buffered as a workflow value. Native Omarchy policy writing remains a delegated API, like the original wrapper; this is not a rewrite of its privileged implementation.

Inputs map `home: text` from HOME, ignored positional arguments (accepted for CLI compatibility), and an optional `theme_file: text` override. With no override the exact original theme location under HOME is constructed. `OMARCHY_PATH` is needed by the Bash oracle to source untouched helpers, but the rewrite uses the reusable RGB conversion applet and fixed default rather than sourcing shell code.

The differential tests use fresh owned temporary HOME directories, fake preferences, a fake policy directory under HOME, and recording executable mocks for every browser, pgrep, omarchy-cmd-present, and policy command. Only the test harness/mock backends use Python. No native browser, gsettings, desktop, policy utility, sudo, pkexec, /etc write, or real profile is invoked. The original wrapper never calls gsettings or edits Preferences; fake profiles are checked for byte-for-byte preservation.

Assertions compare exact logged native utility argv, resulting HOME files, stdout, stderr, and process status for dark/light RGB, defaults, missing and directory theme paths, malformed/out-of-range values, leading zeros, multiline whitespace, all running/installed states, Chrome fallback, and paths with spaces. Native test/jq/printf/env are local applets; the mock log deliberately records the original application API boundary only.

The retained local oracle uses Apple Bash 3.2.57. Its unquoted `$(<$CHROMIUM_THEME)` raises `ambiguous redirect` when HOME contains spaces, then falls back to the default color. The rewrite handles that literal path successfully. A dedicated differential test records this source/host discrepancy; set `OMARCHY_TEST_BASH` to a Bash 5 executable to validate the intended Linux oracle separately. Bash 5 was unavailable locally, so its behavior is not certified here.

Known language limits are tested, not represented as full Bash parity:

- The fixed browser-refresh manifest discards both child streams at the process boundary, matching Bash's `&>/dev/null` without buffering refresh stdout. `pgrep` and the delegated policy command still use the runtime's native stream transport.
- There is no builtin `exit failed`. A final native `false` preserves normalized policy-failure status 1 after refreshes, while Execute adds a native-failure diagnostic. On this path Execute also discards policy stdout; the original streams it. On success policy stdout bytes are preserved but buffered until refreshes complete.
- Shell-source failures, NUL-containing theme files, read-permission races, signal/spawn failure transport, privileged policy file permissions, and live desktop behavior are not certified. Tests use UTF-8 text and silent mock normal paths.

Run from the repository root:

```sh
EXECUTE_TEST_BINARY=/path/to/current/execute python3 tests/omarchy_rewrites/test_05_browser_theme.py -v
```
