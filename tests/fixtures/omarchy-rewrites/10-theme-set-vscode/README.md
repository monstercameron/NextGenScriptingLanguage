# VS Code / VSCodium / Cursor theme rewrite

The fixture contains the untouched MIT-licensed [official script](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-theme-set-vscode) and upstream LICENSE. `provenance.json` records the source SHA-256 hashes and exact revision `c668141e9c42b13c80c9ca4ea108e11708c5e8a5`. The rewrite is a derived work under that license. Acceptance tests check the original hashes and never edit pinned source files.

`10-theme-set-vscode.program` owns editor selection, toggle checks, descriptor-versus-generated precedence, validation/install decisions, directory and symlink creation, registry/obsolete repair, and settings mutation. Generic `path.dirname`, `fs.snapshot`, `fs.write_atomic`, `fs.transaction`, `fs.unlink_checked`, `json.parse`, `json.get`, `json.stringify`, `json.filter`, `json.set`, `jsonc.set`, and `jsonc.remove` applets supply reusable path, snapshot, atomic update, JSON, and JSONC operations. There is no application-specific Rust code. Registry entries are filtered and assembled through generic JSON operations; `jq` remains for simple `.obsolete` key removal/counting, JSON pretty-formatting, and extension-id pattern validation. Fixed editor-list/install manifests preserve literal argv and route process status; list stderr and installation stdout/stderr are discarded at the process boundary. Failed list/install attempts are reported as warnings, and the settings action continues according to the upstream ordering.

Inputs: `home` defaults from HOME. Positional arguments are accepted and ignored, matching the source CLI. Optional `cursor_command` defaults to the upstream `/usr/bin/cursor`; `--input cursor_command=cursor-mock` explicitly maps the otherwise hardcoded executable. Code, code-insiders and codium retain their exact executable and settings/extension locations. Toggle keys retain upstream order. Each editor is independently optional.

Settings files are parsed as JSONC for one structural root-property edit. Comments, trailing commas, spacing, nested objects, and unrelated fields survive. A key mentioned only in a comment or nested object does not count as the active root setting. `set` replaces the last duplicate active root key or inserts one; `remove` removes all active root duplicates. Descriptor names must be JSON strings; a present value of another type is reported and routed as empty. Valid names containing quotes, backslashes, tabs, or newlines are preserved and JSON-escaped in settings. This intentionally accepts names the Bash regex discarded. Malformed JSONC is reported and left byte-for-byte unchanged. Each mutation snapshots the file and performs compare-and-swap under a stable lock, follows an existing settings symlink to its target, and preserves permissions. The lock sidecar remains on disk by design.

Generated package bytes, theme-link target, registry entry structure/order and obsolete cleanup retain the upstream shape. Registry transformations consume the exact content returned by their preceding snapshot and write against that same hash, so a concurrent change is reported as a CAS conflict instead of being overwritten with stale transformed data. Empty regular `.obsolete` files are removed by a one-file compare-and-swap transaction against their original snapshot hash. Successful transactions retain a checksummed recovery journal under `$HOME/.local/state/execute/transactions/` in a unique private invocation directory; tests verify the committed journal data independently of the removed target. These journals are additional state artifacts and there is no automatic retention policy yet. An empty `.obsolete` symlink is removed with `fs.unlink_checked`, which compares the captured snapshot and unlinks the directory entry without following its target. A nonempty symlink uses `fs.write_atomic` with explicit `replace` policy, matching the upstream temporary-file rename while preserving the referent. Package and registry file updates use atomic writes. Generated `.type` is read as JSON and exactly one final line feed is removed before comparison, so a string value of `light\n` selects `vs`; other types and malformed type JSON route to the documented dark fallback with a warning. Extension IDs preserve the source's permissive regex (`--flag` is still accepted), but grep uses an option terminator so leading dashes are treated as data.

## Isolation and verification

Run from the repository root:

```sh
make omarchy-rewrite-check
```

Or run only this acceptance module:

```sh
EXECUTE_TEST_BINARY=/path/to/current/execute python3 -m unittest discover -s tests/omarchy_rewrites -p test_10_vscode_theme.py -v
```

Tests create separate owned temporary HOMEs and private PATHs for both implementations. Only real jq/core utilities and owned fake editor/toggle/presence commands can resolve. Mock extension installs write one marker inside fake HOME. No actual editor binary, user settings, extension installation, or host Cursor executable is used. Cursor's generated branch needs no external executable. Tests cover source integrity, ignored arguments, escaped JSON theme names, JSONC comments/nesting/duplicate keys/insertion/removal/malformed input, settings symlink following, obsolete symlink unlink/replacement without referent mutation, checksummed committed transaction journals, a deterministic registry compare-and-swap conflict, package registration, light-theme normalization, and visible install/list failures.

Set `OMARCHY_REWRITE_EVIDENCE_DIR` to save per-case observations under its
`10-theme-set-vscode/` subdirectory. The default test run writes no evidence
outside its temporary HOME fixtures.
