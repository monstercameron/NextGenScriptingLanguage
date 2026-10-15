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
```

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
| `path` | A contract-only semantic string. Raw form must be UTF-8; JSON form must be a JSON string. It must be nonempty and contain no NUL. A path parameter accepts a string literal or text result reference and emits exactly one argv item. Path output uses the existing text binding type. |
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
| `in_process` | Calls a bundled generic operation in the interpreter process. It does not launch an applet process and cannot join a native pipe group. Since the operation runs with the host interpreter, it is not eligible for the OS command sandbox. |
| `json_lines_worker` | Sends framed requests to a persistent helper process. It cannot join a native pipe group or use the applet cache. The worker is shared by registry clones, serializes requests per worker, and is reaped when its registry pool is dropped. The pool defaults to 16 identities and can be configured from 1 through 1024; a full pool rejects a new identity without spawning or evicting a healthy worker. This backend is supported on Unix; unsupported platforms return an error. |
| `program_module` | Executes a prevalidated local `.program` package export in a private finite scope, with the caller's registry, policy, cancellation and runtime session. Package formats, dependency pins and limits are in the [program module guide](program-modules.md). |
| `remote` | Calls a host-managed endpoint using the typed named ports. The remote request is not replayed after an uncertain transport failure; origins, credentials, descriptor pins, event framing and limits are in the [remote applet guide](../examples/remote/README.md). |

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
command timeouts are not deadlines for arbitrary filesystem calls, and the
interpreter's output limits are not universal bounds on a command's reads,
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
| `json.get`, `json.set` | Select or replace a value using RFC 6901 JSON Pointer. `get` reports a missing target with status 1; `set` can add an object member, replace an array item or append at the array end, but does not create missing parents. |
| `json.map`, `json.filter` | Project one JSON Pointer from each selected array item, or keep items matching up to 32 ANDed predicates, including `exists`, `equals`, `not_equals` and `type`. A missing projection or selected target reports absence; malformed pointers, predicates and wrong value types fail validation. |
| `fs.read`, `fs.snapshot` | Read a regular UTF-8 file up to 16 MiB and return `{content,content_hash,snapshot,symlink,mode}`. The content hash is SHA-256 of file bytes; the snapshot also binds path, link/target identity and metadata. Missing files have the explicit absence outcome. |
| `fs.write_atomic` | Require the expected SHA-256 or `"missing"`, then atomically replace one file. It requires explicit `preserve`, `reject` or `replace` symlink policy and `preserve`, `private` or `exact` permission policy; an optional stable advisory lock path has a 30-second default and 300-second maximum timeout. |
| `fs.transaction` | Apply a bounded journaled batch of expected-hash writes or removals to existing regular files. It is a command applet with explicit recovery, not a multi-file atomic-visibility guarantee. |
| `fs.transaction.recover` | Complete or roll back a retained transaction journal if every target still matches its recorded before or after state, including a requested absent state. A third state is a manual-recovery conflict. |
| `jsonc.set`, `jsonc.remove` | Apply a root-property update to JSONC with line/block comments and trailing commas, with at most 256 levels of nesting. `set` replaces the last active duplicate or inserts the key; `remove` removes every active duplicate. Keys in comments and nested objects are ignored. Both return a typed receipt with path, content hash, snapshot and whether content changed. Malformed input fails before mutation. |
| `jsonc.edit_content` | Apply the same bounded root-property set/remove logic to supplied JSONC text or UTF-8 stdin and return updated text. It is a pure transformation and never writes a file. |

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
