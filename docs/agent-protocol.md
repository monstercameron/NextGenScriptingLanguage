# Agent protocol identifiers and schema coverage

This page lists the machine protocol identifiers that the Rust sources emit, which
ones have a published JSON Schema, and how the repository checks that coverage. It
is a reference for people and agents that parse Execute output. It does not add a
protocol or change one.

## Identifiers

An identifier has the form `execute.NAME/N`. The string appears in the JSON
output of the command that owns it, normally in a `protocol` field, so a reader can
tell which shape it has received. The version `N` is part of the identifier. A
change that would break a reader is published under a new `N`; the rules are in
[stability.md](stability.md).

The source emits 39 distinct identifiers covering 33 names. The count is by
`name/version` pair, so a name with two versions counts twice (for example
`explain/1` and `explain/2`). The scan that produces this list reads the `.rs`
files under `src/` and matches the text `execute.NAME/N`. It does not read the
bundled manifests or the documentation.

Schema files live in `docs/schemas/` and are named `NAME.vN.schema.json`, with
underscores in the name written as hyphens (`what_if/2` is `what-if.v2`). The
exception is `check/1`, whose schema is `docs/check-result.schema.json`.

| Identifier | Schema file | Status |
| --- | --- | --- |
| `execute.check/1` | `docs/check-result.schema.json` | published |
| `execute.checkpoint_status/1` | `checkpoint-status.v1.schema.json` | published |
| `execute.doctor/1` | `doctor.v1.schema.json` | published |
| `execute.elevated_applet/1` | `elevated-applet.v1.schema.json` | published |
| `execute.elevated_applet/2` | none | no schema |
| `execute.error/1` | `error.v1.schema.json` | published |
| `execute.explain/1` | `explain.v1.schema.json` | published |
| `execute.explain/2` | `explain.v2.schema.json` | published |
| `execute.gate/1` | `gate.v1.schema.json` | published |
| `execute.graph-event/1` | `graph-event.v1.schema.json` | published |
| `execute.inspect/1` | `inspect.v1.schema.json` | published |
| `execute.migrate-scan/1` | `migrate-scan.v1.schema.json` | published |
| `execute.what_if/2` | `what-if.v2.schema.json` | published |
| `execute.what_if/1` | none | no schema; legacy plan output still emitted |
| `execute.applet/1` | none | no schema |
| `execute.applet-matches/1` | none | no schema |
| `execute.applet-search/1` | none | no schema |
| `execute.applets/1` | none | no schema |
| `execute.applets-discovery/1` | none | no schema |
| `execute.attach/1` | none | no schema |
| `execute.daemon/1` | none | no schema |
| `execute.graph/1` | none | no schema |
| `execute.graph-run/1` | none | no schema |
| `execute.help/1` | none | no schema |
| `execute.live/1` | none | no schema |
| `execute.model/1` | none | no schema |
| `execute.model/2` | none | no schema |
| `execute.model-request/1` | none | no schema |
| `execute.modules/1` | none | no schema |
| `execute.panic/1` | none | no schema |
| `execute.policy-audit/1` | none | no schema |
| `execute.policy-audit-seal/1` | none | no schema; seal domain label used when hashing |
| `execute.policy-audit-seal/2` | none | no schema; seal domain label used when hashing |
| `execute.record/1` | none | no schema |
| `execute.record-seal/1` | none | no schema |
| `execute.repair/1` | none | no schema |
| `execute.run-parts/1` | none | no schema |
| `execute.show-run/1` | none | no schema |
| `execute.test/1` | none | no schema |

Twelve of the 39 identifiers have a schema. The other 27 are listed as exempt in
`WITHOUT_SCHEMA` in `tests/issue_364_schema_coverage.rs`, each with a reason. An
exemption is a statement of current state, not a permanent allowance.

## Compatibility

The compatibility promise, the version rules and the list of surfaces that carry
them are in [stability.md](stability.md). This page does not restate them. In short,
a published version is not edited, and a breaking change gets a new version.

To find out which version a reader has received, read the `protocol` field of the
JSON document and match it against the table above. The schema file for that
identifier describes the fields that version guarantees.

## Coverage check

`tests/issue_364_schema_coverage.rs` scans `src/**/*.rs` for `execute.NAME/N`
literals and checks each one against the table:

- An identifier with neither a schema file nor an exemption fails the test.
- An exemption whose schema now exists fails the test, so the list shrinks as
  schemas land.
- An exemption that no source file emits fails the test, so stale entries are
  removed.

The test checks that a schema file exists. It does not validate output against
the schema. Output validation is done by `tests/agent_schemas.rs`, which uses a
small checker and covers only the schemas that exist.

## Not in this change

- No `execute schema` subcommand. The issue proposes one so that an agent can fetch
  the schema for the binary it runs; it is not implemented here.
- No schema for the 27 exempt identifiers. Each needs its own file under
  `docs/schemas/`, and the exemption is removed when it lands.
- The checker in `tests/agent_schemas.rs` is not replaced by a standard JSON Schema
  validator.
