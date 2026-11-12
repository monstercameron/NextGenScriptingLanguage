# Stability and deprecation

This page says which Execute surfaces carry a compatibility promise, how each
one is versioned, and how changes are announced. It describes the 0.9 release
candidates. It does not establish a 1.0 policy; the decisions that are still
open are listed at the end.

## Status before 1.0

Execute is pre-1.0 (`Cargo.toml` is at 0.9.0-rc.5). Before a 1.0 release, a
surface keeps its current behaviour only where its row below says it is
versioned or stated stable. Any other change can land in any commit and is
recorded in [CHANGELOG.md](../CHANGELOG.md) under Unreleased.

## Promise levels

- **Versioned.** The identifier carries a version (a marker, a protocol string
  or a schema `$id`). A change that would break readers is published under a new
  version. The published definition of an older version is not edited.
- **Stated stable.** The README or another document says the value is stable,
  but the value has no version marker of its own.
- **Documented.** Described in `docs/` with no version and no stability claim.
  It can change before 1.0, and the change is recorded in the changelog.
- **Experimental.** No surface is labelled experimental yet, in the CLI or in
  the documentation.

## Surfaces

| Surface | Identifier or version | Level |
| --- | --- | --- |
| Program language | `execute N` marker. Only version 1 is accepted; a program without the marker is version 1. `execute inspect` and `execute explain --json` report `language_version`. See [language.md](language.md#language-version). | Versioned |
| Check output | `protocol` value `execute.check/1` in `execute check --json`; result shape in `docs/check-result.schema.json` | Versioned |
| Explain output | `docs/schemas/explain.v1.schema.json` and `explain.v2.schema.json` | Versioned |
| Graph event stream | `execute.graph-event/1`; `docs/schemas/graph-event.v1.schema.json` | Versioned |
| Live control protocol | `execute.live/1`; see [live-control.md](live-control.md) | Versioned |
| Other published schemas | `docs/schemas/checkpoint-status.v1.schema.json`, `error.v1.schema.json`, `repl.v1.schema.json`, `inspect.v1.schema.json`, `gate.v1.schema.json`, `doctor.v1.schema.json`, `what-if.v2.schema.json`, `elevated-applet.v1.schema.json`, `migrate-scan.v1.schema.json` | Versioned |
| Checkpoint journal | `version` field. The current build writes version 6 and reads versions 1 through 6. | Versioned |
| Run record seal | `record_seal_version` (currently 1). No published schema. | Versioned |
| Diagnostic codes | Stable `code` field on each diagnostic, as the [README](../README.md) states | Stated stable |
| Applet names and parameters | None. The catalog is listed by `execute applets list`; see [applet-contracts.md](applet-contracts.md). | Documented |
| Exit codes | None. Changes are recorded in the changelog. | Documented |
| CLI flags and subcommands | None | Documented |

Applet parameters, exit codes and CLI flags have no version marker. Before 1.0
they carry no compatibility promise beyond what the changelog records.

## Deprecation

Execute does not yet have a formal deprecation process, and no check warning
announces a removal in advance. The existing practice is to keep a replaced
field working as an alias. For example, `live schema` keeps the deprecated
`concurrent_clients` field as an alias for `general_clients`
([live-control.md](live-control.md)). A removal without an alias is recorded in
the changelog.

## Announcing changes

- User-facing changes are listed in [CHANGELOG.md](../CHANGELOG.md) under
  Unreleased, and move to a dated version section at release.
- Published versions are not changed. Defects are fixed in a new version
  ([releases.md](releases.md)).
- The changelog has no separate heading for breaking changes yet.

## Not yet decided

- Whether 1.0 extends a compatibility promise to unversioned surfaces (applet
  names and parameters, flags, exit codes).
- The minimum notice before a removal, and whether a removal first appears as a
  check warning with a stable code.
- How experimental surfaces are marked in `applets list --json` and `--help`.
- Whether breaking changes get their own changelog heading.
- Whether CI checks schema content, and applet signatures, against a recorded
  baseline so that a change without a version change fails the build.
