# Importable `.program` modules

A finite `.program` definition can be exposed as a named applet. Callers keep
the ordinary applet syntax:

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
preflight. The entry must be a top-level `define a program named ...` in a
definitions-only file. The entry's `accept` declarations are checked against
the exported input contract. Unknown entries, unsupported interface versions,
invalid input/output schemas, recursive definitions and unsupported lifecycle
declarations fail before the caller starts effects.

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

Only one-shot finite exports are supported. Listener, scheduled, persistent
worker, UI and other lifecycle-owned exports are rejected before execution.
Declared package imports may call one another along the pinned dependency DAG;
cycles and paths deeper than 16 packages are rejected before caller effects.
An export cannot reach an ambient `program_module` applet, so task and graph
workers do not gain a way around the statically bounded dependency graph.
Independent one-shot module calls in the caller remain supported. A package
dependency graph is limited to 16 levels, 128 packages and 16 MiB of source
plus manifest data; all preloaded module graphs for one execution share a
16 MiB bound. The existing reusable-program expansion limit also applies.
Direct module source and every explicitly locked transitive package manifest
and source are part of checkpoint identity, so a resumed run refuses to reuse
completed work after module content changes.
Ordinary child applet manifests and executable contents are also fingerprinted
before execution. Checkpointed calls fail closed when a module body changes its
working directory and invokes an ordinary child applet, because that child’s
relative executable identity cannot yet be pinned to its exact scoped cwd.
Non-checkpointed module calls can still change cwd; the caller’s cwd remains
isolated and is restored when the module returns.
