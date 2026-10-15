# Language server and discovery

Execute provides a stdio Language Server Protocol server for `.program` files.
Start it with:

```sh
execute lsp
```

The process reads and writes LSP JSON-RPC frames on standard input and output.
It implements incremental document synchronization, structured diagnostics,
applet and parameter completion, applet and reusable-program hover, local
reusable-program definition and references, and reusable-program rename. Rename
returns a workspace edit only when the proposed identifier is valid, does not
collide with another reusable program in the file, and the edited document still
parses. Navigation and rename are limited to local `define a program named`,
`run program`, `then program`, and `pipe to program` symbols in the open file;
the server does not rename applet names or arbitrary variables.

The server loads manifests from `./manifests` when run from a project checkout,
then checks the installed executable's sibling manifest locations. Set
`EXECUTE_MANIFEST_DIR` to use another manifest directory. LSP positions follow
the protocol's zero-based UTF-16 encoding. The JSON diagnostics API documents
its separate byte-offset and one-based column coordinates below. The wire
behavior follows the [official LSP 3.17 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/).
The server bounds headers to 64 KiB, messages to 8 MiB, open documents to 256,
each document to 4 MiB, all open text to 32 MiB, each change notification to
128 edits, and rename results to 10,000 edits. It rejects duplicate
`Content-Length` headers, stale document versions, and invalid edit batches
without applying any part of the batch.

The VS Code grammar package remains usable without a language server. Configure
an LSP client to launch `execute` with the `lsp` argument and associate it with
the `execute-program` language ID (or `.program` files) to enable semantic
features.

## Structured diagnostics

`flow::diagnostics::analyze_source(source, registry)` returns independent,
machine-readable diagnostics. `execute check --json` includes these diagnostics
while retaining the existing check response fields.

Each diagnostic has `severity`, stable `code`, `message`, and a `span`. Span
`start_byte` and `end_byte` are a half-open UTF-8 byte range. `line`, `column`,
`end_line`, and `end_column` are one-based and columns count Unicode scalar
values. `precision: "token"` means the span identifies a located token; `"line"`
means the parser only identified the containing line, so the full line is
reported rather than an invented token range. LSP notifications translate
these ranges to zero-based UTF-16 positions.

Diagnostics include recoverable independent line syntax errors, unknown applet
names, unknown or missing applet parameters, statically known literal parameter
type mismatches, and pipeline connections whose declared output and stdin
schemas disagree. A pipeline is checked only when both sides declare the
relevant semantic schemas.

## Installed command inventory

`execute applets discover` lists executable files found in PATH directories.
It reads directory entries and file metadata only. It does not start commands,
call `--help`, or infer argument, output, effect, or lifecycle contracts. Every
unmanifested command is marked `unknown_without_manifest`; a validated manifest
remains the source of semantic contract information.

## Typed port matching

`execute applets match --json REQUEST` ranks declared output-to-input port pairs.
The request may constrain the source, target, both, or neither:

```json
{
  "source": { "schema": { "type": "text" } },
  "target": { "schema": { "type": "integer" } },
  "same_applet": false,
  "limit": 20
}
```

Each result reports both endpoints, a score, whether their schemas and delivery
models are compatible, and path-specific `issues` for mismatches. Recursive
schema kinds and record/list shapes plus delivery must agree; a nonnullable
output may feed a nullable input, while the reverse is rejected. Text-to-JSON,
path-to-text and other type or encoding changes require an explicit converter.
The matcher reports declared contracts
and does not imply that the runtime can route every named port through every
execution mode. Scores favor exact request matches and
compatible pairs, then recursively similar record/list shapes; similarity is a
ranking hint and never turns an incompatible pair into a compatible one.
For an incompatible pair, `suggested_converters` can identify a one-step route
through a declared in-process, one-shot applet with no declared effects, one
output, a matching input, and no other required inputs. Each suggestion names
the applet and its input/output ports. This checks declared interfaces; the
applet may still reject particular values at runtime, and the matcher does not
invent multi-step paths or infer semantics from executable help text.
