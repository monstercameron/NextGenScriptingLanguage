# Changelog

## 0.2.1

- Package the source grammar for native status, scalar conditions, JSON operations
  and same-file reusable programs, with complete validated snippets.
- Add repository, homepage and issue metadata; derive the VSIX filename from the
  package version.
- Validate snippet defaults with the current source runtime, or an explicit
  `EXECUTE_TEST_BINARY`, instead of the retained historical binary.
- Refresh installation and contributor instructions.

## 0.2.0

- Highlight top-level typed program inputs and defaults in Execute 0.8.0.
- Add a complete program-input template verified without supplying required inputs.

## 0.1.1

- Highlight direct quoted executable calls introduced in Execute 0.7.0, including native argument lists and result bindings.
- Add a complete `program-command` snippet validated by Execute 0.7.0.

## 0.1.0

- Recognize `.program` files as Execute Program.
- Highlight finite commands, native streams, services, and single-file TUI syntax.
- Add comment/bracket support, indentation folding, two-space defaults, and snippets.
- Verify scopes using the VS Code TextMate/Oniguruma engine and real project programs.
