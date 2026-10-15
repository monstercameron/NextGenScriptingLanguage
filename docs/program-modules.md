# Importable `.program` modules

A finite `.program` definition can be exposed as a named applet. A scheduled
`.program` source can also be exposed as a lifecycle-owned graph service.
Callers keep the ordinary applet syntax:

```program
accept payload as json
run examples.records.report with payload payload as report
then encode report as json
```

The applet manifest selects the source file, exported definition and stable
interface version under its versioned execution contract:

```json
{
  "contract": {
    "version": 1,
    "backend": "program_module",
    "program": {
      "source": "../records.program",
      "entry": "echo_record",
      "interface_version": 1
    },
    "lifecycle": { "kind": "oneshot" },
    "output": {
      "type": "record",
      "encoding": "json",
      "fields": {
        "name": { "type": "text" },
        "count": { "type": "integer" }
      },
      "required": ["name", "count"]
    }
  }
}
```

`source` is relative to the applet manifest that declares it, regardless of
the caller's current directory. The runtime resolves and hashes source during
preflight. For interface 1, the entry must be a top-level
`define a program named ...` in a definitions-only file. Its `accept`
declarations are checked against the exported input contract. Interface 2
selects the scheduled source root `main`, as described below. Unknown entries,
unsupported interface versions, invalid input/output schemas, recursive
definitions and unsupported lifecycle declarations fail before effects begin.

For a package export, also set both `package_manifest` and `package_lock` in
`contract.program`. They use the same applet-manifest-relative path rule. The
descriptor's `source` must resolve to the source selected by the named package
export, and `entry` selects that package's public export name. The loader checks
the explicit lock before execution. Direct source descriptors omit both package
paths and do not discover package dependencies implicitly.

## Package manifest

`program.modules.json` describes a local package and its exports. The sample
under [examples/modules](../examples/modules/) provides a typed record export
and file-loaded applet manifest. Run it from the repository root:

```sh
execute --manifest-dir examples/modules \
  --input 'payload={"name":"demo","count":2}' \
  examples/modules/caller.program
```

The package manifest and lock live below the catalog root so the registry reads
only the applet manifest while package paths remain confined to that root. The
package manifest fields are strict: unknown keys are errors.

```json
{
  "format_version": 1,
  "name": "example.text",
  "version": "1.2.3",
  "interface_version": 1,
  "source": "src/library.program",
  "source_sha256": "64 lowercase or uppercase hexadecimal characters",
  "exports": {
    "describe": {
      "entry": "describe",
      "interface_version": 1,
      "inputs": [
        { "name": "payload", "type": "json", "required": true }
      ],
      "output": {
        "type": "record",
        "encoding": "json",
        "fields": {
          "name": { "type": "text" },
          "count": { "type": "integer" }
        },
        "required": ["name", "count"]
      },
      "outcomes": [],
      "result_envelope": false,
      "lifecycle": { "kind": "one_shot" },
      "capabilities": [],
      "effects": []
    }
  },
  "dependencies": {}
}
```

An optional `ports` property may be included in a package export. Interface-2
exports require it; interface-1 exports may omit it, and omission is preserved
when the legacy metadata is serialized.

Package versions use SemVer. Dependency `version` accepts an exact version
(`=1.2.3`) or a caret range (`^1.2.0`). A dependency declares the relative
package-manifest `manifest` path, `source_sha256` for that dependency's
`.program` source, `manifest_sha256` for its package metadata, and an interface
range whose lower bound is inclusive and upper bound is exclusive. For example, `interface_min: 1` with
`interface_max_exclusive: 2` accepts interface version 1. Dependency paths
are resolved relative to the manifest that names them. There is no network
resolution or implicit installer.

The loader rejects a path that resolves outside its configured module root,
including a symlink whose target is outside that root. It checks the selected
source hash, every dependency source hash, package and interface versions,
export declarations, and dependency cycles before a module can execute.
Dependency aliases are callable only inside the declaring package. For example,
if the package declares a dependency named `text`, its exports are available to
that package as `text.normalize` and `text.report`. These names resolve to the
pinned package exports inside a private catalog scope; an ambient catalog entry
with the same name cannot replace the dependency. Each dependency can in turn
use only its own declared aliases. Calls to unrelated ambient `program_module`
applets from a module body are rejected during preflight.

## Lock file and bounds

`program.modules.lock.json` is a deterministic snapshot of every package
reachable from the selected root. It pins each package manifest path, version,
interface version, manifest SHA-256 and source SHA-256. Loading with a lock
requires an exact match. Updating a package or dependency therefore requires
an explicit lock refresh; loading never writes or downloads packages.

The current finite module interface is version 1. An export declares typed
inputs, output, outcome classifications, result-envelope behavior, capabilities
and effects. It can return raw bytes or a typed value. A record output is
encoded as JSON and gives independent named outputs, such as `name` and
`count`; the runtime validates the JSON against the record schema before
making it available to a caller.
Module-local inputs, bindings and current-directory changes stay in a child
scope, and the caller's scope is restored when the export returns.

## Scheduled service interface 2

Interface 2 opts into a scheduled service lifecycle. Interface 1 remains the
finite, definitions-only format. Package format and lock format stay at
version 1; `interface_version: 2` is the explicit compatibility boundary, and
an older runtime that supports only interface 1 rejects it instead of treating
it as a finite export.

A direct service descriptor selects the reserved source root `main`:

```json
{
  "contract": {
    "version": 1,
    "backend": "program_module",
    "program": {
      "source": "poller.program",
      "entry": "main",
      "interface_version": 2
    },
    "lifecycle": { "kind": "program_service" },
    "output": {
      "type": "record",
      "encoding": "json",
      "fields": { "count": { "type": "integer" } },
      "required": ["count"]
    },
    "ports": {
      "inputs": {
        "threshold": {
          "schema": { "type": "integer" },
          "delivery": "single"
        }
      },
      "events": {
        "updated": {
          "schema": {
            "type": "record",
            "fields": { "count": { "type": "integer" } },
            "required": ["count"]
          },
          "delivery": "events"
        }
      }
    }
  }
}
```

The source declares startup inputs and one periodic or cron trigger:

```program
accept threshold as integer
every 5 seconds
  run "printf" with arguments ["{\"count\":1}"]
```

The service root is not a reusable finite definition. It must declare exactly
one `every` or `on schedule` trigger; `listen` sources are not part of this
interface. Input declarations are bound once when the graph starts. Each must
have a corresponding typed `Single` input port, and the graph rejects records
or event inputs rather than inferring a join or changing startup values while
the service is running. Defaults remain available for optional inputs.

Each service declares exactly one typed event port and no stdout output ports.
After a handler completes, its entire output is decoded through the declared
applet output contract and emitted as one event. The event schema must exactly
match that decoded value. When `result_envelope` is enabled, the event carries
the canonical result-envelope record. Raw byte outputs are rejected because
they do not provide a typed event schema. This keeps output meaning in the
applet contract and does not infer channel names from fields in returned JSON.

For a package, declare exactly one export with interface 2, `lifecycle:
{"kind":"schedule"}` and `entry: "main"`. The applet descriptor's `entry`
remains the public export name, such as `poll`; it does not select a private
finite definition. The export declares the same input, output, outcome,
result-envelope, capability, effect and port metadata as the applet contract.
The loader requires exact equality, so a package update cannot silently change
the applet's published interface. `ports` is optional package metadata for
legacy interface-1 exports and is omitted from their serialized form when
absent; it is required for interface 2.

The package export contains that same contract metadata:

```json
{
  "format_version": 1,
  "name": "example.poller",
  "version": "1.0.0",
  "interface_version": 2,
  "source": "src/poller.program",
  "source_sha256": "64 hexadecimal characters",
  "exports": {
    "poll": {
      "entry": "main",
      "interface_version": 2,
      "inputs": [
        { "name": "threshold", "type": "integer", "required": true }
      ],
      "output": {
        "type": "record",
        "encoding": "json",
        "fields": { "count": { "type": "integer" } },
        "required": ["count"]
      },
      "lifecycle": { "kind": "schedule" },
      "ports": {
        "inputs": {
          "threshold": {
            "schema": { "type": "integer" },
            "delivery": "single"
          }
        },
        "events": {
          "updated": {
            "schema": {
              "type": "record",
              "fields": { "count": { "type": "integer" } },
              "required": ["count"]
            },
            "delivery": "events"
          }
        }
      }
    }
  },
  "dependencies": {}
}
```

In a graph, a call to a `program_service` applet becomes a persistent graph
node automatically. The graph owns its lifetime: activation starts the
existing `.program` scheduler, and graph cancellation, reconfiguration or a
total timeout stops it. Handlers run serially, one at a time. The current
scheduler skips missed timer ticks by default, retaining the interval phase.
Each emitted event goes through the graph's bounded queue; a lossless edge can
backpressure the handler, while a configured lossy edge follows its declared
drop policy. A handler error, invalid decoded output or failed event emission
fails the service node and the graph. There is no implicit retry or
exactly-once acknowledgement between an external effect and downstream event
processing. A new graph generation starts with fresh service inputs and
handler scopes.

Interface-2 services are graph nodes only. A finite program, reusable finite
definition or another service handler cannot invoke one as an ordinary
one-shot applet call; preflight rejects that nesting before effects. Service
handlers can call finite applets through their preloaded package dependency
scope. Imported source, manifests and dependencies are resolved and validated
before graph effects begin. No wrapper or generated application source is
created.

Interface-1 modules retain their existing one-shot behavior and package JSON:
unknown entries, unsupported lifecycle declarations, invalid input/output
schemas, recursive definitions and unsupported interface versions fail before
the caller starts effects. Declared package imports may call one another along
the pinned dependency DAG; cycles and paths deeper than 16 packages are
rejected before caller effects. An export cannot reach an ambient
`program_module` applet, so task and graph workers do not gain a way around the
statically bounded dependency graph. A package dependency graph is limited to
16 levels, 128 packages and 16 MiB of source plus manifest data; all preloaded
module graphs for one execution share a 16 MiB bound. The existing
reusable-program expansion limit also applies. Direct module source and every
explicitly locked transitive package manifest and source are part of checkpoint
identity, so a resumed run refuses to reuse completed work after module
content changes.

Ordinary child applet manifests and executable contents are also fingerprinted
before execution. Checkpointed finite calls fail closed when a module body
changes its working directory and invokes an ordinary child applet, because
that child’s relative executable identity cannot yet be pinned to its exact
scoped cwd. Non-checkpointed finite module calls can still change cwd; the
caller’s cwd remains isolated and is restored when the module returns.
