# Workspace layout differential fixture

The unchanged `upstream.sh` comes from Omarchy's
[`bin/omarchy-hyprland-workspace-layout-toggle`](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-hyprland-workspace-layout-toggle)
at revision `c668141e9c42b13c80c9ca4ea108e11708c5e8a5`.
SHA-256: `0e35529ed0391c2a990f5cca02a4a08a6edf471ba65aa1b65c53c3eb1f28244c`.
The upstream project is [omacom/omarchy](https://github.com/omacom/omarchy);
the copied license is `examples/omarchy-rewrites/UPSTREAM-LICENSE`.

Run from this repository:

```sh
python3 -m unittest discover -s tests/omarchy_rewrites -p test_07_workspace_layout.py -v
```

Set `EXECUTE_TEST_BINARY` to override the default immutable candidate at
`/Users/earlcameron/Documents/Codex/2026-10-12/you/work/omarchy-rewrites-2026-10-15/execute-base`.
Set `OMARCHY_REWRITE_EVIDENCE_DIR` to write paired JSON observations.

The test grants each process a PATH containing real `jq`, `mkdir`, `printf`,
and `false`, plus only the owned `hyprctl` and `omarchy-notification-send`
fixture endpoints. Those endpoints record exact argv and the file bytes visible
at each call, then return configured stdout, stderr and status. HOME is an owned
temporary directory containing spaces. Tests never reach a Hyprland socket or
the user's notification service. The Python fixture implements endpoint
responses only; application control flow is in the `.program` file.

The rewrite consumes typed HOME and ignored positional inputs, normalizes
Bash/jq text spelling with jq, selects typed JSON id/layout text and a boolean
guard, and owns both layout/fallback branches and the native file save. The
state write uses `fs.snapshot` plus compare-and-swap `fs.write_atomic`, with an
explicit follow-symlink policy and a stable lock sidecar. Snapshot, directory,
and save failures are reported to stderr while compositor evaluation and
notification still proceed. Ids retain their spelling, including negative
numeric strings with leading zeroes.
The `^-?[0-9]+$` guard executes before the second query, directory creation,
Lua save, layout application and notification. Native argv preserves spaces.

Known runtime differences:

- Execute has no per-command stderr redirection. Eval stdout is discarded,
  but eval stderr is visible; upstream redirects both. A differential test
  records this gap explicitly rather than claiming exact stderr parity.
- Native failures preserve status and add Execute diagnostics. Invalid ids
  therefore return the same status 1 and make no changes, with extra stderr.
- A failed notification's stdout, and earlier fallback stdout if notification
  fails, can be discarded by Execute's failed finite-output contract. Successful
  fallback and notification output is concatenated in exact upstream order.
- Native atomic save preserves existing permissions and follows a state-file
  symlink to its target. Its retained lock sidecar is an intentional atomicity
  artifact: when a save occurs, tests require the exact sibling lock file to
  remain empty and mode `0600`, and account for that file in later fixture
  observations. Tests also verify symlink preservation and the update to the
  target.
- Malformed first-query JSON still prevents action. A malformed second-query
  response is explicitly routed to the source's empty-layout/default `dwindle`
  arm after the parse status is captured. Empty query output and nonzero query
  status with valid JSON are covered.
- NUL-containing strings are rejected at Execute's typed text boundary; Bash
  command substitution may remove NUL bytes. This fixture does not certify them.

These are candidate validation results, not a claim of full Bash equivalence.
