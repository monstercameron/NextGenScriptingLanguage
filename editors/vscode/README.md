# Execute .program for VS Code

Syntax highlighting and snippets for the Execute Unix glue language: native
pipelines, cached finite transformations, listeners, schedules, and terminal apps.
Open a `.program` file to activate highlighting automatically.

```text
run cat with path "contacts.txt"
pipe to tr with from "[:upper:]" and to "[:lower:]"
pipe to sort
pipe to uniq
then save to "contacts-clean.txt"
```

## Install

In VS Code, open the Command Palette, choose **Extensions: Install from VSIX…**,
and select `execute-program-0.2.0.vsix`. Or use the VS Code CLI:

```sh
code --install-extension execute-program-0.2.0.vsix
```

If a file is already open as Plain Text, choose **Execute Program** from the
language-mode picker in the bottom-right corner. The language ID is
`execute-program` and the TextMate scope is `source.execute`.

## Included

- Typed top-level program inputs and literal defaults, including text-list inputs.
- Context-aware highlighting for quoted direct executable names, applet names and multiword aliases, named
  parameters, result bindings, strings/JSON escapes, comments, booleans, integers,
  lists, cache modifiers, service triggers, and the implemented TUI vocabulary.
- `#` comment toggling, list bracket matching, quote/bracket pairing, indentation
  folding, two-space defaults, and indentation after event/component/widget headers.
- Source snippets cover finite stdin, conditions, JSON selection and same-file reuse
  as well as the released command/service/TUI forms. The packaged 0.2.0 extension
  predates these additions. Start with `program-input`, `program-command`, `program-pipeline`, `program-cache`,
  `program-listener`, `program-stream`, `program-interval`, `program-schedule`,
  `program-screen`, or `program-components`. Use **Snippets: Fill File with
  Snippet** for a complete program, or type a prefix and accept the suggestion.
- Standard token scopes that inherit your current color theme.

A keyword-shaped applet is still an applet: `then false` highlights `false` as a
command; `and cache false` highlights `cache` as a parameter and `false` as a
boolean. In strings, `#`, `$HOME`, backticks, and `$(...)` stay literal text.
Unterminated strings reset at the end of the line so one typo does not recolor
all subsequent statements.

## Scope

This is a declarative language extension. It has no executable activation code,
telemetry, command runner, or language-server dependency. It does not run your
programs or load applet manifests. Highlighting is lexical, not interpreter
validation or semantic completion; token colors depend on the active theme.
Use `execute check file.program` for runtime grammar/manifest validation.

The pending source grammar highlights scalar `if`/`otherwise` blocks, `is not`,
and native `and keep status as name` bindings. It adds a complete conditional
snippet. Installed VSIX 0.2.0 predates these changes.

`else`, `for`, `while`, and `loop` at statement starts are marked unsupported
because Execute 0.8.0 does not implement those control blocks. Applets named those
words can still be invoked explicitly with `run`.

## Develop and package

From this directory, with Node.js 22+ and npm:

```sh
npm ci --ignore-scripts --no-audit --no-fund
npm test
npm run package
```

The package command runs the tests before building the VSIX with the official
VS Code packager. Open this folder in VS Code and press F5 to preview the extension
in a separate Extension Development Host. The supplied launch configuration opens
the project's CPU monitor source; it does not execute the program.

Tests use `vscode-textmate` and `vscode-oniguruma`, the tokenization libraries used
by VS Code, rather than approximating the grammar with JavaScript regexes. They
check token scopes, line-state recovery, editor configuration, snippet expansion,
and the repository's real `.program` corpus. Complete snippet defaults are also
validated with `execute check` when the runtime is available in the checkout.
The repository's `make check` passes the freshly built source interpreter through
`EXECUTE_TEST_BINARY`; set that variable when testing against another build.

The implementation follows the official [syntax highlighting guide](https://code.visualstudio.com/api/language-extensions/syntax-highlight-guide)
and [language configuration guide](https://code.visualstudio.com/api/language-extensions/language-configuration-guide).
This package is distributed locally as a VSIX; it has not been published to the
Marketplace. `execute-lang` is its local publisher identifier.

MIT licensed.

The pending Execute source candidate adds `execute check FILE --json` for a
versioned static diagnostic result (`execute.check/1`). It shares validation
with ordinary `check` and does not execute applets. The extension remains a
syntax/snippet package; it does not automatically run the interpreter or expose
an editor diagnostics provider. Installed Execute 0.8.0 lacks this CLI option.
