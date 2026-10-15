# Default browser fixture provenance and contract

Untouched source: [omacom/omarchy `bin/omarchy-default-browser`](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-default-browser).

- Repository: https://github.com/omacom/omarchy
- Pinned revision: `c668141e9c42b13c80c9ca4ea108e11708c5e8a5`
- `upstream.sh` SHA-256: `4c8254a3fdf4948013901caec2ed847e2eb580b045a4b1322271ab2d94c1bc1f`
- Read-only source checkout HEAD was verified against the pinned revision.
- The original runs with `/bin/bash`; the rewrite runs with the reference Execute binary, overridable using `EXECUTE_TEST_BINARY`.

## Input and native-command mapping

`selection` is text from positional argument 1, with default empty text.
`supplied_arguments` is the complete positional text list: its presence distinguishes
no arguments (query) from one empty argument (invalid selection). Remaining
positional arguments are ignored, matching upstream. Named typed inputs override
these sources; to select a browser without positional arguments, supply both
`--input selection=zen --input 'supplied_arguments=["zen"]'`.
No environment value is read into an Execute input. Native commands inherit HOME,
PATH, locale and other environment values. The real `env -u BROWSER` removes
BROWSER only for `xdg-settings`; notification invocation inherits it unchanged.

The mapping, conditions, ordinary command status handling, and notification
selection are native `.program` statements. Real `jq -Rsj` removes trailing LF
bytes solely to model Bash command substitution. Real `printf` emits names and
combines successful set/notification output; real `false` supplies status 1.
There is no shell invocation, generated business logic, or call to upstream in
the rewrite. Execute must include process inputs, loops, reusable finite programs,
conditions, and native status capture.

## Isolation and observations

Tests create a private temporary HOME and PATH containing real env/jq/printf/false
symlinks and private `xdg-settings` / `omarchy-notification-send` mocks. No host
xdg-settings or desktop notification executable can be resolved. Mock set effects
write only the private `browser-state` file. Python is test instrumentation only.
Each run records argv, BROWSER presence, stdout, stderr, exit status, notification
records, browser state, HOME contents, and whether a metacharacter payload created
an unwanted file. Original and rewrite start with separate cleared observations.

Coverage includes all seven browser query/set mappings, known query with trailing
newlines and nonzero probe, unknown query's second lookup, unknown second-lookup
failure still returning zero, empty positional selection, invalid/metacharacter
selection, ignored literal metacharacter extras, set failure, notification failure,
output composition, and explicit typed input overrides.

## Measured compatibility limits

The tests assert these differences explicitly rather than claiming byte parity:

- Invalid selection: upstream prints Usage to stdout and exits 1. Execute returns
  1 and runs no XDG/notification command, but discards the earlier finite Usage
  output on the final native failure and appends a native failure diagnostic.
- Failed set: both return 1 and skip notification. Execute adds its failure
  diagnostic; any stdout from the failed operation is discarded.
- Failed notification: both preserve the notification status (tested as 23) and
  retain the successful set side effect. Execute adds its native failure
  diagnostic and discards finite stdout from that invocation and preceding set.
- Successful set/notification stdout bytes are concatenated exactly, but Execute
  buffers them until success; streaming timing is different.

Additional runtime boundaries: named captures and process inputs require UTF-8;
NUL handling is not equivalent to Bash command substitution. The finite capture
limit applies, and a very large ignored argv list can reach Execute's loop guard
(100,000 iterations). Missing executables, signals, timeouts and invalid encodings
are runtime failures rather than ordinary captured statuses; these are not claimed
as Bash-equivalent. This fixture does not exercise a live desktop.

## Run

From the repository root:

```sh
python3 -m unittest discover -s tests/omarchy_rewrites -p test_06_default_browser.py -v
```

Set `OMARCHY_REWRITE_EVIDENCE_DIR` to retain per-case JSON observations. The original
reference run wrote evidence to
`work/omarchy-rewrites-2026-10-15/06/` in the parent workspace.
