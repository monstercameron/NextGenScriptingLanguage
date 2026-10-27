# Data-flow policy

The opt-in data-flow policy governs which declared sensitivity labels may leave
the host through a remote applet, or persist in a cache, checkpoint or saved
file. It complements the capability policy: capability grants authorize an
applet's declared operations and OS sandbox requests; data-flow grants authorize
labeled information at selected sinks. Neither policy infers labels from
arbitrary content.

Supply a policy with the source-build CLI:

~~~sh
execute report.program --data-flow-policy ./data-flow-policy.json
~~~

The file is strict version-1 JSON. Only the version is required; omitted
sections have no grants. Unknown fields, duplicate keys, invalid labels and
noncanonical origins fail validation. The policy is bounded to 64 KiB. On Unix,
it must be a regular file owned by the effective user with no group/other
permissions and is opened without following symlinks. Windows rejects reparse
points and requires the file owner to be the current user with an explicit,
non-inherited DACL that grants access only to that user, SYSTEM, or local
Administrators.

~~~json
{
  "version": 1,
  "remote_sinks": {
    "https://api.example.test": {
      "allow": ["public", "internal"]
    }
  },
  "persistence": {
    "cache": ["internal"],
    "checkpoint": ["confidential"],
    "save": ["internal"]
  },
  "cache_purity": {
    "trusted_applets": ["trusted.formatter"]
  },
  "secrets": {
    "environment": {
      "remote.lookup": ["API_TOKEN"]
    }
  }
}
~~~

## Labels and remote egress

Applet port sensitivity is one of public, internal, confidential, secret or
unknown. An omitted sensitivity is unknown. The runtime retains the distinct
labels that contribute to a flow; unknown does not erase a known Secret label.
These are declarations carried across typed boundaries, not content inspection
or proof that an arbitrary executable behaves as declared. Before finite
workflow effects, the runtime scans the actually invoked applets and task-group
calls, plus transitive program-module bodies, and forms an immutable run-scoped
union of their declared input/output labels and declared secret environment
sources. Referenced inputs add Unknown as well as any declared input label.
This conservative floor prevents a downstream Public declaration from
lowering a Secret producer, but it can include labels from branches or tasks
that do not run. Graphs preserve distinct labels across edges and likewise do
not downgrade a producer label at a Public consumer.

Unlabelled stdin, file reads, listener stdin and caller-supplied initial bytes
are Unknown. Literal parameters are Public unless their input port declares
another label. The runtime does not track a separate label on every finite
binding, inspect payload contents to infer labels, or infer labels for data
transformed outside its typed boundaries.

With an active policy, remote egress requires an exact origin entry whose
allow list contains the effective sensitivity label. Origins use canonical
HTTP(S) scheme, host and optional non-default port only. Do not include a path,
query, fragment, credentials, wildcard or trailing slash. For example, the
policy entry https://api.example.test applies to endpoint paths on that origin,
but not to a different host or port. Unknown data is denied unless the policy
explicitly includes unknown for that origin.

This origin and label check applies to host-managed remote applet requests. A
data-flow policy does not inspect or mediate network traffic opened directly by
an arbitrary native child process. Without a data-flow policy, legacy
host-managed remote behavior is preserved. A capability policy alone does not
authorize or constrain remote data labels; the CLI warns that remote egress
and persistence are outside data-flow grants when only the capability policy
is configured. See [applet capability policy](../SECURITY.md#applet-contracts-and-optional-capability-policy)
for that separate boundary.

## Persistence

The persistence keys are cache, checkpoint and save. With an active policy,
public data may persist by default. Internal and confidential data require that
label in the grant for that persistence kind. Unknown data is denied by
default. Secret data is always denied as plaintext and cannot be enabled by a
policy entry; this runtime does not encrypt these stores.

When the data-flow policy is absent, existing persistence behavior is retained
except that Secret data is still never written as plaintext. A present policy
also activates the cache-purity checks below.

The runtime also treats an applet that declares the `filesystem.write` effect
as a persistence sink and checks the run-scoped labels before launching it.
This covers the declared core file mutators, including `fs.write_atomic`,
`fs.transaction`, `fs.transaction.recover`, `fs.unlink_checked`, `jsonc.set`
and `jsonc.remove`. It cannot guarantee protection from arbitrary native code
that writes files while omitting or lying about its effects, or from
host-managed writes outside these guarded sinks; this is not a universal
data-loss-prevention layer.

## Cache purity

Under an active policy, a cacheable applet must have a version-1 contract and
must either be on the fixed reviewed pure in-process operation list or appear
in cache_purity.trusted_applets. The latter is an explicit operator trust
assertion, not evidence derived from an applet's declarations. A cacheable
flag, a capability list or a contract does not prove that arbitrary native
code is pure or accounts for filesystem, environment, clock and other
dependencies. Without a data-flow policy, the prior cache behavior remains.

## Applet-scoped environment secrets

The secrets.environment map associates canonical applet names with environment
variable names whose effective values should be treated as secrets. It names
values; it does not supply them. The runtime captures only the resolved
environment snapshot passed to that applet after its environment filtering,
removals and overrides. It does not scan the host environment. If any names are
declared for an applet, that applet's outputs are treated as Secret for remote
egress and persistence decisions, including before a cache lookup.

Registered values are redacted by exact byte match in supported diagnostic
surfaces. Redaction is bounded to 256 distinct values, 64 KiB per value and
256 KiB total. Exceeding a bound fails closed rather than emitting an
unredacted diagnostic. This does not recognize transformed, encoded or partial
secret values, and it does not scrub arbitrary program output or persistent
files; Secret sensitivity separately blocks plaintext persistence.

## Limits

Sensitivity declarations describe authored interfaces; they do not prove that
arbitrary native code preserves labels or reveal every secret derived from an
input. A declared Secret environment variable makes that applet's outputs
Secret, while exact-value redaction only covers registered bytes on supported
diagnostic surfaces. Unknown provenance stays unknown, and an active policy
denies unknown remote egress and persistence by default. Review applet manifests, origin
grants, persistence grants and cache-purity assertions as part of deployment.
The policy decision history is bounded and records identifiers, action, origin,
risk and outcome, not request payloads or secret values. `execute run
--record` persists that audit into the run record with the policy in force, so
grants and denials stay reviewable after the run; `execute show-run` prints
them. A run with no policy records that no confinement applied.

