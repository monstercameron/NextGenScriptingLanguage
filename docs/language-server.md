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

`execute lsp` is part of the `full` build (the `lsp` feature); a lean build refuses it
with a message that says to use the full build. The server never searches the working
directory for manifests, because a workspace could then shadow bundled applets. It checks
the installed executable's sibling `manifests` and `share/execute/manifests` locations.
Set `EXECUTE_MANIFEST_DIR` to use another manifest directory. LSP positions follow
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

Diagnostics include recoverable syntax errors, duplicate names, scope and range
errors, reusable-program resolution and input errors, unknown applets and
parameters, statically known type mismatches, and incompatible pipeline
schemas. A pipeline is checked only when both sides declare the relevant
semantic schemas.

### Diagnostic codes

Every diagnostic carries a code. A published code is never renamed, and a changed
meaning gets a new code. The family names the stage that finds the problem.

| Code | Family | Meaning |
| --- | --- | --- |
| `check.duplicate_name` | validate | a declaration, parameter, binding, or object repeats a name |
| `check.scope` | validate | a control-flow, job, declaration, or trigger is outside its valid scope or order |
| `check.program.unknown` | resolve | a called reusable program is not defined |
| `check.program.cycle` | validate | a reusable program call graph contains a recursive cycle |
| `check.program.input.unknown` | validate | a reusable program call supplies an input it does not declare |
| `check.program.input.required` | validate | a reusable program call omits a required input |
| `check.range` | validate | a literal is outside the supported range for its setting |
| `check.value.constraint` | validate | a literal violates a declared value or input constraint |
| `check.parameter.direct` | validate | a direct executable call uses named applet parameters |
| `check.bound_output_withheld` | validate | a final bound value is internal; explicitly reuse it to produce program stdout |
| `parse.lexical` | parse | a line has an unterminated or invalid quoted or bracketed token |
| `parse.syntax` | parse | a statement does not match the grammar |
| `parse.indentation` | parse | a line is indented with tabs, or its indentation does not nest |
| `parse.control` | parse | a control construct is unsupported or nested beyond the supported depth |
| `source.invisible_format` | validate | source contains a Unicode format control that can change how it is displayed |
| `check.undefined_name` | validate | a referenced result or input name is not defined at this point |
| `check.type_mismatch` | type | statically known value types are incompatible at a use site |
| `check.diagnostics.truncated` | validate | more diagnostics were found than the response limit permits |
| `applet.unknown` | resolve | the operation names no declared applet and is not a quoted executable |
| `applet.parameter.unknown` | resolve | the applet declares no parameter with this name |
| `applet.parameter.type` | type | a literal value does not match the parameter's declared type |
| `applet.parameter.required` | validate | a required parameter is missing from the call |
| `applet.pipeline.incompatible` | type | a pipeline stage's declared output does not satisfy the next stage's input |
| `data_flow_policy_not_configured` | policy | a capability policy is set without a data-flow policy, so remote egress and persistence are unprotected |
| `check_failed` | validate | validation failed without a static diagnostic; the `causes` list explains why |
| `lint.shell_expansion_literal` | lint | shell-like expansion syntax in a literal argument is passed literally |
| `lint.executable_name_whitespace` | lint | a quoted executable name contains shell metacharacters and is looked up as one filename |
| `lint.empty_loop` | lint | a loop is guaranteed to execute zero times |
| `lint.unused_binding` | lint | a result binding is not read in its lexical program body |
| `lint.unused_input` | lint | a declared input is not read in its lexical program body |
| `model.result_unchecked` | lint | a model result is consumed without checking its acceptance gate |
| `model.threshold_missing` | lint | a classification or scoring call omits its minimum confidence threshold |
| `model.text_to_command` | lint | model-generated text is passed to a shell command interpreter |
| `lint.unreachable` | lint | a statement follows an unconditional exit in the same body |

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
For an incompatible pair, `suggested_converters` identifies one-step routes
through declared in-process, one-shot applets with no declared effects, one
output, a matching input, and no other required inputs. The additive
`suggested_conversion_plans` field can also contain ordered chains of two or
three such applets:

```json
{
  "suggested_conversion_plans": [
    {
      "steps": [
        {
          "applet": "path.basename",
          "input_port": "path",
          "output_port": "value",
          "outcome": "contracts_match",
          "score": 100
        },
        {
          "applet": "number.parse_integer",
          "input_port": "text",
          "output_port": "value",
          "outcome": "contracts_match",
          "score": 100
        }
      ]
    }
  ]
}
```

Plans are ordered by stage count and then by applet and port names. Multi-stage
planning is bounded to three stages, five plans per endpoint pair, 512 expanded
paths, 2,048 queued paths, and 8,192 candidate-edge checks per endpoint pair.
Across a single match request, multi-stage planning examines at most 1,000,000
schema/edge pairs, expands 8,192 paths, checks 65,536 candidate edges, and
returns 256 plans. It retains at most four paths per typed state and stage
count. These limits keep broad
multi-stage routes bounded; a route beyond them may be omitted. Direct
single-stage suggestions continue to scan the eligible converter catalog for
each retained incompatible pair. Ranking still examines the source/target
endpoint cross product, so its comparison time is O(source endpoints × target
endpoints). It retains only the requested top matches (capped at 1,000) while
ranking, rather than materializing that cross product. The planner connects
stages only when the declared directional schemas are compatible and delivery
modes agree, and skips paths that revisit a typed state. Similarity scores
never establish compatibility. Contract version 1 declares no converter cost
or losslessness, so plans make neither claim. They check interfaces only; an
applet may still reject particular values at runtime. The matcher does not
infer contracts or semantics from executable help text.

Diagnostics with a suggested repair carry a `fixes` array in `execute check --json`:
each fix has a message and `edits`, and each edit has a `span` and a `replacement`.
The language server offers each fix as a `quickfix` code action (`codeActionProvider`),
for diagnostics whose range overlaps the request. Current suggestions cover misspelled
parameter names, applet names, and statement keywords, and quoting an unknown applet
phrase as a direct executable.
