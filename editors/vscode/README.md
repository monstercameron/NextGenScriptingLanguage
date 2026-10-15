# Execute .program for VS Code

Syntax highlighting and snippets for readable Execute workflows: native pipes,
typed inputs, JSON, conditions, reusable programs, services and terminal apps.
Open a `.program` file to activate highlighting automatically.

```text
run cat with path "contacts.txt"
pipe to tr with from "[:upper:]" and to "[:lower:]"
pipe to sort
pipe to uniq
then save to "contacts-clean.txt"
```

## Install

Build a source VSIX with the instructions below. In VS Code, choose
**Extensions: Install from VSIX…** and select `execute-program-0.2.2.vsix`, or use:

```sh
code --install-extension execute-program-0.2.2.vsix
```

Earlier installed VSIX files retain their original contents. Use the current
source package for current snippets. This extension has not been published to
the Marketplace; `execute-lang` is its local publisher identifier.

If a file opens as Plain Text, select **Execute Program** in the language-mode
picker. The language ID is `execute-program`; TextMate scope is `source.execute`.

## Included

- Typed CLI inputs and defaults, including JSON and text-list inputs.
- Context-aware token scopes for applet calls, bindings, JSON object/array/scalar
  values, comments, status, conditionals, loops, transfers, try/catch/finally,
  parallel/race task groups, typed graph declarations, and cache modifiers.
- Finite stdin, JSON selection, same-file reusable programs, listeners, schedules,
  persistent streams, typed applet graphs and implemented TUI vocabulary.
- Folding, `#` comments, list/object bracket pairing and two-space indentation defaults.
- Complete-program and statement snippets using prefixes such as `program-input`,
  `program-pipeline`, `program-condition`, `program-try`, `program-parallel`,
  `program-json-pointer`, `program-graph`, `program-listener` and `program-screen`.
- Standard scopes that inherit the active color theme.

A keyword-shaped applet stays an applet: `then false` highlights `false` as a
command. In strings, `#`, `$HOME`, backticks and `$(...)` remain literal text.
An unterminated string resets at the next line. Unsupported standalone forms such
as `else`, `for` and `loop` are marked; valid `while` headers and applets with any
of those names retain their ordinary scopes. Lexical highlighting does not check
whether a `break`, `continue` or `return` occurs in a valid semantic context.

## Scope

This declarative extension provides lexical highlighting and snippets. It has no
activation code, telemetry, command runner or language server. It does not execute
programs or load applet manifests, and highlighting is not semantic validation.
Use `execute check FILE.program` or `execute check FILE.program --json` for the
runtime's static checks. See the [language reference](https://github.com/monstercameron/NextGenScriptingLanguage/blob/main/docs/language.md).

## Develop and package

From the repository root, with stable Rust, Node.js 24 and npm:

```sh
cargo build --locked --release
npm --prefix editors/vscode ci --ignore-scripts --no-audit --no-fund
npm --prefix editors/vscode test
npm --prefix editors/vscode run package
```

Tests prefer `target/debug/execute` when it exists, otherwise select
`target/release/execute` (`execute.exe` on Windows), or use the explicit
`EXECUTE_TEST_BINARY` override. The retained historical `bin/execute` cannot
validate current manifests and is not a development default. A target-specific
build can be selected with the override.

The package command runs tests before building a VSIX with the official VS Code
packager. Its filename derives from `name` and `version` in `package.json`.
Update the extension's root package and lockfile versions together when changing
its source version; the runtime has its own version. Open this directory in VS
Code and press F5 for an Extension Development Host. The launch configuration
opens the CPU monitor source and does not execute it.

Tests use VS Code's `vscode-textmate` and `vscode-oniguruma` libraries. They check
scopes, line recovery, configuration, snippet expansion, the real `.program`
corpus, and complete snippet defaults against the actual runtime's `check`.
The repository's `make check` passes a fresh interpreter explicitly.

Implementation references: [syntax highlighting](https://code.visualstudio.com/api/language-extensions/syntax-highlight-guide)
and [language configuration](https://code.visualstudio.com/api/language-extensions/language-configuration-guide).

Licensed under the [MIT License](LICENSE).
