# Applet contracts

Versioned applet contracts describe the values, outcomes, streams, execution
backend and declared effects at a named applet boundary. They add a checked
interface to the existing manifest call syntax; they do not generate wrapper
source or make an applet safe by themselves. These are current-source features
and require a source build. Published 0.9.0-rc.4 and the retained 0.8.0 binary
do not include them.

## Inspect the catalog

```sh
execute applets list
execute applets inspect printf
execute applets inspect "count lines"
execute applets list --manifest-dir ./my-applets
execute applets list --manifest-catalog ./my-applets.catalog
```

`--manifest-dir` retains its directory or single-manifest JSON file behavior.
`--manifest-catalog` explicitly selects one packed catalog file; the selectors
are mutually exclusive. No catalog is merged or selected automatically.

The default manifest directory is the `manifests` folder beside the installed
binary, or the system location when there is none. Its bundled applets are
served from an index compiled into the binary, so a run does not read or
re-validate each file. The index is used only when the directory holds exactly
the indexed files, matched by file name and byte size. Any other set falls back
to the full load, so a changed or broken file reports the same error as before.
The check does not read file contents, so an edit that keeps a file's byte size
is not detected. Editing a system-wide directory needs root in any case. An
explicit `--manifest-dir` or `--manifest-catalog` always gets full validation.

`list` returns protocol `execute.applets/1` with applets sorted by canonical
name. `inspect` returns `execute.applet/1`; canonical names and phrases resolve
to the same validated manifest. Discovery validates manifest data without
locating or launching executables. Native availability is deliberately reported
as not checked: a binary may be missing, resolve differently from another cwd
or PATH, require credentials, or reject its argv. Direct quoted commands remain
available separately from catalog discovery.

Each inspected applet includes its canonical name and aliases, resolved
parameter schema, ordered argv template, stdin/stdout contract and execution
metadata. A `cacheable` declaration does not prove purity, and an
`uncertain_on_failure` declaration guides recovery without guaranteeing exactly
once effects. Use this view to inspect the declared interface, then use the
versioned fields below for typed values and backend behavior.

Two other declarations have runtime consequences. For an in-process applet, the
declared `effects` decide checkpoint [retry safety](language.md#retry-safety): reads or
no effects are retry-safe, anything else is uncertain after a crash. A manifest may also
declare `build_feature` (`remote` or `desktop`); in a lean build its inspected contract
shows `requires_full_build` and the applet refuses to run with a message that says to use
the full build.

## Alternative parameters

A manifest may declare `parameter_alternatives` when an invocation accepts
several combinations of inputs. Each rule has a single-line `message` and an
`any_of` list of branches; each branch is a nonempty list of predicates that
must all match. For example:

```json
{
  "parameter_alternatives": [{
  "message": "choose a literal or a file",
  "any_of": [
    [{"test": "present", "parameter": "literal"},
     {"test": "absent", "parameter": "file"}],
    [{"test": "absent", "parameter": "literal"},
     {"test": "nonempty", "parameter": "file"}]
  ]
  }]
}
```

Predicates are `present`, `absent`, `nonempty` (string or path),
`negative_integer`, `boolean` (with a `value`), and `number_range` (with
inclusive `minimum` and `maximum` integer bounds). Numeric bounds must fit
within ±2^53 so integer-to-binary64 conversion is exact. A manifest is refused
if a rule names an undeclared parameter, has the wrong parameter type, or has
empty or oversized branches. Checking rejects a call only when every branch is
known to fail; values supplied by references are checked after resolution before
execution. Inspection exposes the rules and their JSON Schema equivalent.
Helper invocations bypass manifests, so applet implementations must continue
to validate their own inputs.

## A manifest with typed output

A manifest can keep the ordinary named-applet parameter and argv mapping while
declaring its output schema. This `text.matches` adapter uses the bundled
`execute-applet` command and sends the declared parameters as one compact JSON
argument:

```json
{
  "name": "text.matches",
  "binary": "execute-applet",
  "parameters": {
    "text": { "type": "string", "required": true },
    "needle": { "type": "string", "required": true }
  },
  "arguments": [
    { "literal": "text.matches" },
    { "literal": "--args" },
    { "json_arguments": true }
  ],
  "contract": {
    "version": 1,
    "backend": "command",
    "output": {
      "type": "record",
      "encoding": "json",
      "fields": {
        "matches": { "type": "boolean" },
        "count": { "type": "integer" },
        "byte_offsets": { "type": "json" }
      },
      "required": ["matches", "count", "byte_offsets"]
    }
  }
}
```

Set `backend` to `in_process` and `binary` to `@execute-applet` to run one of
the bundled generic operations inside the interpreter. For a persistent helper,
use `backend: "json_lines_worker"`, `binary: "execute-applet"`, and
`lifecycle: {"kind":"json_lines"}`. The same `.program` invocation works for
all three backends:

```text
accept text as text
accept needle as text
run text.matches with text text and needle needle as result
take boolean at "/matches" from result as found
if found is true
  run "printf" with arguments ["found\n"]
otherwise
  run "printf" with arguments ["not found\n"]
```

The output schema is checked when the applet result is bound to `result`.
Selection then supplies an ordinary typed boolean to the program's condition.
Without `as result`, the applet's raw output bytes remain the pipeline value;
typed decoding does not rewrite stdout or a later native pipe. JSON- and
record-valued parameters use the explicit `json_parameter` mapping for one
parameter or `json_arguments` to encode the entire argument map. They cannot
be passed through ordinary scalar `parameter` or `spread` nodes.

Native pipes carry the producer's stdout bytes directly. Preflight checks both
the declared value schemas and wire encodings: raw text cannot feed JSON stdin,
while JSON-encoded text can feed a JSON stdin port. Semantic widening such as
Number to JSON applies to typed values and does not rewrite raw pipe bytes.
Output normalization such as `trim_final_lf` also runs only during decoding.
Bind that value before passing it as a typed parameter; direct pipes into typed
stdin reject this normalization mismatch. Bytes stdin still accepts the raw bytes.

## Input and output types

Legacy manifests that omit `contract` remain command applets with one-shot
lifecycle and raw output. A v1 contract defaults to `backend: "command"` and
the one-shot lifecycle. Unknown contract keys and unsupported versions are
errors during catalog validation.

| Contract type | Wire form and validation |
| --- | --- |
| `bytes` | Raw bytes only. It has no typed binding. |
| `text` | UTF-8 text, either raw bytes or a JSON string. Empty text is valid. |
| `path` | A contract-only semantic string. Raw form must be UTF-8; JSON form must be a JSON string. It must be nonempty and contain no NUL. A path parameter accepts a string literal or text result reference and emits exactly one argv item. Path output uses the existing text binding type. A leading `~/` (or `~` alone) in a path parameter's value expands to the home directory before the applet sees it, for command applets and in-process `fs.*` applets alike; `~user` is not expanded. Argv text that is not a path parameter, and every string parameter, stay literal. |
| `integer` | JSON signed 64-bit integer. |
| `number` | Any finite JSON number, including integers and fractional values. |
| `boolean` | JSON boolean. |
| `json` | Any complete JSON value. |
| `record` | JSON object checked against recursively declared fields and required names. Undeclared fields and byte fields are rejected. A field of type `json` accepts any JSON value, including arrays and nested objects. |
| `list` | JSON array checked item by item against its recursive `items` schema. Lists cannot contain raw-byte values; a top-level list output uses JSON encoding. |

An output schema requires an explicit `encoding`. Text and path accept `raw` or
`json`; integer, number, boolean, json, record and list require `json`; bytes
require `raw`.
`trim_final_lf: true` is permitted only for raw text or path, and removes one
final LF only. It does not trim spaces, remove CR, or normalize line endings.
There is no nominal Path value in the `.program` AST: path semantics are checked
at the manifest boundary, while program bindings remain text.

Recursive `ValueSchema` values support scalars, records and lists. Records may
declare `fields` and `required`; lists declare `items`. Those structural fields
are rejected on other schema kinds. `nullable: true` explicitly permits JSON
`null` in addition to the declared type, including for nested record fields and
list items. Bytes cannot be nullable or embedded in a JSON record/list.
For typed connections, every field a producer may emit must be accepted by
the consumer, and every consumer-required field must be guaranteed by the
producer. A consumer may declare additional optional fields.

Text schemas may restrict values with a nonempty `enum` array:

```json
{"type":"text","enum":["json","yaml"]}
```

An enum validates literals and runtime values at the typed boundary. For a
port connection, every value allowed by the producer must be accepted by the
consumer: an enum containing only `json` can feed one containing `json` and
`yaml`, while unconstrained text or an enum containing `xml` cannot. Graph
preflight uses a declared allowed value when it validates a parameter binding.

## Named applet ports

The optional `contract.ports` object describes typed `inputs`, `outputs` and
`events` by identifier. An input port maps to a manifest parameter of the same
name. The special `stdin` input is available only when the manifest accepts
stdin and must declare an encoding. Bytes use `raw`; text and path accept `raw`
or `json`; integer, number, boolean, JSON, record and list values use `json`. Ordinary
named parameters are already typed values and do not take a per-port encoding.
When a parameter is declared as generic JSON but its input port is `number`, the
contract refines it to a finite JSON number and rejects other JSON values during
canonical invocation validation.

An output port is selected from the contract's canonical decoded output. Its
optional `pointer` is an RFC 6901 JSON Pointer; multiple fields from one record
must use explicit pointers. If `result_envelope` is enabled, pointers address
the envelope fields, including `/value`, `/outcome`, `/kind`,
`/effect_certainty` and `/status`; the
declared success value becomes nullable because non-success envelopes carry
`null`. Output ports use the top-level output encoding and cannot override it.
`delivery` describes `single`, `records` or `events` delivery. A `records`
output can select a list and declare the item's schema as the port schema.
Event namespace entries must use `events` delivery and carry payload metadata;
they do not add a local stdout transport. `sensitivity` classifies a port as
`public`, `internal`, `confidential`, `secret` or `unknown` (the default).
Unknown is retained as a distinct conservative label for graph/security policy
decisions; the label alone does not sanitize a payload or create an OS sandbox.

For example, a helper that accepts one path parameter and returns its contents
could declare:

```json
{
  "contract": {
    "version": 1,
    "output": { "type": "record", "encoding": "json", "fields": {
      "content": { "type": "text" },
      "snapshot": { "type": "text" }
    }, "required": ["content", "snapshot"] },
    "ports": {
      "inputs": {
        "path": { "schema": { "type": "path" }, "delivery": "single", "sensitivity": "internal" }
      },
      "outputs": {
        "content": { "schema": { "type": "text" }, "delivery": "single", "pointer": "/content", "sensitivity": "confidential" },
        "snapshot": { "schema": { "type": "text" }, "delivery": "single", "pointer": "/snapshot", "sensitivity": "internal" }
      },
      "events": {
        "changed": { "schema": { "type": "json" }, "delivery": "events", "sensitivity": "unknown" }
      }
    }
  }
}
```

Remote applet descriptors use these ports to validate request values, outputs
and event payloads. Port declarations do not change the call syntax or raw
pipeline bytes of a local command applet.

## Per-applet environment

An optional `contract.environment` declares environment changes for command and
JSON-lines-worker applets:

```json
"environment": {
  "set": { "LC_ALL": "C" },
  "unset": ["LANG"]
}
```

Names must be portable environment identifiers, values cannot contain NUL, and
the same name cannot appear in both `set` and `unset`. At launch, removals are
applied to the inherited environment first, then explicit values override it.
A worker receives the resolved environment when it starts, and that snapshot is
part of its reuse identity. With a capability policy, each name in `set` must
also be in that applet grant's environment allowlist; an `unset` name may be
removed without being granted. Other backends reject this field until they
support its semantics. Discovery and Debug output reveal the set/unset names,
not the values; the values remain part of manifest/runtime identity. Keep
credentials in an explicit secret source rather than embedding them in a
manifest.

## Outcomes and explicit routing

With no `outcomes` table, status zero is `success` and every other status is a
failure. A contract can name statuses as `success`, `absence`, `failure` or
`retryable`. Statuses and names must be unique. Only a `success` outcome is
implicitly accepted. An absence or error does not silently become an empty value,
false condition or skipped result.

Each declared status may also set `effect_certainty` to `not_started`,
`known_response` or `uncertain`. If omitted, success and absence default to
`known_response`, while failure and retryable default to `uncertain`. This
describes whether the host knows that an applet effect started or completed; it
does not make a retry safe. Signals, spawn failures, timeouts, cancellation and
transport failures do not receive fabricated applet outcome classifications.

`and keep status as name` explicitly captures the native-style status for a
single applet call outside a native pipe group. A captured non-success can then
continue into program control flow. Use `result_envelope: true` when the typed
success value and its classified outcome should travel together:

```json
{
  "name": "file.snapshot",
  "binary": "execute-applet",
  "parameters": { "path": { "type": "path", "required": true } },
  "arguments": [
    { "literal": "fs.read" },
    { "literal": "--args" },
    { "json_arguments": true }
  ],
  "contract": {
    "version": 1,
    "backend": "command",
    "output": { "type": "json", "encoding": "json" },
    "outcomes": [
      { "status": 0, "name": "present", "kind": "success", "effect_certainty": "known_response" },
      { "status": 1, "name": "absence", "kind": "absence", "effect_certainty": "known_response" },
      { "status": 2, "name": "invalid", "kind": "failure", "effect_certainty": "uncertain" },
      { "status": 75, "name": "retryable", "kind": "retryable", "effect_certainty": "uncertain" }
    ],
    "result_envelope": true
  }
}
```

```text
accept path as text
run file.snapshot with path path as reply and keep status as code
take text at "/outcome" from reply as outcome
take text at "/effect_certainty" from reply as certainty
take integer at "/status" from reply as reported_status
if certainty is "uncertain"
  run "printf" with arguments ["inspect before retry (status %s)\n", reported_status]
otherwise
  if outcome is "absence"
    run "printf" with arguments ["missing (status %s)\n", reported_status]
  otherwise
    take text at "/value/content" from reply as contents
    run "printf" with arguments ["%s", contents]
```

The envelope has `{value, outcome, kind, effect_certainty, status}`. On success,
`value` is decoded by the declared output schema. For a declared non-success,
`value` is null and the other fields preserve the classified outcome, effect
certainty and actual status. The envelope's canonical value schema is exposed
as `execution.result_envelope_schema` in discovery and by
`AppletContract::result_envelope_schema()` in Rust. On an outcomes table,
certainty uses the per-status declaration or the defaults above; without a
table, status-zero success is `known_response` and other statuses are
`uncertain`.
An envelope does not itself authorize continuation: capture status explicitly
as shown. Unlisted nonzero statuses remain failures. Signals, spawn failures,
timeouts, cancellation and transport errors remain runtime errors rather than
fabricated applet outcomes. Status-bound calls cannot be cached or placed in a
native pipe group.

Legacy cache entries continue to contain the original raw stdout bytes. A v1
contract cache entry preserves the native status alongside raw stdout, so a
cache hit is classified and decoded through the same outcome and type checks as
a fresh call. The typed result envelope therefore keeps the actual recorded
status. Non-success results are not cached: they fail unless status is
explicitly captured, and status-bound calls are excluded from caching.

## Execution backends and lifetimes

| Backend | Behavior and limits |
| --- | --- |
| `command` | Launches the manifest executable once per invocation with literal argv. It uses the native process supervisor and can join native `pipe to` groups. |
| `in_process` | Calls a bundled generic operation in the interpreter process. It does not launch an applet process and cannot join a native pipe group. Selected bounded CPU loops cooperatively check workflow cancellation, the per-call `--command-timeout-ms` budget and the parent total deadline. A synchronous OS call or library routine without checkpoints can still return after the budget; an interrupted started applet has uncertain effect certainty. Since the operation runs with the host interpreter, it is not eligible for the OS command sandbox. |
| `json_lines_worker` | Sends framed requests to a persistent helper process. It cannot join a native pipe group or use the applet cache. The worker is shared by registry clones, serializes requests per worker, and is reaped when its registry pool is dropped. Sensitivity labels are retained with each live worker and included in the preflight floor of workflows that invoke that applet; a concurrent request rechecks the retained labels after acquiring the worker slot. The history is discarded when the worker process is destroyed. The pool defaults to 16 identities and can be configured from 1 through 1024; a full pool rejects a new identity without spawning or evicting a healthy worker. This backend is supported on Unix and native Windows x64/ARM64. Windows workers join an owned Job Object before running and use the same frame, timeout and cancellation limits. |
| `program_module` | Interface 1 executes a prevalidated finite export in a private scope. Interface 2 declares a scheduled or native-listener `.program` service whose lifetime belongs to a graph node and whose handler emits one typed event. Both use preloaded local source, package dependencies, the caller's runtime and normal supervision; details are in the [program module guide](program-modules.md). |
| `remote` | Calls a host-managed endpoint using the typed named ports. The remote request is not replayed after an uncertain transport failure; origins, credentials, descriptor pins, event framing and limits are in the [remote applet guide](../examples/remote/README.md). |

`lifecycle: {"kind":"program_service"}` is valid only for
`backend: "program_module"` with `program.interface_version: 2`. Interface 1
continues to pair with `oneshot`; other backends keep their existing lifecycle
contracts. Interface 2 is intentionally explicit so an older runtime rejects
it instead of treating a persistent source as a finite entry. A service applet
cannot be called as an ordinary finite result-producing applet: in a graph,
the runtime starts it as a persistent node and binds its declared typed startup
inputs once. It exposes one event port and no stdout output ports. Each
completed handler's full decoded result becomes one event, with bounded graph
queue and cancellation behavior described in the [graph guide](graph.md).

The worker protocol uses one newline-delimited JSON object per request:

```json
{"id":1,"arguments":["text.matches","--args","{\"needle\":\"TODO\",\"text\":\"TODO\"}"],"input":[]}
```

The response is `{"id":1,"status":0,"output":[123, ...]}` with optional
`stderr`. `output` and `input` are JSON arrays of byte values, not base64.
Responses must match the outstanding request id. A malformed, truncated,
oversized or mismatched frame, worker crash, timeout or cancellation kills and
reaps that worker; Execute does not retry a transmitted request because its
effects may be uncertain. The next explicit call may start another worker.
Default lifecycle limits are a 30-second worker timeout, 8 MiB request/response
frame and 64 KiB retained stderr; maxima are 300 seconds, 64 MiB and 1 MiB.
The registry pool defaults to 16 worker identities and accepts a configured
capacity from 1 through 1024. It keys reuse on the canonical applet name,
executable path and content digest, working directory, full environment,
startup arguments and capability-policy fingerprint. At capacity, a new
identity is rejected without spawning a process or evicting a healthy worker.

The `command`, `in_process` and `json_lines_worker` backends use the same applet
call syntax and parameter validation. Program modules and remote applets also
resolve as named applets but use their package or port descriptors. A native
pipeline uses direct OS pipes, so preflight rejects a buffered in-process,
worker, module or remote applet anywhere in that pipeline before earlier
effects can run. Worker framing is separate from the native `stdio` contract.
The in-process call budget begins at applet dispatch and uses the earlier of
`--command-timeout-ms` and the absolute parent total deadline. Input capture
from stdin uses the remaining call budget; the same deadline is passed to
cooperative CPU checkpoints.

An optional manifest-level `stdio` object sets `stdout` and/or `stderr` to
`capture`, `discard` or `inherit`. Omitted streams keep the host's previous
defaults: terminal stdout is captured, while stderr is captured only when the
runtime is collecting diagnostics and otherwise inherited. Discard attaches
the OS null device, so discarded output is not buffered by Rust and does not
count against the interpreter's output-capture limit. Intermediate stdout in a
native pipe is always sent directly to the next process; only the final stage
uses its declared stdout mode. A non-captured final stdout cannot supply a
result binding, save, downstream consumer or stream listener.

These contracts do not make multi-applet or multi-file updates transactional.
The generic filesystem helpers atomically replace one file at a time. Native
command timeouts are not deadlines for arbitrary filesystem calls, and
cooperative in-process checks do not interrupt a synchronous filesystem call,
JSON parser/serializer routine, or other library operation between checkpoints.
The interpreter's output limits are not universal bounds on a command's reads,
writes, memory, filesystem effects or data sent through inherited descriptors.

## Bundled generic operations

`execute-applet` supplies generic transformations. Its one-shot command form is
`execute-applet OPERATION --args JSON_OBJECT`; it reads optional bytes from
stdin and writes one compact JSON value plus LF. The same implementation is
available to `in_process` manifests. The distributable helper is installed next
to `execute`; build both from the same source revision. `@execute-applet` is a
reserved in-process marker, while command and worker manifests name the actual
helper executable.

| Operation | Behavior |
| --- | --- |
| `path.dirname`, `path.basename`, `path.remove_suffix` | Small path-string transformations; they do not access the filesystem. |
| `text.trim_final_lf`, `text.normalize` | Return supplied `args.text` or UTF-8 stdin with one final LF removed. `text.normalize` is currently the same operation; neither trims other whitespace nor normalizes line endings. |

| `text.concat`, `text.matches` | Join a string list with an optional separator; or return match presence, count and UTF-8 byte offsets for a literal needle. |
| `number.add`, `number.subtract`, `number.multiply`, `number.divide`, `number.compare` | Operate on finite JSON numbers and declare Number input/output ports (compare returns an integer). Integral add/subtract/multiply use checked integer arithmetic; division and fractional arithmetic use IEEE-754 binary64, so decimal results are not guaranteed to be exact. Division by zero fails; compare returns -1, 0 or 1. |
| `number.to_text` | Accepts a Number and returns its deterministic, locale-independent JSON numeric representation as text. It does not coerce strings or promise exact-decimal formatting. |
| `number.parse_integer` | Parse UTF-8 decimal or explicitly based input in radix 2 through 36, with optional ASCII-whitespace trimming and signed 64-bit range checks. |
| `color.rgb_to_hex`, `color.parse_rgb` | Convert three channels from 0 through 255 into `#rrggbb`, or parse a strict RGB triple with an explicit fallback color. |
| `json.parse`, `json.stringify` | Parse strict UTF-8 JSON (up to 16 MiB and 128 nesting levels) or emit compact escaped JSON. Malformed or oversized input fails with status 2. |
| `csv.parse`, `csv.stringify` | Parse UTF-8 RFC 4180-style CSV into a typed `{headers, rows}` string table or emit its deterministic CRLF form. Headers are optional; duplicate headers, ragged rows, malformed quoting, invalid UTF-8 and non-string cells fail. These are bounded finite transforms, not streaming operations. |
| `json.get`, `json.set` | Select or replace a value using RFC 6901 JSON Pointer. `get` reports a missing target with status 1; `set` can add an object member, replace an array item or append at the array end, but does not create missing parents. |
| `json.map`, `json.filter` | Project one JSON Pointer from each selected array item, or keep items matching up to 32 ANDed predicates, including `exists`, `equals`, `not_equals` and `type`. A missing projection or selected target reports absence; malformed pointers, predicates and wrong value types fail validation. |
| `json.has`, `json.delete` | Test a JSON Pointer or remove its target. `json.has` defaults an omitted path to the root. `json.delete` requires a path and refuses the root. |
| `json.sort_by`, `json.unique_by`, `json.group_by` | Sort, deduplicate, or group array items by a JSON Pointer key. An omitted `pointer` defaults to the empty pointer, selecting each item itself; explicit non-string or malformed pointers fail. |
| `fs.read`, `fs.snapshot` | Read a regular UTF-8 file up to 16 MiB and return `{content,content_hash,snapshot,symlink,mode}`. The content hash is SHA-256 of file bytes; the snapshot also binds path, link/target identity and metadata. Missing files have the explicit absence outcome. |
| `fs.write_atomic` | Require the expected SHA-256 or `"missing"`, then atomically replace one file. It requires explicit `preserve`, `reject` or `replace` symlink policy and `preserve`, `private` or `exact` permission policy; an optional stable advisory lock path has a 30-second default and 300-second maximum timeout. |
| `fs.transaction` | Apply a bounded journaled batch of expected-hash writes or removals to existing regular files. It is a command applet with explicit recovery, not a multi-file atomic-visibility guarantee. |
| `fs.transaction.recover` | Complete or roll back a retained transaction journal if every target still matches its recorded before or after state, including a requested absent state. A third state is a manual-recovery conflict. |
| `jsonc.set`, `jsonc.remove` | Apply a root-property update to JSONC with line/block comments and trailing commas, with at most 256 levels of nesting. `set` replaces the last active duplicate or inserts the key; `remove` removes every active duplicate. Keys in comments and nested objects are ignored. Both return a typed receipt with path, content hash, snapshot and whether content changed. Malformed input fails before mutation. |
| `jsonc.edit_content` | Apply the same bounded root-property set/remove logic to supplied JSONC text or UTF-8 stdin and return updated text. It is a pure transformation and never writes a file. |

JSON applets whose manifests do not accept stdin require `document` in workflows.
The in-process applet API can still read a document from its input bytes when
`document` is omitted. Stdin-capable manifests accept a piped document.

Every text parser strips one leading UTF-8 BOM. Duplicate JSON object keys are
refused everywhere. `json.filter` compares numeric values by value (so `1`,
`1.0` and `1e0` match). For JSON Pointer array selection, a nonnumeric index is
an absent value. YAML and TOML stringification refuses nesting beyond the
parser's depth limit.

`fs.ensure_line` matches lines after ignoring one leading BOM and preserves that
BOM in the file. When appending, it uses CRLF if the file's first line ending is
CRLF. `path.dirname` and `path.basename` use POSIX-style slash-separated
paths on every host. For an empty path, `path.dirname` returns `.` and
`path.basename` returns an empty string, regardless of its optional suffix.
Slash-only paths return `/` for both applets, including repeated slashes.

The filesystem write helper locks one path, checks the expected content,
writes a same-directory temporary file, syncs the file and (on Unix) its parent,
rechecks the snapshot, then renames. Lock files are deliberately retained so
participants keep locking the same inode. Advisory locking coordinates only
processes that use the same lock. The path recheck and rename are not an atomic
compare-and-swap against arbitrary writers; keep parent directories trusted.
On Unix, `preserve` copies permission mode bits, not ownership, ACLs, extended
attributes or timestamps. On non-Unix platforms, `private` and `exact` are
rejected and `preserve` uses platform inheritance. Select symlink and
permission policy explicitly.

`reject` refuses any final symlink. `replace` replaces the final symlink itself,
including a dangling link (which must have expected content `"missing"`).
`preserve` follows the final link only after canonical resolution, a no-follow
open and identity recheck. The read helpers return UTF-8 text; binary files and
text exceeding the 16 MiB limit fail rather than being truncated.

`fs.transaction` and `fs.transaction.recover` are version-1 command applet
contracts exposed through `execute-applet`; they are not direct host-language
filesystem primitives. A transaction accepts a required `journal`
directory and `operations` with `{path, expected_content_hash, action, content?}`.
`action` is `write` or `remove`; writes require string `content` and removals
forbid it. The journal directory must not exist, its parent must exist, and the
private journal is retained as a recovery/audit record. Targets must be
distinct existing regular files; symlinks are rejected. The default target-lock
timeout is 30 seconds (maximum 300 seconds). A batch is limited to 32 targets
and 16 MiB of combined before-and-after contents, with each file also capped
at 16 MiB; serialized journal metadata is separately limited to 1 MiB.

The success receipt contains status, journal and entries. Each entry contains
path, before_hash, after_hash, before_mode, after_exists and state. Every
target existed at staging. For a write, after_exists is true and after_hash
hashes the replacement bytes. For a removal, after_exists is false and
after_hash is the empty-byte SHA-256, with the boolean distinguishing absence
from an empty file. before_mode is the original Unix permission bits or null
on non-Unix systems. Rolling back a removal restores those mode bits on Unix,
but ownership, ACLs, extended attributes and timestamps are not preserved;
the recreated file belongs to the current user.

The journal retains before_mode and applies the captured permission bits to
replacement writes and recovery where the platform supports it. It does not
preserve ownership or ACLs; Windows restoration follows the helper's platform
behavior.
Recovery takes that journal and `decision:"complete"` or `"rollback"`, then
returns `status:"committed"` or `"rolled_back"` with the journal and entries.
An expected-hash mismatch or other pre-staging conflict fails with `conflict`
before file mutation. Failures are nonzero with stable codes
`transaction_rolled_back` (compensation completed), `manual_recovery` (a target
is in a third state or a required target is missing during restoration) or
`uncertain_effect`; diagnostics identify the journal to retain and recover.
On Windows, the prepared journal is published before its journal and target
locks are acquired. Apply verifies the same prepared intent under the journal
lock and rechecks target contents under the target locks before writing.
A conflict or lock failure in this phase can leave a `prepared` journal;
the diagnostic names it for inspection and explicit recovery. Retrying with
the same journal path requires settling that retained journal first.
The transaction locks targets in sorted order, records checksummed private
before/after state and durable intent/progress, then atomically replaces files
one by one. On an error it attempts reverse compensation using hash checks.
Recovery proceeds only while each target still matches its recorded before or
after state, including absence for a removal; a third-state change requires
manual resolution. Other processes
can observe a partially applied batch, and a crash can leave that state for
recovery. The journal coordinates only these file updates: it does not include
external commands or their effects. It is not a transaction for arbitrary
filesystem writers, and the journal directory must remain available until
recovery is no longer needed. The journal contains before-and-after recovery
data for affected files, so protect it like the files it records. Its locks are
advisory and coordinate only participants that use the same lock.
There is no transaction cleanup applet; keep the journal until its recovery and
audit record is no longer needed, then remove it deliberately.

Native command timeouts are not deadlines for arbitrary filesystem calls, and
the interpreter's output limits are not universal bounds on a command's reads,
writes, memory, filesystem effects or data sent through inherited descriptors.

The applet catalog and failure codes are source-defined contracts, not an
application policy layer. Workflow code chooses which generic operation to call,
the order of calls, and whether to handle an absence or retryable result.

## Capability metadata and operating-system sandboxing

`capabilities` records declared permission identifiers; `effects` records the
effects an applet says it may perform, and each effect must also appear in
`capabilities`. Identifiers are exact-match namespaced strings so a host can
extend the vocabulary. Declarations alone do not constrain an applet. Without
an explicit policy, native commands retain the caller's environment and
privileges.

When a policy is present, it is a strict version-1 allowlist: each invoked
applet must have a canonical grant and a v1 contract, and direct quoted commands
are denied. Only listed environment names are passed through; a binary SHA-256
pin is optional. `sandbox_required: false` checks declarations and host file
paths but does not isolate the child process. `sandbox_required: true` opts in
to platform OS enforcement for command and worker processes; an unsupported or
unavailable backend fails before workflow effects. In-process operations cannot
use that OS sandbox. See [security boundaries](../SECURITY.md#applet-contracts-and-optional-capability-policy)
for path rules, platform details and the limits of each mechanism.

### Privileged steps: `deny_privileged` (best effort)

`execute explain` marks a step as privileged when its executable is a known
privilege elevator (`sudo`, `sudo-rs`, `pkexec`, `doas`, `run0`, `su`, `runuser`,
`systemd-run`, `machinectl`, `gsudo`, `please`, `runas`, `sg`, `newgrp`). It also
marks a command launcher or shell whose arguments can name another program
(`env`, `nice`, `nohup`, `timeout`, `stdbuf`, `setsid`, `xargs`, `busybox`, `flock`,
`chroot`, `unshare`, `nsenter`, `ionice`, `chrt`, `taskset`, `time`, `watch`,
`script`, `start-stop-daemon`, `setpriv`, and the shells `sh`, `bash`, `dash`, `zsh`,
`fish`, `ksh`, `mksh`, `ash`, `csh`, `tcsh`, `yash`, `posh`, `cmd`, `powershell`,
`pwsh`). Names match after the directory and any `.exe` suffix are removed, in any
case. The executable that would actually run is checked too, by its canonical path,
so a link to an elevator is still an elevator.

A capability policy with `"deny_privileged": true` refuses those steps before any
process starts. Elevators are always refused. Launchers and shells are refused
whatever their arguments, because their arguments can name a program the program
text never mentions; a launcher can be permitted by name with
`"allow_wrappers": ["env"]`. The check runs in the static preflight and again at
every command launch, so finite runs, resumed runs, task groups, parallel loops,
spawned jobs, command conditions and called programs are all covered.

This is a best-effort deny-list of known elevators and launchers. It recognises
names, not behavior: a renamed or unlisted tool is not recognised. Fail-closed
enforcement is the strict capability policy itself, an explicit allowlist of
granted applets and executables. A policy without a grant for an applet refuses
it, and direct quoted commands are refused outright. An applet that is granted
still runs whatever binary its manifest names, so grant only manifests whose
binaries you have reviewed. Do not rely on `deny_privileged` as the only control.

## File tests, globs and line lists

These in-process operations answer `if` conditions and feed `for each` without
a helper process. Each one is a `core.*` manifest and needs no grammar.

```text
accept target as text
run fs.test with path target and kind "directory" as is_dir
if is_dir is true
  run "printf" with arguments ["%s is a directory\n", target]
```

`fs.test` takes `path` and a `kind` from `exists`, `file`, `directory`,
`symlink`, `readable`, `writable`, `executable` and `empty`, and returns a
boolean. The `kind` port is a text enum, so a misspelled kind fails at
validation before any effect. An absent path answers `false`. A dangling
symbolic link does not `exist`. Every kind except `symlink` follows links to
their targets; `symlink` inspects the entry itself. `empty` means a zero-byte
regular file or a directory with no entries. On Unix, `readable`, `writable`
and `executable` call `access(2)` with the real user ID, as `test -r`, `-w` and
`-x` do. On Windows the approximation is: `readable` opens the file (or lists the
directory), `writable` checks the read-only attribute, and `executable` checks
for an `.exe`, `.com`, `.bat` or `.cmd` extension on a regular file. Windows ACLs
are not evaluated. Other stat failures, such as permission denied while
searching a parent, are errors rather than `false`.

`fs.glob` takes `pattern` and an optional boolean `include_hidden`, and returns a
list of text:

```text
accept pattern as text
run fs.glob with pattern pattern as files
for each f in files
  run "cat" with arguments [f]
```

Syntax applies one path component at a time. `*` and `?` match within a
component. `[...]` matches a set, with ranges and `!` or `^` negation, and `]`
first in a set is literal. An unterminated set is an error. A component that is
exactly `**` matches zero or more directory levels, does not follow symbolic
links, and cannot be the last component. Any other character is literal, so
there is no escape syntax. Wildcards skip names beginning with `.` unless
`include_hidden` is true or the component starts with a literal dot. `.` and
`..` are never produced by a wildcard. A leading `~` followed by a separator or
the end of the pattern expands to `HOME` (`USERPROFILE` on Windows when `HOME`
is unset); `~user` is not expanded. A pattern ending in a separator matches only
directories, and each result keeps that trailing separator. Relative patterns
resolve against the program's working directory and return paths as written.
Results are unique and sorted by UTF-8 bytes, not by locale. An empty match is an
empty list, and a missing directory on the path is also an empty match. Other
directory read errors fail. A result over 100,000 paths, over 1,000,000 examined
directory entries, a non-UTF-8 name in a scanned directory, or recursion deeper
than 256 levels fails with an explicit error. Output uses the platform's native
separator. Under a strict capability policy these two operations are denied, as
`fs.read` is, because they read the host filesystem.

`text.lines` takes `text` or stdin and returns a list of text split on LF. One
final LF is a terminator and adds no empty item, and one CR before each LF (or
at the end of an unterminated final line) is removed. Empty input gives an
empty list. Lists over 100,000 items fail.

`text.split` takes `text` or stdin and exactly one of `separator` (a nonempty
text) or `nul: true`, which splits on NUL for `find -print0` style output. One
final separator terminates the last item, as with `text.lines`. Empty input gives
an empty list.

A loop keeps only its final iteration's output, as described in the language
reference. To collect every item, append to a text binding inside the loop with
`text.concat`, then print once:

```text
accept listing as text
accept summary as text with default ""
run text.lines with text listing as items
for each item in items
  run text.concat with parts [summary, "[", item, "]"] as summary
run "printf" with arguments ["items: %s\n", summary]
```

The example programs `examples/stdlib/file-tests.program`,
`glob-files.program` and `lines-to-list.program` use these forms. The
operations are covered by `tests/lane_a_applets.rs` and `tests/lane_a_cli.rs`.

## Text and path builders

These in-process operations build strings without spawning `jq` or `printf`.
Every one takes named text arguments and returns one text value.

| Applet | Arguments | Behavior |
| --- | --- | --- |
| `text.default_if_empty` | `text`, `default` | Returns `default` only when `text` has zero bytes; spaces are not empty. |
| `text.replace` | `text`, `old`, `new` | Replaces every non-overlapping literal `old` (nonempty) left to right. No pattern syntax. |
| `text.strip_prefix`, `text.strip_suffix` | `text` and `prefix` or `suffix` | Removes one matching end; otherwise returns `text` unchanged. |
| `text.lower`, `text.upper` | `text` | Unicode default case mapping, independent of locale. `straße` upper-cases to `STRASSE`. |
| `text.join` | `parts`, optional `separator` | Same operation as `text.concat`, named for string building. |
| `path.join` | `parts` | Joins segments with the platform separator via `Path::push`. Empty segments are skipped and an absolute segment replaces the path so far. Nothing is normalized. |

These results are JSON string values on the in-process path, as the other text
applets are. Writing them as raw bytes at a pipe or terminal boundary needs a
runtime change (issue #45). Typed `as name` bindings already receive the exact
text.

The example `examples/stdlib/text-builders.program` shows the forms together.
Coverage is in `tests/lane_a_text_path.rs`.

## Template rendering

`text.render` substitutes `{{ name }}` placeholders from a JSON object. It takes
exactly one of `template` (text) or `template_path` (a path read relative to the
program directory, at most 16 MiB and UTF-8), plus `values` (a JSON object):

```text
run text.render with template_path "alacritty.toml.tpl" and values values as rendered
```

A placeholder is `{{` and `}}` with optional spaces or tabs inside. Its name is
one to 128 ASCII letters, digits, `_`, `-` or `.`, looked up as an exact key. A
missing key, a value that is null, an array or an object, an invalid or
unterminated placeholder, or a non-object `values` fails. Strings are inserted
verbatim. Numbers and booleans use their JSON text (`42`, `0.5`, `true`). Extra
keys are ignored. Substituted text is never scanned again. `{{{{` produces a
literal `{{`, and a `}}` outside a placeholder is literal. There are no
conditions, loops or expressions, and the output keeps every other byte, with no
added or removed trailing newline. It is not on the strict capability policy's
in-process allowlist, so a strict policy denies it for both forms; a policy-aware
template form would need its own reviewed entry.

The example `examples/stdlib/render-template.program` reads a JSON object with
`json.parse`. Coverage is in `tests/lane_a_render.rs`. That test uses a fixture
of the `{{ name }}` form rather than an Omarchy file.

## Strict YAML, TOML and INI subsets

`yaml.parse`, `toml.parse` and `ini.parse` read `text` (or stdin) and return the
document as a JSON value. `yaml.stringify`, `toml.stringify` and `ini.stringify`
take `value` (a JSON value) and return text. Each format is a small strict
subset implemented in the interpreter, so no parser crate is added. Anything
outside the subset fails with the format name and a line number. Duplicate keys
fail, nesting is limited to 64 levels, input is limited to 16 MiB, and a document
may hold at most 1,000,000 values. Output keys are sorted. Comments are not
preserved, so these operations do not edit a file in place.

## Environment files without `source`

`env.load` parses `KEY=VALUE` shell environment files, such as `/etc/os-release` or a
theme file, and returns a JSON object of text values. It never runs the file. A line
may begin with `export`; blank and `#` comment lines are skipped. A value is unquoted,
single-quoted (literal), or double-quoted with the escapes `\\`, `\"`, `\$` and
``\` ``. A value that a shell would expand, run, or split is refused with its line
number: `$`, backquotes, and other shell metacharacters outside single quotes, an
unquoted value with spaces, and a name set twice. Errors name a line, never a value.

The file is read by an ordinary `read` step, so the read is a host read that the
capability policy authorizes as any other read is. `env.load` takes that text as its
pipeline input:

```program
read "/etc/os-release"
then env.load
```

Its result is a JSON value, so `take json at "/ID"` reads one name. The applet has no
effects and runs under a strict policy. Values in a file that the policy marks as
sensitive are not yet labelled; the file's values are ordinary text.

YAML supports block mappings and sequences indented with spaces, plain, single-
and double-quoted scalars, one-line flow collections, literal (`|`) and folded
(`>`) block scalars with clip, strip and keep chomping, comments, and a single
leading `---`. A plain scalar resolves to null (`null`, `~`, empty), a boolean
(`true` or `false` in common capitalizations), an integer or a decimal float.
Other plain text is a string, so `0x1F` and `1_000` stay strings. Anchors,
aliases, tags, directives, multiple documents, complex keys, merge keys, tabs in
indentation, and multi-line plain or quoted scalars fail. Infinite and NaN values
fail because JSON cannot represent them.

TOML supports bare, quoted and dotted keys, basic, literal and multi-line strings
with the TOML escapes, integers in decimal, `0x`, `0o` and `0b` forms with
underscores, decimal floats, booleans, arrays that may span lines with a trailing
comma, inline tables, `[table]` headers and `[[array]]` headers. Dates, times,
`inf` and `nan` fail. A header may not repeat a table, and a header may not
extend a value created by a dotted key or an inline table. `toml.stringify`
rejects null, because TOML has no null value.

INI supports `[section]` headers, `key = value` and `key: value` entries, and
comment lines starting with `;` or `#`. Values are the raw text after trimming,
so every INI value is a string and quotes are not interpreted. Continuation lines,
duplicate keys or sections, and a section that shares a name with a top-level key
fail. `ini.stringify` accepts top-level scalars and one level of sections, and it
rejects text that would change on the next read.

For YAML and TOML, `parse` after `stringify` returns the same JSON value for the
supported shapes. INI values always come back as text. Coverage is in
`tests/lane_a_config.rs`.

## Idempotent filesystem ensures

These in-process operations converge a path on a desired state and report whether
they had to change it. Each returns a record whose `changed` boolean can feed a
condition after `take boolean at "/changed" from result as name`. When the path
already matches, nothing is written and no lock file is created. A changed file
write takes the same sibling advisory lock as `fs.write_atomic` and replaces the
file with its compare-and-swap, so a concurrent edit makes the call fail with a
conflict rather than losing that edit.

- `fs.ensure_file` takes `path`, `content` (text) and an optional `mode`. It
  returns `changed`, `path` and `mode_applied`.
- `fs.ensure_line` takes `path`, `line` (one line without a line break) and
  `state` of `"present"` (the default) or `"absent"`. Matching ignores a trailing
  CR, and other bytes are kept exactly. A missing file is created only for
  `present`. It returns `changed` and `path`.
- `fs.ensure_symlink` takes `path` (the link) and `target`. It creates the link,
  keeps it when it already points at `target`, and replaces an existing link. It
  refuses to replace a regular file or directory, and it takes no advisory lock.
  It returns `changed`, `path` and `target`.
- `fs.ensure_dir` takes `path` and an optional `mode`, creates missing parents
  (like `mkdir -p`), and refuses an existing non-directory. It returns `changed`,
  `path` and `mode_applied`.

`mode` is octal text such as `"0644"` or `"600"`, so it reads like `chmod`. It is
enforced on Unix. On other platforms it is validated, `mode_applied` is `false`,
and the file mode is left alone. `fs.ensure_file` and `fs.ensure_line` follow an
existing symbolic link to its target, as `tee` does. Both read at most 16 MiB,
and `fs.ensure_line` requires UTF-8 text. These operations write host files, so a strict
capability policy denies them, as it denies `fs.write_atomic`.

```text
accept root as text
run fs.ensure_dir with path root as made_dir
take boolean at "/changed" from made_dir as dir_changed
```

The example `examples/stdlib/ensure-config.program` combines these with
`path.join`. Coverage is in `tests/lane_a_ensure.rs`.

## Checking programs in CI

`execute check FILE` validates a program without running its applets or effects.
`--json` prints the full diagnostic record. `--format github` prints one GitHub
Actions workflow command per diagnostic, so a pull request shows the problem on the
line it names: `::error file=...,line=...,col=...,title=CODE::message`. Warnings
become `::warning`, and hints become `::notice`. The file name is relative to the
working directory with forward slashes. The exit status is unchanged, so a failing
check still fails the job. `--format` accepts only `github` or `json`.

The repository's composite action builds the CLI and adds it to `PATH`:

```yaml
# .github/workflows/programs.yml
name: check programs
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/setup-execute
      - name: Check every program
        run: |
          status=0
          for file in examples/*.program; do
            execute check "$file" --format github --manifest-dir manifests || status=1
          done
          exit "$status"
```

Programs that declare required inputs need `--input NAME=VALUE` in the check, or
they report the missing input. GitLab does not render workflow commands, so its
example stores the JSON record as an artifact and fails the job on the same exit
status:

```yaml
# .gitlab-ci.yml
check-programs:
  image: rust:latest
  script:
    - cargo build --locked --release --bin execute
    - status=0
    - for file in examples/*.program; do ./target/release/execute check "$file" --json --manifest-dir manifests > "$(basename "$file").check.json" || status=1; done
    - exit "$status"
  artifacts:
    when: always
    paths:
      - "*.check.json"
```

## Theme switching

`examples/desktop/theme-switch.program` composes the applets above into a theme
switch. It renders each `*.tpl` template into a new theme directory with
`text.render`, points the current-theme link at that directory with
`fs.ensure_symlink`, and reloads the compositor. If the reload fails, it points the
link back at the previous target and fails.

Ordering matters more than atomicity. Rendering finishes before the link changes,
so a render failure leaves the current theme untouched. The link swap is the commit
point. The rollback is a second link update, not a transaction: `fs.transaction`
handles regular files and rejects symbolic links, so it cannot swap the link.
`fs.ensure_symlink` reports the previous target in `previous` so the rollback knows
where to point. A first switch needs an existing link, because a previous target
of `""` cannot be restored. A glob that matches no templates renders nothing and
the switch proceeds; the example does not count matches.

Typed editing of hyprlang, TOML and INI configuration files is intentionally out of
scope for applets. Templates are rendered as text, and the program writes them. No
applet parses or rewrites those formats in place.

Coverage: `tests/lane_a_theme.rs`. The portable test checks the program. The Unix
tests create real links and a fake `hyprctl`.

## Desktop wrappers

These are ordinary command manifests that wrap installed desktop tools. Each one
declares its argv shape, an outcome table, and its declared effects. Choices are
port enums, so a bad value fails at `check`. Switch-style options use a
`flags` list, because a boolean parameter would still count as present when
`false`. Outcome names replace bare exit codes where the tool's codes are known.
Menus and status queries use `result_envelope`, so a program can read the outcome
name from `take text at "/outcome" from reply` after `and keep status as code`.
Without that status capture, a non-success exit stops the program.

- Notifications and OSD: `notify.send` (summary, body, urgency, icon, expire_ms,
  app_name, replace_id, up to three actions, flags). With an action, `--wait`
  makes stdout the selected action key. `notify.dismiss` and `notify.restore`
  wrap `makoctl`. `osd.volume`, `osd.brightness` and `osd.caps` wrap
  `swayosd-client`.
- Menus: `menu.walker` and `menu.fuzzel` run in `--dmenu` mode and read items from
  stdin. `menu.gum_choose`, `menu.gum_input` and `menu.gum_confirm` wrap `gum`.
  Exit 1 from walker or fuzzel, and exit 130 from gum, both map to `cancelled`.
  The exit-code meanings were taken from upstream documentation and need checking
  against the installed versions.
- systemd: `systemd.user.is_active` maps 0 to `active`, 3 to `inactive` and 4 to
  `unknown`. `start`, `stop`, `restart`, `enable` and `disable` are effectful
  and `uncertain_on_failure`.
- D-Bus: `dbus.monitor_user` and `dbus.monitor_system` run `busctl monitor
  --json=short` as listener producers, with an optional `match` rule.
- Packages: `package.query` maps `pacman -Qi` to `installed` or `not_installed`.
  `package.install` (pacman, `--needed --noconfirm`) and `package.install_aur`
  (yay, `--needed`) are effectful and `uncertain_on_failure`. Installing requires
  privileges the wrapper does not provide. `package.check_updates` maps
  `checkupdates` exit 0 to `updates` and exit 2 to `up_to_date`.
- Hyprland: `hypr.clients`, `hypr.monitors` and `hypr.workspaces` return a list of
  JSON values. Items are typed as `json` because the record schema rejects
  undeclared fields and Hyprland's objects contain many. `hypr.activewindow`
  returns one JSON value. `hypr.dispatch` takes a list of dispatcher arguments
  and is effectful. `hypr.socket2` is a `socat` listener producer.

Environment inputs. The Hyprland wrappers read `HYPRLAND_INSTANCE_SIGNATURE` from
the environment, which the tool inherits. The socket for `hypr.socket2` is
`$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock`, passed as the
`socket_path` input. The manifest adds the `UNIX-CONNECT:` prefix itself. The path must
be absolute and free of `,`, `:`, `!`, whitespace, control characters and `..`
components, so socat's address language (`EXEC:`, `SYSTEM:`, `OPEN:`) cannot be
reached. The check runs before socat starts. A listener must follow its input
declarations directly, so the caller composes the path; `examples/desktop/hyprland-events.program`
shows the command. Listener handlers receive each record as their pipeline input,
so `then "cat"` forwards it unchanged.

`hypr.dispatch` is an allowlist. Its first argument must be exactly one of a fixed
set of non-spawning dispatchers (`workspace`, `movefocus`, `killactive`, and so on;
see the manifest). The whole first item is compared, case-insensitively. A blank
item, an item starting with `-` (which hyprctl would read as its own option, so
`-j exec` cannot hide a dispatcher), and any unknown name are refused before
hyprctl starts. `exec` and `execr` are not in the list. `hypr.dispatch_exec` accepts
only those two dispatchers and declares `process.spawn`, so capability policy can
tell the two apart. Both are effectful. hyprctl's `--` end-of-options marker is not
used, because its support is not confirmed from documentation.

Both dispatch manifests also set `forbid_option_items`, which applies to every item,
not only the first. An item starting with `-` is refused unless it is a plain number
such as `-10` or `-10%`, and an item containing `;`, a newline, a carriage return or
a NUL is refused. A later `--batch` therefore cannot run a `;`-separated command
batch. The rule runs on literal values at check time and on resolved values before
argv is built.
The notification action keys, the menu prompts and the D-Bus match rule are not
interpreted as commands: notify-send only reports the selected key, gum and the
menus only read input, and busctl only filters messages, so those manifests do
not refuse input.

Prefixed arguments (`{"prefix": ..., "parameter": ...}`) are the general form for
a path appended to a tool's own address syntax. They validate the same way.

Examples live in `examples/desktop/`. The portable checks are in
`tests/lane_a_desktop.rs`: they inspect each manifest, assert the argv shapes
with `execute explain`, and run `execute check` on the examples. Tests that run
the tools use owned shell fakes on Unix only, so the real tools are not required.

## Benchmark boundary

The [applet boundary harness](../benchmarks/applet_boundaries.py) compares one
identical operation through the one-shot helper, in-process and persistent
worker paths. It validates exact output, status and typed binding before timing,
then retains interleaved raw samples, launch counts, serialization costs,
hashes, warmups and environment. Run it only against the final release build,
with builds and tests stopped; the wrapper requires explicit final-build and
idle-window confirmations. A result file is evidence for its exact source,
executables and host; it cannot establish a universal speed advantage. Omarchy
happy-path timings are reported separately from correctness improvements.
The [paired Omarchy harness](../benchmarks/omarchy_success.py) reuses its
owned Bash-differential fixtures and has no result until the final-build gate.
