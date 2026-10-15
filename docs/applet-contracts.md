# Applet discovery

Ask the runtime for the validated callable contracts in its manifest catalog:

```sh
execute applets list
execute applets inspect printf
execute applets inspect "count lines"
execute applets list --manifest-dir ./my-applets
execute applets inspect my.applet --manifest-dir ./my-applet.json
```

Both commands print JSON. `list` uses protocol `execute.applets/1` and an
`applets` array sorted by canonical name. `inspect` uses `execute.applet/1` and
an `applet` object. Canonical names and phrases resolve to the same object.
`validation: "manifest_static"` means the complete catalog passed the runtime's
ordinary manifest validation. Invalid catalogs, unknown names and malformed
options fail with a diagnostic on stderr and no JSON on stdout. Use
`execute applets --help` for command syntax, or `--` before a name beginning
with a hyphen. Options must precede that delimiter.

Discovery uses the same catalog selection as program execution: an explicit
`--manifest-dir` file or directory, otherwise the local or installed bundle.
It reads manifests without locating, launching, or probing their binaries.
An explicit missing path is an error. A missing optional default catalog can
produce an empty list; direct quoted executable calls still work separately.

Each applet object contains:

| Field | Meaning |
| --- | --- |
| `name`, `phrases` | Canonical name and accepted phrase aliases. Aliases share one backend contract. |
| `backend`, `binary`, `builtin` | Native process or runtime builtin, exact executable token, and builtin identity when present. |
| `parameters_schema` | JSON Schema 2020-12 for resolved named parameters: string, signed 64-bit integer, boolean, or homogeneous scalar list; required names, minimum list lengths and rejection of unknown parameters. Strings exclude NUL because argv cannot contain it. |
| `argv_template` | Ordered, tagged `literal`, `parameter`, `spread`, and `when_present` nodes from the validated manifest. Each scalar becomes one argv element; spread emits one per list item; conditional append tests parameter presence, including a supplied `false` or `0`. There is no shell expansion. |
| `io` | Whether piped byte input is accepted, byte stdout or an empty builtin result, and `output_schema: null` because semantic output types are undeclared. |
| `execution` | Availability assessment, shell expansion policy, cache declaration, uncertain-failure declaration, effect category and sandbox boundary. |

For example, `execute applets inspect printf` describes the shipped `message`
string parameter and the two argv nodes: literal `%s\n`, then parameter
`message`. It records `stdin: "not_accepted"`, `stdout: "bytes"`, native
availability `"not_checked"`, and sandbox `"not_enforced"`. `cd` instead records
backend `"builtin"`, availability `"runtime_builtin"`, effect
`"working_directory"`, and empty stdout. Its `binary: "cd"` is a builtin
identifier; the runtime does not launch a `cd` executable.

The schema describes resolved values, before argv formatting. Program references
still require the language's ordinary scope and type validation. Integers must
be representable as signed 64-bit values; encodings that decode as other JSON
number types do not implicitly become integer inputs. The contract is a
projection of validated runtime data, separate from checkpoint serialization.
Changing a manifest is reflected on the next discovery invocation.

Native `availability: "not_checked"` is deliberate: an executable can be missing,
resolve differently through PATH or cwd, need credentials, or reject the mapped
arguments. A cacheable declaration does not prove purity. The uncertainty marker
records recovery policy for failures, not exactly-once effects. Bounds, timeout,
failure and stream framing follow their existing runtime contracts.

These protocols describe the native manifest catalog and its builtin. They do
not enumerate every installed executable, discover IPC/QML/remote APIs, assert
output schemas, negotiate backend versions, or grant permissions. Native applets
retain caller privileges; per-applet sandbox enforcement is absent. A Flatpak
installation has its separate application-wide sandbox contract. This increment
adds callable discovery without establishing universal applet coverage.
