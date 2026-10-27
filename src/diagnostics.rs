//! Structured static diagnostics for `.program` source.
//!
//! Spans use UTF-8 byte offsets in a half-open range and one-based Unicode-scalar
//! columns. A `line` span is intentionally broad when the parser only identifies
//! a line; token spans are emitted only when the token can be located reliably.

use crate::ast::{Connection, Invocation, Program, Step, StepKind, Value};
use crate::contracts::{AppletBackend, OutputType, ValueSchema};
use crate::manifest::{Manifest, Registry};
use crate::parser::{self, Token};
use crate::suggest;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// One diagnostic code that `execute check --json` can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeInfo {
    /// The stable code. Published codes never change text or meaning.
    pub code: &'static str,
    /// The stage that finds the problem: parse, resolve, type, policy or validate.
    pub family: &'static str,
    pub meaning: &'static str,
    /// Default severity emitted by static analysis for this code.
    pub severity: DiagnosticSeverity,
    /// A minimal source fragment that commonly produces this diagnostic.
    pub example: &'static str,
    /// The usual repair, stated for humans rather than as an edit.
    pub typical_fix: &'static str,
}

/// Every code `execute check --json` can emit. A code is never renamed. A new
/// meaning gets a new code, and the family is documented per code.
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: "check.duplicate_name",
        family: "validate",
        meaning: "a declaration, parameter, binding, or object repeats a name",
        example: "run \"true\" as result\nrun \"true\" as result",
        typical_fix: "Rename one binding so every result name is unique in its scope.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.scope",
        family: "validate",
        meaning: "a control-flow, job, declaration, or trigger is outside its valid scope or order",
        example: "break",
        typical_fix: "Move the statement into a valid loop, program, or job lifetime.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.program.unknown",
        family: "resolve",
        meaning: "a called reusable program is not defined",
        example: "run program workre",
        typical_fix: "Correct the program name or define the called program.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.program.file.unknown",
        family: "resolve",
        meaning: "a called program file path does not exist",
        example: "run program file \"missing.program\"",
        typical_fix: "Correct the path or restore the missing program file.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.program.cycle",
        family: "validate",
        meaning: "a reusable program call graph contains a recursive cycle",
        example: "define a program named worker\n  run program worker",
        typical_fix: "Remove the recursive call or restructure it without a cycle.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.program.input.unknown",
        family: "validate",
        meaning: "a reusable program call supplies an input it does not declare",
        example: "run program worker with typo \"value\"",
        typical_fix: "Rename the supplied input to one declared by the program.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.program.input.required",
        family: "validate",
        meaning: "a reusable program call omits a required input",
        example: "define a program named worker\n  accept input as text\n  use value input\nrun program worker",
        typical_fix: "Supply the required input, or declare a default value.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.range",
        family: "validate",
        meaning: "a literal is outside the supported range for its setting",
        example: "exit with status 300",
        typical_fix: "Choose an integer within the documented range.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.value.constraint",
        family: "validate",
        meaning: "a literal violates a declared value or input constraint",
        example: "accept color as text one of [\"red\"] with default \"blue\"",
        typical_fix: "Choose a value that satisfies the declaration's constraints.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.parameter.direct",
        family: "validate",
        meaning: "a direct executable call uses named applet parameters",
        example: "run \"grep\" with args [\"needle\"]",
        typical_fix: "Use the `arguments` parameter for the direct executable's argv list.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.bound_output_withheld",
        family: "validate",
        meaning: "a final bound value is internal; explicitly reuse it to produce program stdout",
        example: "run text.trim with text \" value \" as result",
        typical_fix: "Use an unbound `use value result` or encode the value to stdout.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "parse.lexical",
        family: "parse",
        meaning: "a line has an unterminated or invalid quoted or bracketed token",
        example: "run \"printf\" with arguments [\"missing quote]",
        typical_fix: "Close the string and use valid escapes and balanced brackets.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "parse.syntax",
        family: "parse",
        meaning: "a statement does not match the grammar",
        example: "rnu \"printf\" with arguments [\"x\"]",
        typical_fix: "Rewrite the statement using a supported language form.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "parse.indentation",
        family: "parse",
        meaning: "a line is indented with tabs, or its indentation does not nest",
        example: "\tuse value result",
        typical_fix: "Use spaces and indent the line under its containing block.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "parse.control",
        family: "parse",
        meaning: "a control construct is unsupported or nested beyond the supported depth",
        example: "try\n  try\n    run \"true\"",
        typical_fix: "Use a supported control construct and stay within its nesting limit.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "source.invisible_format",
        family: "validate",
        meaning: "source contains a Unicode format control that can change how it is displayed",
        example: "a source line containing U+200B ZERO WIDTH SPACE",
        typical_fix: "Remove the invisible Unicode format character.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.undefined_name",
        family: "validate",
        meaning: "a referenced result or input name is not defined at this point",
        example: "use value result",
        typical_fix: "Define the name first or correct it to an existing name.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.type_mismatch",
        family: "type",
        meaning: "statically known value types are incompatible at a use site",
        example: "if count equals \"many\"",
        typical_fix: "Convert the value or use operands with compatible types.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "check.diagnostics.truncated",
        family: "validate",
        meaning: "more diagnostics were found than the response limit permits",
        example: "a source file with more than 10,000 independent errors",
        typical_fix: "Fix the reported diagnostics, then check again for the remaining errors.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "applet.unknown",
        family: "resolve",
        meaning: "the operation names no declared applet and is not a quoted executable",
        example: "run text.trim_finall_lf with text \"x\"",
        typical_fix: "Correct the applet name or quote a direct executable name.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "exec.not_found",
        family: "resolve",
        meaning: "an explicitly resolved quoted executable was not found on PATH",
        example: "run \"prinft\" with arguments [\"x\"]",
        typical_fix: "Correct the executable name, install it on PATH, or omit --resolve for a portable check.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "applet.parameter.unknown",
        family: "resolve",
        meaning: "the applet declares no parameter with this name",
        example: "run fixture.reader with argumens \"x\"",
        typical_fix: "Rename the parameter to one declared by the applet.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "applet.parameter.type",
        family: "type",
        meaning: "a literal value does not match the parameter's declared type",
        example: "run fixture.reader with text 42",
        typical_fix: "Provide a value matching the parameter's declared type.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "applet.parameter.required",
        family: "validate",
        meaning: "a required parameter is missing from the call",
        example: "run fixture.reader",
        typical_fix: "Supply a value for the required parameter.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "applet.parameter.constraint",
        family: "validate",
        meaning: "a literal parameter value violates a statically known applet constraint",
        example: "run wait.until with probe \"tcp_port_open\" and host \"8.8.8.8\" and port 443 and timeout_ms 0",
        typical_fix: "Use a loopback IP address for the TCP port probe.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "applet.pipeline.incompatible",
        family: "type",
        meaning: "a pipeline stage's declared output does not satisfy the next stage's input",
        example: "then text.decode",
        typical_fix: "Add a conversion or connect stages with compatible schemas and encodings.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "data_flow_policy_not_configured",
        family: "policy",
        meaning: "a capability policy is set without a data-flow policy, so remote egress and persistence are unprotected",
        example: "execute check workflow.program --capability-policy allow.json",
        typical_fix: "Add a data-flow policy that grants the intended egress and persistence.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "check_failed",
        family: "validate",
        meaning: "validation failed without a static diagnostic; the causes list explains why",
        example: "a check failure that has no source-aware diagnostic yet",
        typical_fix: "Follow the causes and report any reproducible source case without a specific code.",
        severity: DiagnosticSeverity::Error,
    },
    CodeInfo {
        code: "lint.shell_expansion_literal",
        family: "lint",
        meaning: "shell-like expansion syntax in a literal argument or path is passed literally",
        example: "run \"echo\" with arguments [\"$HOME/path\"]",
        typical_fix: "Declare and use the environment value, or format a checked value explicitly.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "lint.executable_name_whitespace",
        family: "lint",
        meaning: "a quoted executable name contains shell metacharacters and is looked up as one filename",
        example: "run \"ls *.txt\"",
        typical_fix: "Put the executable in the command name and the remaining words in `arguments`.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "lint.empty_loop",
        family: "lint",
        meaning: "a loop is guaranteed to execute zero times",
        example: "repeat 0 times\n  run \"true\"",
        typical_fix: "Use a positive repeat count or remove the loop.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "lint.unused_binding",
        family: "lint",
        meaning: "a result binding is not read in its lexical program body",
        example: "run \"printf\" with arguments [\"x\"] as unused",
        typical_fix: "Remove the binding or use its value later in the same program body.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "lint.unused_input",
        family: "lint",
        meaning: "a declared input is not read in its lexical program body",
        example: "accept unused as text\nrun \"true\"",
        typical_fix: "Remove the input declaration or reference the input in the program body.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "model.result_unchecked",
        family: "lint",
        meaning: "a model result is consumed without checking its acceptance gate",
        example: "take text at \"/label\" from result as label",
        typical_fix: "Check `/accepted` before using the model label, decision, or score.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "model.threshold_missing",
        family: "lint",
        meaning: "a classification or scoring call omits its minimum confidence threshold",
        example: "run jev.classify with question \"route\" and labels [\"a\", \"b\"]",
        typical_fix: "Set a positive `minimum` appropriate to the workflow.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "model.text_to_command",
        family: "lint",
        meaning: "model-generated text is passed to a shell command interpreter",
        example: "run \"sh\" with arguments [\"-c\", generated_text]",
        typical_fix: "Keep model output as data and pass it only to a fixed, reviewed command.",
        severity: DiagnosticSeverity::Warning,
    },
    CodeInfo {
        code: "lint.unreachable",
        family: "lint",
        meaning: "a statement follows an unconditional exit in the same body",
        example: "exit with status 0\nrun \"true\"",
        typical_fix: "Remove the unreachable statement or move it before the exit.",
        severity: DiagnosticSeverity::Warning,
    },
];

/// The registry entry for a code, if the code is published.
pub fn code_info(code: &str) -> Option<&'static CodeInfo> {
    CODES.iter().find(|info| info.code == code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpanPrecision {
    Token,
    Line,
}

/// Stable source location. Byte offsets are UTF-8 offsets and end-exclusive;
/// line and column fields are one-based, with columns counting Unicode scalars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub precision: SpanPrecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    pub span: SourceSpan,
    /// Machine-applicable repairs, most likely first. Each fix is independent:
    /// applying one should resolve the diagnostic it is attached to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<SuggestedFix>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedInformation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedInformation {
    pub message: String,
    pub span: SourceSpan,
}

/// A repair described as text replacements over the checked source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedFix {
    pub message: String,
    pub edits: Vec<TextEdit>,
}

/// Replace the text covered by `span` with `replacement`. Span offsets are the same
/// UTF-8 byte offsets used by diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub span: SourceSpan,
    pub replacement: String,
}

impl SuggestedFix {
    fn replace(
        message: impl Into<String>,
        span: SourceSpan,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            edits: vec![TextEdit {
                span,
                replacement: replacement.into(),
            }],
        }
    }
}

impl Diagnostic {
    fn error(code: impl Into<String>, message: impl Into<String>, span: SourceSpan) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            code: code.into(),
            message: message.into(),
            span,
            fixes: Vec::new(),
            related: Vec::new(),
        }
    }
}

/// Parse and statically validate source, retaining independent diagnostics when
/// one malformed line does not prevent checking another line or invocation.
pub fn analyze_source(source: &str, registry: &Registry) -> Vec<Diagnostic> {
    analyze_source_at(source, None, registry)
}

/// Like `analyze_source`, for text read from `path`, so its program file calls
/// and location inputs resolve from that file's directory.
pub fn analyze_source_at(
    source: &str,
    path: Option<&std::path::Path>,
    registry: &Registry,
) -> Vec<Diagnostic> {
    analyze_source_impl(
        source,
        path,
        registry,
        #[cfg(feature = "lsp")]
        None,
        None,
    )
}

#[cfg(feature = "lsp")]
pub(crate) fn analyze_source_scoped<'a>(
    source: &str,
    path: Option<&std::path::Path>,
    registry: &'a Registry,
    scope: Option<&'a crate::program_files::SourceScope<'a>>,
) -> Vec<Diagnostic> {
    analyze_source_impl(source, path, registry, scope, None)
}

fn analyze_source_impl(
    source: &str,
    path: Option<&std::path::Path>,
    registry: &Registry,
    #[cfg(feature = "lsp")] scope: Option<&crate::program_files::SourceScope<'_>>,
    snapshot: Option<&crate::program_files::SourceLoadSnapshot>,
) -> Vec<Diagnostic> {
    const MAX_DIAGNOSTICS: usize = 10_000;
    let mut diagnostics = Vec::new();
    let mut local_error_lines = BTreeSet::new();

    let line_starts = LineStarts::new(source);
    for (offset, character) in source.char_indices() {
        // A leading byte-order mark is stripped by the parser, as run does, so it is
        // not an invisible character inside the program.
        if offset == 0 && character == '\u{feff}' {
            continue;
        }
        if is_invisible_format(character) {
            diagnostics.push(Diagnostic::error(
                "source.invisible_format",
                format!(
                    "source contains invisible format character U+{:04X}",
                    character as u32
                ),
                line_starts.span_at(
                    source,
                    offset,
                    offset + character.len_utf8(),
                    SpanPrecision::Token,
                ),
            ));
        }
    }
    for statement in parser::logical_lines(source).unwrap_or_default() {
        let line_number = statement.number;
        if let Some(tab) = indentation_tab(&statement.text) {
            diagnostics.push(Diagnostic::error(
                "parse.indentation",
                "tabs are not allowed for indentation",
                line_starts.span_at(
                    source,
                    line_starts.start(line_number) + tab,
                    line_starts.start(line_number) + tab + 1,
                    SpanPrecision::Token,
                ),
            ));
            local_error_lines.insert(line_number);
            continue;
        }

        let content = statement.text.as_str();
        let physical_line = source
            .lines()
            .nth(line_number.saturating_sub(1))
            .unwrap_or("");
        let bom_bytes = if line_number == 1 && physical_line.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        let physical_line = &physical_line[bom_bytes..];
        let content_source_offset = (parser::strip_comment(physical_line).trim_start_matches(' ')
            == content)
            .then(|| line_starts.start(line_number) + bom_bytes + statement.indent);
        let (tokens, token_ranges) = match parser::tokenize_with_ranges(content) {
            Ok(parsed) => parsed,
            Err(error) => {
                let span = error
                    .downcast_ref::<parser::LocatedParseError>()
                    .and_then(parser::LocatedParseError::source_range)
                    .and_then(|range| {
                        content_source_offset.map(|offset| {
                            line_starts.span_at(
                                source,
                                offset + range.start,
                                offset + range.end,
                                SpanPrecision::Token,
                            )
                        })
                    })
                    .unwrap_or_else(|| line_starts.span(source, line_number));
                diagnostics.push(Diagnostic::error(
                    "parse.lexical",
                    lexical_message(content),
                    span,
                ));
                local_error_lines.insert(line_number);
                continue;
            }
        };
        if tokens.is_empty() {
            continue;
        }
        let label = match parser::statement_label(&tokens, line_number) {
            Ok(label) => label,
            Err(error) => {
                let message = format!("{error:#}");
                diagnostics.push(Diagnostic::error(
                    syntax_code(&message),
                    message,
                    line_starts.span(source, line_number),
                ));
                local_error_lines.insert(line_number);
                continue;
            }
        };
        let (tokens, token_ranges) = if label.is_some() {
            (&tokens[1..], &token_ranges[1..])
        } else {
            (&tokens[..], &token_ranges[..])
        };
        if is_contextual_header(tokens) {
            continue;
        }
        if let Err(error) = parser::validate_statement(tokens) {
            let message = format!("{error:#}");
            let mut diagnostic =
                classified_parse_error(source, &message, line_number, &line_starts);
            let span = error
                .downcast_ref::<parser::LocatedParseError>()
                .and_then(parser::LocatedParseError::token_index)
                .and_then(|index| token_ranges.get(index))
                .and_then(|range| {
                    content_source_offset.map(|offset| {
                        line_starts.span_at(
                            source,
                            offset + range.start,
                            offset + range.end,
                            SpanPrecision::Token,
                        )
                    })
                })
                .unwrap_or_else(|| line_starts.span(source, line_number));
            diagnostic.span = span;
            let content_start = content_source_offset
                .unwrap_or_else(|| line_starts.start(line_number) + statement.indent);
            if let Some(first) = content.split_whitespace().next() {
                if let Some(keyword) = suggest::keyword_for(first) {
                    let span = line_starts.span_at(
                        source,
                        content_start,
                        content_start + first.len(),
                        SpanPrecision::Token,
                    );
                    diagnostic
                        .message
                        .push_str(&format!("; did you mean {keyword:?}?"));
                    diagnostic.fixes.push(SuggestedFix::replace(
                        format!("replace {first:?} with the keyword {keyword:?}"),
                        span,
                        keyword,
                    ));
                }
            }
            diagnostics.push(diagnostic);
            local_error_lines.insert(line_number);
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let access = crate::program_files::Access {
        registry,
        cwd: &cwd,
    };
    let replay_only = snapshot.is_some_and(crate::program_files::SourceLoadSnapshot::is_replay_only)
        || {
            #[cfg(feature = "lsp")]
            {
                scope.is_some_and(|scope| scope.is_replay_only())
            }
            #[cfg(not(feature = "lsp"))]
            {
                false
            }
        };
    let source_path = path.map(|path| {
        if replay_only {
            path.to_path_buf()
        } else {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
    });
    let mut captured_snapshot = None;
    #[cfg(feature = "lsp")]
    let parsed = if scope.is_some() {
        crate::program_files::parse_at_scoped(source, path, Some(access), scope)
    } else {
        let (parsed, loaded) =
            crate::program_files::parse_at_with_snapshot(source, path, Some(access), snapshot);
        captured_snapshot = Some(loaded);
        parsed
    };
    #[cfg(not(feature = "lsp"))]
    let (parsed, loaded) =
        crate::program_files::parse_at_with_snapshot(source, path, Some(access), snapshot);
    #[cfg(not(feature = "lsp"))]
    {
        captured_snapshot = Some(loaded);
    }
    match parsed {
        Ok(program) => {
            collect_lint_diagnostics(source, &program, &line_starts, &mut diagnostics);
            if let Some(step) = program
                .steps
                .last()
                .filter(|step| step.kind.program_stdout() == Some(false))
            {
                diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warning,
                    code: "check.bound_output_withheld".into(),
                    message: "the final bound value is internal and will not be printed; use an unbound `use value NAME` for text or `then encode NAME as json` to emit it".into(),
                    span: line_starts.span(source, step.line),
                    fixes: Vec::new(),
                    related: Vec::new(),
                });
            }
            let mut local_program = program.clone();
            local_program
                .definitions
                .retain(|name, _| !name.starts_with("file:"));
            collect_program_diagnostics(
                source,
                &local_program,
                registry,
                &line_starts,
                &mut diagnostics,
            );
            collect_pipeline_diagnostics(
                source,
                &local_program,
                registry,
                &line_starts,
                &mut diagnostics,
            );
            if let Some(parent_path) = path {
                #[cfg(feature = "lsp")]
                let replay_scope = scope.map(|scope| scope.replay());
                let replay_snapshot = captured_snapshot
                    .as_ref()
                    .map(crate::program_files::SourceLoadSnapshot::replay);
                let mut calls = Vec::new();
                collect_file_calls(&program.steps, &mut calls);
                for (name, definition) in &program.definitions {
                    if name.starts_with("file:") {
                        continue;
                    }
                    collect_file_calls(&definition.steps, &mut calls);
                }
                for (line, call) in calls
                    .iter()
                    .filter(|(_, call)| call.operation.starts_with("file:"))
                {
                    let key = &call.operation;
                    let Some(child_path) = key
                        .strip_prefix("file:")
                        .map(std::path::PathBuf::from)
                    else {
                        continue;
                    };
                    #[cfg(feature = "lsp")]
                    let child_source = scope
                        .and_then(|scope| scope.loaded_source(&child_path))
                        .or_else(|| {
                            captured_snapshot
                                .as_ref()
                                .and_then(|snapshot| snapshot.loaded_source(&child_path))
                        });
                    #[cfg(not(feature = "lsp"))]
                    let child_source = captured_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.loaded_source(&child_path));
                    let Some(child_source) = child_source else { continue; };
                    let mut child = analyze_child_source(
                        &child_source,
                        &child_path,
                        registry,
                        #[cfg(feature = "lsp")]
                        replay_scope.as_ref(),
                        replay_snapshot.as_ref(),
                    );
                    let parent_path = source_path.as_deref().unwrap_or(parent_path);
                    let mut callsite = line_starts.span(source, *line);
                    callsite.path = Some(parent_path.display().to_string());
                    callsite.path_bytes = Some(parent_path.as_os_str().as_encoded_bytes().to_vec());
                    for diagnostic in &mut child {
                        diagnostic.related.push(RelatedInformation {
                            message: "called from here".into(),
                            span: callsite.clone(),
                        });
                    }
                    diagnostics.append(&mut child);
                }
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            #[cfg(feature = "lsp")]
            let replay_scope = scope.map(|scope| scope.replay());
            let replay_snapshot = captured_snapshot
                .as_ref()
                .map(crate::program_files::SourceLoadSnapshot::replay);
            if let Some(path_text) = message
                .split_once("called program file ")
                .and_then(|(_, tail)| {
                    tail.split_once(" is not valid program text")
                        .map(|(path, _)| path)
                })
            {
                let child_path = std::path::PathBuf::from(path_text);
                #[cfg(feature = "lsp")]
                let child_source = scope
                    .and_then(|scope| scope.loaded_source(&child_path))
                    .or_else(|| {
                        captured_snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.loaded_source(&child_path))
                    });
                #[cfg(not(feature = "lsp"))]
                let child_source = captured_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.loaded_source(&child_path));
                if let (Some(parent_path), Some(child_source)) = (path, child_source) {
                    let mut child = analyze_child_source(
                        &child_source,
                        &child_path,
                        registry,
                        #[cfg(feature = "lsp")]
                        replay_scope.as_ref(),
                        replay_snapshot.as_ref(),
                    );
                    let name = child_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    let call_line = source
                        .lines()
                        .position(|line| line.contains(name))
                        .map_or(1, |line| line + 1);
                    let mut callsite = line_starts.span(source, call_line);
                    let parent_path = source_path.as_deref().unwrap_or(parent_path);
                    callsite.path = Some(parent_path.display().to_string());
                    callsite.path_bytes = Some(parent_path.as_os_str().as_encoded_bytes().to_vec());
                    for diagnostic in &mut child {
                        diagnostic.related.push(RelatedInformation {
                            message: "called from here".into(),
                            span: callsite.clone(),
                        });
                    }
                    diagnostics.append(&mut child);
                }
            }
            let error_line = find_line_number(&message).unwrap_or(1);
            let callee_parse_error = message.contains("called program file ")
                && message.contains(" is not valid program text");
            if !callee_parse_error && !local_error_lines.contains(&error_line) {
                diagnostics.push(classified_parse_error(
                    source,
                    &message,
                    error_line,
                    &line_starts,
                ));
            }
        }
    }

    diagnostics.retain(|diagnostic| {
        diagnostic.severity != DiagnosticSeverity::Warning
            || !warning_suppressed(source, diagnostic.span.line, &diagnostic.code)
    });
    diagnostics.sort_by(|left, right| {
        (&left.span.path, left.span.start_byte, &left.code, &left.message).cmp(&(
            &right.span.path,
            right.span.start_byte,
            &right.code,
            &right.message,
        ))
    });
    if let Some(path) = source_path.as_deref() {
        let display = path.display().to_string();
        let bytes = path.as_os_str().as_encoded_bytes().to_vec();
        for diagnostic in &mut diagnostics {
            if diagnostic.span.path.is_none() {
                diagnostic.span.path = Some(display.clone());
                diagnostic.span.path_bytes = Some(bytes.clone());
            }
            for fix in &mut diagnostic.fixes {
                for edit in &mut fix.edits {
                    if edit.span.path.is_none() {
                        edit.span.path = Some(display.clone());
                        edit.span.path_bytes = Some(bytes.clone());
                    }
                }
            }
        }
    }
    diagnostics.dedup_by(|left, right| {
        left.code == right.code
            && left.message == right.message
            && left.span.path == right.span.path
            && left.span.start_byte == right.span.start_byte
            && left.span.end_byte == right.span.end_byte
    });
    if diagnostics.len() > MAX_DIAGNOSTICS {
        let omitted = diagnostics.len() - (MAX_DIAGNOSTICS - 1);
        diagnostics.truncate(MAX_DIAGNOSTICS - 1);
        diagnostics.push(Diagnostic::error(
            "check.diagnostics.truncated",
            format!("diagnostic output limit reached; {omitted} additional diagnostics omitted"),
            line_starts.span_at(source, source.len(), source.len(), SpanPrecision::Line),
        ));
    }
    diagnostics
}

/// Parser errors include both grammar failures and semantic constraints checked
/// while building the AST. Keep those constraints out of the `parse.*` family.
fn classified_parse_error(
    source: &str,
    message: &str,
    line: usize,
    line_starts: &LineStarts,
) -> Diagnostic {
    let lower = message.to_ascii_lowercase();
    let code = if lower.contains("recursive program call")
        || lower.contains("program file calls form a cycle")
    {
        "check.program.cycle"
    } else if (lower.contains("duplicate")
        || lower.contains("more than once")
        || lower.contains("both set and removed"))
        && (lower.contains("label")
            || lower.contains("input")
            || lower.contains("program")
            || lower.contains("binding")
            || lower.contains("name")
            || lower.contains("identifier")
            || lower.contains("parameter")
            || lower.contains("object key")
            || lower.contains("choice")
            || lower.contains("environment"))
        || lower.contains("cannot match the group result binding")
    {
        "check.duplicate_name"
    } else if (lower.contains("only valid inside") || lower.contains("only valid in"))
        && (lower.contains("loop") || lower.contains("reusable program"))
        || lower.contains("must be awaited inside")
        || lower.contains("cannot be cancelled outside")
        || lower.contains("must follow only initial folder setup")
        || lower.contains("listener event header must follow")
        || lower.contains("on exit and on interrupt apply to one-shot programs")
        || lower.contains("input declarations must precede executable statements")
        || lower.contains("program definitions must precede executable statements")
        || lower.contains("program inputs must precede")
        || lower.contains("reusable stdin must be read at the start")
        || lower.contains("program definitions must be top-level")
        || lower.contains("the `execute n` version marker must be the first statement")
        || lower.contains("otherwise must follow a matching if body")
        || lower.contains("catch and finally must belong to a try block")
        || lower.contains("task groups require named applets")
        || lower.contains("task group bodies accept only independent")
        || lower.contains("task group entries")
        || lower.contains("background pipeline")
        || lower.contains("program calls cannot")
        || lower.contains("listener source must be an applet")
        || lower.contains("listener source cannot bind")
        || lower.contains("command condition takes only")
        || lower.contains("redirection and environment apply only")
        || lower.contains("stdin from` applies only")
        || lower.contains("applies only to") && !lower.contains("applies only to text inputs")
        || lower.contains("applies to one")
        || lower.contains("applies to a single")
        || lower.contains("cannot be combined")
    {
        "check.scope"
    } else if lower.contains("program file") && lower.contains("not found") {
        "check.program.file.unknown"
    } else if lower.contains("undefined program")
        || lower.contains("program") && lower.contains("not found")
    {
        "check.program.unknown"
    } else if lower.contains("input")
        && (lower.contains("not declared")
            || lower.contains("unknown input")
            || lower.contains("has no input"))
    {
        "check.program.input.unknown"
    } else if lower.contains("required input")
        || lower.contains("missing required input")
        || lower.contains("requires input")
    {
        "check.program.input.required"
    } else if lower.contains("outside the supported")
        || lower.contains("must be between")
        || lower.contains("must be a positive integer")
        || lower.contains("must be greater than zero")
        || lower.contains("interval is too large")
        || lower.contains("cannot contain more than")
        || lower.contains("backoff must be at most")
        || lower.contains("interval must be")
        || lower.contains("task limit") && lower.contains("range")
        || lower.contains("must be a positive integer in range")
        || lower.contains("status must be")
        || lower.contains("retry count") && lower.contains("range")
    {
        "check.range"
    } else if (lower.contains("default") && lower.contains("one of the allowed values"))
        || lower.contains("must be one of")
        || lower.contains("contains a nul byte")
        || lower.contains("question must be nonempty")
        || lower.contains("cron expression cannot be empty")
        || lower.contains("output path must be nonempty")
        || lower.contains("invalid environment name")
        || lower.contains("unsupported interval unit")
        || lower.contains("one of") && lower.contains("more than once")
        || lower.contains("program location inputs take their value")
    {
        "check.value.constraint"
    } else if lower.contains("applies only to text inputs")
        || lower.contains("program location inputs must be text")
        || lower.contains("secret prompt must be a text input")
        || lower.contains("from arguments` requires list of text")
        || lower.contains("always supplies a list; omit its default")
        || lower.contains("backoff must be a whole number")
        || lower.contains("expects")
        || lower.contains("requires true or false")
        || lower.contains("expects list of text")
        || lower.contains("expects text list items")
        || lower.contains("environment value") && lower.contains("must be")
        || lower.contains("flag") && lower.contains("must be a boolean input")
    {
        "check.type_mismatch"
    } else if lower.contains("direct executable") && lower.contains("parameter") {
        "check.parameter.direct"
    } else {
        syntax_code(message)
    };
    let line_span = line_starts.span(source, line);
    let words = line_words_from_start(source, line_span.start_byte);
    let semantic_match =
        |word: &&(String, usize, usize, bool)| semantic_token_matches(&word.0, &lower);
    let token = if code == "check.program.file.unknown" {
        words
            .windows(2)
            .find(|pair| pair[0].0 == "file" && pair[1].3)
            .map(|pair| &pair[1])
    } else if code == "check.duplicate_name" || code == "check.value.constraint" {
        words.iter().rev().find(semantic_match)
    } else {
        words.iter().find(semantic_match)
    }
    .or_else(|| {
        (code == "check.duplicate_name")
            .then(|| {
                words
                    .windows(2)
                    .rev()
                    .find(|pair| pair[0].0 == "as" && !pair[1].3)
                    .map(|pair| &pair[1])
            })
            .flatten()
    })
    .or_else(|| {
        (code == "check.scope" || code == "check.value.constraint")
            .then(|| words.iter().find(|word| !word.3))
            .flatten()
    })
    .or_else(|| {
        (code == "check.range")
            .then(|| {
                words
                    .iter()
                    .rev()
                    .find(|word| !word.3 && word.0.parse::<i128>().is_ok())
            })
            .flatten()
    });
    let span = token
        .map(|word| line_starts.span_at(source, word.1, word.2, SpanPrecision::Token))
        .unwrap_or(line_span);
    let mut diagnostic = Diagnostic::error(code, message, span.clone());
    if code == "check.program.unknown" && span.precision == SpanPrecision::Token {
        let names = parser::logical_lines(source)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|statement| {
                let tokens = parser::tokenize(&statement.text).ok()?;
                match tokens.as_slice() {
                    [
                        Token::Word("define"),
                        Token::Word("a"),
                        Token::Word("program"),
                        Token::Word("named"),
                        Token::Word(name),
                        ..
                    ] => Some((*name).to_owned()),
                    [Token::Word("define"), Token::Word(name), ..] => Some((*name).to_owned()),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        if let Some(suggestion) = suggest::closest(
            &source[span.start_byte..span.end_byte],
            names.iter().map(String::as_str),
        ) {
            diagnostic
                .message
                .push_str(&format!("; did you mean {suggestion:?}?"));
            diagnostic.fixes.push(SuggestedFix::replace(
                format!("replace the program name with {suggestion:?}"),
                span,
                suggestion,
            ));
        }
    }
    diagnostic
}

fn semantic_token_matches(token: &str, message: &str) -> bool {
    let token = token.trim_matches(['"', '`']).to_ascii_lowercase();
    [
        "break",
        "continue",
        "return",
        "await",
        "cancel",
        "status",
        "retry",
        "repeat",
        "interval",
        "limit",
        "position",
        "count",
        "times",
        "input",
        "default",
        "parameter",
        "key",
        "choice",
        "task",
        "direct",
        "retry",
        "interactively",
        "environment",
        "trigger",
        "one",
    ]
    .iter()
    .any(|candidate| token == *candidate && message.contains(candidate))
        || message.contains(&format!("{token:?}"))
        || message.contains(&format!("`{token}`"))
        || token.bytes().all(|byte| byte.is_ascii_digit()) && message.contains(&token)
}

fn warning_suppressed(source: &str, line: usize, code: &str) -> bool {
    let Some(line_text) = source.lines().nth(line.saturating_sub(1)) else {
        return false;
    };
    let bytes = line_text.as_bytes();
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b'#' {
            return line_text[index + 1..]
                .trim()
                .strip_prefix("execute-allow:")
                .is_some_and(|codes| {
                    codes
                        .split_ascii_whitespace()
                        .any(|allowed| allowed == code)
                });
        }
    }
    false
}

fn collect_lint_diagnostics(
    source: &str,
    program: &Program,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    collect_step_lints(source, &program.steps, line_starts, diagnostics);
    collect_model_lints(source, &program.steps, line_starts, diagnostics);
    let listener_binding = match program.trigger.as_ref() {
        Some(crate::ast::Trigger::Listen { binding, .. }) => Some(binding.as_str()),
        _ => None,
    };
    collect_unused_names(
        source,
        &program.inputs,
        &program.steps,
        line_starts,
        diagnostics,
        listener_binding,
    );
    for definition in program.definitions.values() {
        collect_step_lints(source, &definition.steps, line_starts, diagnostics);
        collect_model_lints(source, &definition.steps, line_starts, diagnostics);
        collect_unused_names(
            source,
            &definition.inputs,
            &definition.steps,
            line_starts,
            diagnostics,
            None,
        );
    }
    collect_path_literal_diagnostics(
        source,
        &program.steps,
        Some(program),
        line_starts,
        diagnostics,
    );
    for definition in program.definitions.values() {
        collect_path_literal_diagnostics(source, &definition.steps, None, line_starts, diagnostics);
    }
    let mut calls = Vec::new();
    collect_calls(&program.steps, &mut calls);
    for definition in program.definitions.values() {
        collect_calls(&definition.steps, &mut calls);
    }
    for (line, call) in calls {
        if call.direct
            && call
                .operation
                .chars()
                .any(|ch| matches!(ch, ' ' | '*' | '?' | '|' | ';'))
        {
            let span = line_words_from_start(source, line_starts.start(line))
                .into_iter()
                .find(|word| word.3 && word.0.trim_matches('"') == call.operation)
                .map(|word| line_starts.span_at(source, word.1, word.2, SpanPrecision::Token))
                .unwrap_or_else(|| line_starts.span(source, line));
            let mut diagnostic = Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "lint.executable_name_whitespace".into(),
                message: format!("quoted executable name {:?} is treated as one filename; split the executable from its arguments", call.operation),
                span,
                fixes: Vec::new(),
            };
            if !call.parameters.contains_key("arguments") {
                let mut words = call.operation.split_whitespace();
                if let Some(executable) = words.next() {
                    let arguments = words.map(str::to_owned).collect::<Vec<_>>();
                    let replacement = if arguments.is_empty() {
                        format!("\"{executable}\"")
                    } else {
                        format!(
                            "\"{executable}\" with arguments {}",
                            serde_json::to_string(&arguments).unwrap_or_default()
                        )
                    };
                    diagnostic.fixes.push(SuggestedFix::replace(
                        "split executable name and arguments",
                        diagnostic.span.clone(),
                        replacement,
                    ));
                }
            }
            diagnostics.push(diagnostic);
        }
        if let Some(Value::List(arguments)) = call.parameters.get("arguments") {
            for (index, argument) in arguments.iter().enumerate() {
                if call.direct
                    && shell_command_argument_index(&call.operation, arguments) == Some(index)
                {
                    continue;
                }
                let Value::String(value) = argument else {
                    continue;
                };
                if !has_shell_expansion(value) {
                    continue;
                }
                let span =
                    parameter_list_item_span_at(source, line, "arguments", index, line_starts)
                        .unwrap_or_else(|| {
                            parameter_value_span_at(source, line, "arguments", line_starts)
                                .unwrap_or_else(|| line_starts.span(source, line))
                        });
                let fixes = shell_expansion_argument_fix(
                    source,
                    program,
                    line,
                    call,
                    index,
                    value,
                    line_starts,
                )
                .into_iter()
                .collect();
                diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warning,
                    code: "lint.shell_expansion_literal".into(),
                    message: format!("argument {index} contains shell expansion syntax, but Execute passes it literally; declare an environment input or format the value explicitly"),
                    span,
                    fixes,
                });
            }
        }
    }
}

fn shell_command_argument_index(executable: &str, arguments: &[Value]) -> Option<usize> {
    fn executable_name(value: &str) -> &str {
        std::path::Path::new(value)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(value)
    }
    fn shell_name(value: &str) -> bool {
        matches!(
            executable_name(value),
            "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish"
        )
    }
    let executable = executable_name(executable);
    fn shell_script_index(shell: &str, arguments: &[Value]) -> Option<usize> {
        fn simple_option(shell: &str, flag: char) -> bool {
            match shell {
                "bash" => "abefhiklmnprstuvxB".contains(flag),
                "sh" | "dash" => "aefhkmnuvx".contains(flag),
                "zsh" => "aefhklmnuvx".contains(flag),
                "ksh" => "aefhkmnuvx".contains(flag),
                "fish" => "ilnpsqvx".contains(flag),
                _ => false,
            }
        }
        let mut index = 0;
        while let Some(value) = arguments.get(index) {
            let Value::String(argument) = value else {
                return None;
            };
            if argument == "-c" {
                return Some(index + 1);
            }
            if argument == "--" || !argument.starts_with('-') {
                return None;
            }
            if shell == "bash" && matches!(argument.as_str(), "-O" | "--rcfile" | "--init-file")
                || matches!(shell, "sh" | "bash" | "dash" | "zsh" | "ksh") && argument == "-o"
            {
                if !matches!(arguments.get(index + 1), Some(Value::String(_))) {
                    return None;
                }
                index += 2;
                continue;
            }
            let Some(flags) = argument.strip_prefix('-') else {
                return None;
            };
            let chars = flags.chars().collect::<Vec<_>>();
            if chars.last() == Some(&'c')
                && chars[..chars.len() - 1]
                    .iter()
                    .all(|flag| simple_option(shell, *flag))
            {
                return Some(index + 1);
            }
            if !chars.is_empty() && chars.iter().all(|flag| simple_option(shell, *flag)) {
                index += 1;
                continue;
            }
            return None;
        }
        None
    }
    fn env_command_index(arguments: &[Value]) -> Option<usize> {
        let mut index = 0;
        while let Some(Value::String(argument)) = arguments.get(index) {
            if argument == "--" {
                index += 1;
                break;
            }
            if argument.starts_with('-') {
                if matches!(argument.as_str(), "-S" | "--split-string") {
                    return None;
                }
                if matches!(argument.as_str(), "-u" | "--unset" | "-C" | "--chdir") {
                    index += 2;
                    continue;
                }
                if argument.starts_with("--unset=")
                    || argument.starts_with("--chdir=")
                    || argument.starts_with("--split-string=")
                    || matches!(
                        argument.as_str(),
                        "-i" | "--ignore-environment" | "-0" | "--null" | "--debug"
                    )
                {
                    index += 1;
                    continue;
                }
                return None;
            }
            let Some((name, _)) = argument.split_once('=') else {
                return Some(index);
            };
            if name.is_empty()
                || !name.chars().enumerate().all(|(position, ch)| {
                    ch == '_'
                        || ch.is_ascii_alphanumeric() && (position > 0 || !ch.is_ascii_digit())
                })
            {
                return Some(index);
            }
            index += 1;
        }
        arguments.get(index).and_then(|value| match value {
            Value::String(_) => Some(index),
            _ => None,
        })
    }
    if shell_name(executable) {
        return shell_script_index(executable, arguments);
    }
    let (child_index, child_args) = match executable {
        "env" => {
            let child_index = env_command_index(arguments)?;
            (child_index, &arguments[child_index + 1..])
        }
        "busybox" => {
            let applet_index =
                usize::from(matches!(arguments.first(), Some(Value::String(flag)) if flag == "--"));
            (applet_index, arguments.get(applet_index + 1..)?)
        }
        _ => return None,
    };
    let Value::String(child) = arguments.get(child_index)? else {
        return None;
    };
    shell_name(child)
        .then(|| {
            shell_script_index(executable_name(child), child_args)
                .map(|index| child_index + 1 + index)
        })
        .flatten()
}

fn shell_expansion_argument_fix(
    source: &str,
    program: &Program,
    line: usize,
    call: &Invocation,
    argument_index: usize,
    value: &str,
    line_starts: &LineStarts,
) -> Option<SuggestedFix> {
    let first = program.steps.first()?;
    if program.trigger.is_some()
        || !matches!(
            &first.kind,
            StepKind::Invoke {
                call: first_call,
                connection: Connection::Start,
            } if first.line == line && first_call.operation == call.operation
        )
        || source.starts_with('\u{feff}')
    {
        return None;
    }
    let line_text = source.lines().nth(line.saturating_sub(1))?;
    if line_text.trim_start() != line_text || !line_text.starts_with("run ") {
        return None;
    }
    let (template, variables) = shell_variable_template(value)?;
    let argument_span =
        parameter_list_item_span_at(source, line, "arguments", argument_index, line_starts)?;
    let insert_at = line_starts.start(line);
    let insertion_span = line_starts.span_at(source, insert_at, insert_at, SpanPrecision::Token);
    let mut declarations = Vec::new();
    let mut values = Vec::new();
    let mut variable_bindings = BTreeMap::new();
    for (index, variable) in variables.iter().enumerate() {
        let field = format!("v{index}");
        let binding = variable_bindings
            .entry(variable.clone())
            .or_insert_with(|| {
                fresh_binding_name(
                    source,
                    &format!("execute_{}", variable.to_ascii_lowercase()),
                )
            });
        if !declarations
            .iter()
            .any(|decl: &String| decl.contains(binding.as_str()))
        {
            declarations.push(format!(
                "accept {binding} as text from environment {} with default \"\"\n",
                serde_json::to_string(variable).ok()?
            ));
        }
        values.push(format!("\"{field}\": {binding}"));
    }
    let values = format!("{{{}}}", values.join(", "));
    let format_binding = fresh_binding_name(source, "execute_formatted_argument");
    let format_line = format!(
        "run text.format with template {} and values {values} and escape \"plain\" as {format_binding}\n",
        serde_json::to_string(&template).ok()?
    );
    let insertion = format!("{}{format_line}", declarations.concat());
    Some(SuggestedFix {
        message: "read the environment value explicitly and format the argument".into(),
        edits: vec![
            TextEdit {
                span: argument_span,
                replacement: format_binding,
            },
            TextEdit {
                span: insertion_span,
                replacement: insertion,
            },
        ],
    })
}

fn parameter_list_item_span_at(
    source: &str,
    line: usize,
    parameter: &str,
    item_index: usize,
    line_starts: &LineStarts,
) -> Option<SourceSpan> {
    let words = line_words_from_start(source, line_starts.start(line));
    let index = find_parameter_index(&words, parameter)?.checked_add(1 + item_index)?;
    let item = words.get(index)?;
    item.3
        .then(|| line_starts.span_at(source, item.1, item.2, SpanPrecision::Token))
}

fn fresh_binding_name(source: &str, base: &str) -> String {
    let used = |candidate: &str| {
        let mut offset = 0;
        for raw_line in source.split_inclusive('\n') {
            if line_words_from_start(source, offset)
                .iter()
                .any(|word| !word.3 && word.0 == candidate)
            {
                return true;
            }
            offset += raw_line.len();
        }
        false
    };
    if !used(base) {
        return base.to_owned();
    }
    (2..)
        .map(|suffix| format!("{base}_{suffix}"))
        .find(|candidate| !used(candidate))
        .unwrap_or_else(|| format!("{base}_generated"))
}

fn shell_variable_template(value: &str) -> Option<(String, Vec<String>)> {
    if value.contains("{{") || value.contains("}}") {
        return None;
    }
    let chars = value.char_indices().collect::<Vec<_>>();
    let mut output = String::new();
    let mut variables = Vec::new();
    let mut cursor = 0;
    let mut index = 0;
    while index < chars.len() {
        let (offset, ch) = chars[index];
        if ch == '\\' {
            // Backslash-escaped dollar/backtick is literal in the shell syntax
            // this lint is intended to recognize.
            if chars
                .get(index + 1)
                .is_some_and(|(_, next)| matches!(next, '$' | '`'))
            {
                output.push_str(&value[cursor..offset + ch.len_utf8()]);
                let (next_offset, next) = chars[index + 1];
                output.push(next);
                cursor = next_offset + next.len_utf8();
                index += 2;
                continue;
            }
        }
        if ch == '`' {
            return None;
        }
        if ch != '$' {
            index += 1;
            continue;
        }
        let start = index;
        let (variable, after) = if chars.get(index + 1).is_some_and(|(_, next)| *next == '{') {
            let close = (index + 2..chars.len()).find(|cursor| chars[*cursor].1 == '}')?;
            let variable = value[chars[index + 2].0..chars[close].0].to_owned();
            (variable, close + 1)
        } else {
            let next = chars.get(index + 1)?.1;
            if !(next == '_' || next.is_ascii_alphabetic()) {
                return None;
            }
            let mut end = index + 2;
            while end < chars.len() && (chars[end].1 == '_' || chars[end].1.is_ascii_alphanumeric())
            {
                end += 1;
            }
            (
                value[offset + 1..chars.get(end).map_or(value.len(), |(offset, _)| *offset)]
                    .to_owned(),
                end,
            )
        };
        if variable.is_empty()
            || !variable
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            return None;
        }
        output.push_str(&value[cursor..offset]);
        let placeholder =
            if let Some(existing) = variables.iter().position(|existing| existing == &variable) {
                existing
            } else {
                variables.push(variable);
                variables.len() - 1
            };
        output.push_str(&format!("{{{{v{placeholder}}}}}"));
        cursor = if after > start {
            chars.get(after).map_or(value.len(), |(offset, _)| *offset)
        } else {
            value.len()
        };
        index = after;
    }
    if variables.is_empty() {
        return None;
    }
    output.push_str(&value[cursor..]);
    Some((output, variables))
}

fn collect_path_literal_diagnostics(
    source: &str,
    steps: &[Step],
    program: Option<&Program>,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut calls = Vec::new();
    collect_calls(steps, &mut calls);
    for (line, call) in calls {
        for (name, value) in &call.parameters {
            if !matches!(
                name.as_str(),
                "path" | "file" | "directory" | "output" | "working_directory"
            ) {
                continue;
            }
            if matches!(value, Value::String(text) if has_shell_expansion(text)) {
                let fixes = program
                    .and_then(|program| match value {
                        Value::String(text) => shell_expansion_value_fix(
                            source,
                            program,
                            line,
                            call,
                            name,
                            None,
                            text,
                            line_starts,
                        ),
                        _ => None,
                    })
                    .into_iter()
                    .collect();
                diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warning,
                    code: "lint.shell_expansion_literal".into(),
                    message: format!("path parameter {name:?} contains shell expansion syntax, but Execute passes it literally; use an explicit environment input or format the path"),
                    span: parameter_value_span_at(source, line, name, line_starts).unwrap_or_else(|| line_starts.span(source, line)),
                    fixes,
                });
            }
        }
    }
    for step in steps {
        let path = match &step.kind {
            StepKind::UseFolder { path }
            | StepKind::Read { path, .. }
            | StepKind::Save { path, .. }
            | StepKind::Append { path } => Some(path),
            StepKind::LaunchDetached { output } => output.as_ref(),
            StepKind::Redirect { path, .. } => path.as_ref(),
            _ => None,
        };
        if matches!(path, Some(Value::String(text)) if has_shell_expansion(text)) {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "lint.shell_expansion_literal".into(),
                message: "path contains shell expansion syntax, but Execute treats it literally; use an explicit environment input or format the path".into(),
                span: line_starts.span(source, step.line),
                fixes: Vec::new(),
            });
        }
    }
}

fn shell_expansion_value_fix(
    source: &str,
    program: &Program,
    line: usize,
    call: &Invocation,
    parameter: &str,
    item_index: Option<usize>,
    value: &str,
    line_starts: &LineStarts,
) -> Option<SuggestedFix> {
    let first = program.steps.first()?;
    if program.trigger.is_some()
        || !matches!(
            &first.kind,
            StepKind::Invoke {
                call: first_call,
                connection: Connection::Start,
            } if first.line == line && first_call.operation == call.operation
        )
        || source.starts_with('\u{feff}')
    {
        return None;
    }
    let line_text = source.lines().nth(line.saturating_sub(1))?;
    if line_text.trim_start() != line_text || !line_text.starts_with("run ") {
        return None;
    }
    let (template, variables) = shell_variable_template(value)?;
    let span = match item_index {
        Some(index) => parameter_list_item_span_at(source, line, parameter, index, line_starts)?,
        None => parameter_value_span_at(source, line, parameter, line_starts)?,
    };
    let insert_at = line_starts.start(line);
    let insertion_span = line_starts.span_at(source, insert_at, insert_at, SpanPrecision::Token);
    let mut declarations = Vec::new();
    let mut values = Vec::new();
    let mut variable_bindings = BTreeMap::new();
    for (index, variable) in variables.iter().enumerate() {
        let field = format!("v{index}");
        let binding = variable_bindings
            .entry(variable.clone())
            .or_insert_with(|| {
                fresh_binding_name(
                    source,
                    &format!("execute_{}", variable.to_ascii_lowercase()),
                )
            });
        if !declarations
            .iter()
            .any(|decl: &String| decl.contains(binding.as_str()))
        {
            declarations.push(format!(
                "accept {binding} as text from environment {} with default \"\"\n",
                serde_json::to_string(variable).ok()?
            ));
        }
        values.push(format!("\"{field}\": {binding}"));
    }
    let values = format!("{{{}}}", values.join(", "));
    let format_binding = fresh_binding_name(source, "execute_formatted_path");
    let format_line = format!(
        "run text.format with template {} and values {values} and escape \"plain\" as {format_binding}\n",
        serde_json::to_string(&template).ok()?
    );
    Some(SuggestedFix {
        message: "read the environment value explicitly and format the path".into(),
        edits: vec![
            TextEdit {
                span,
                replacement: format_binding,
            },
            TextEdit {
                span: insertion_span,
                replacement: format!("{}{format_line}", declarations.concat()),
            },
        ],
    })
}

fn collect_unused_names(
    source: &str,
    inputs: &[crate::ast::InputDeclaration],
    steps: &[Step],
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
    implicit_listener_record: Option<&str>,
) {
    let mut reads = BTreeSet::new();
    collect_step_reads(steps, &mut reads);
    for input in inputs {
        if implicit_listener_record != Some(input.name.as_str()) && !reads.contains(&input.name) {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "lint.unused_input".into(),
                message: format!("input {:?} is declared but never read", input.name),
                span: binding_span(source, line_starts, input.line, &input.name),
                fixes: Vec::new(),
            });
        }
    }
    let mut bindings = Vec::<(String, usize)>::new();
    collect_step_bindings(steps, &mut bindings);
    for (name, line) in bindings {
        if implicit_listener_record != Some(name.as_str()) && !reads.contains(&name) {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "lint.unused_binding".into(),
                message: format!("binding {name:?} is never read"),
                span: binding_span(source, line_starts, line, &name),
                fixes: Vec::new(),
            });
        }
    }
}

fn binding_span(source: &str, line_starts: &LineStarts, line: usize, name: &str) -> SourceSpan {
    line_words_from_start(source, line_starts.start(line))
        .into_iter()
        .find(|word| !word.3 && word.0 == name)
        .map(|word| line_starts.span_at(source, word.1, word.2, SpanPrecision::Token))
        .unwrap_or_else(|| line_starts.span(source, line))
}

fn collect_step_bindings(steps: &[Step], bindings: &mut Vec<(String, usize)>) {
    let mut index = 0;
    while index < steps.len() {
        let step = &steps[index];
        // Treat each reusable program as its own scope, while preserving bindings
        // across control-flow blocks just as the checker does.
        let nested_end = match &step.kind {
            StepKind::Loop { end, .. }
            | StepKind::If { end, .. }
            | StepKind::IfCommand { end, .. }
            | StepKind::Try { end, .. }
            | StepKind::OnInterrupt { end }
            | StepKind::TempScope { end, .. } => Some(*end),
            _ => None,
        };
        if let Some(end) = nested_end.filter(|end| *end > 1 && index + *end <= steps.len()) {
            match &step.kind {
                StepKind::TempScope { binding, .. } => bindings.push((binding.clone(), step.line)),
                StepKind::Loop {
                    kind: crate::ast::LoopKind::ForEach { binding, .. },
                    ..
                } => bindings.push((binding.clone(), step.line)),
                _ => {}
            }
            if let StepKind::If { otherwise, .. } | StepKind::IfCommand { otherwise, .. } =
                &step.kind
            {
                let then_end = if *otherwise < end {
                    index + *otherwise
                } else {
                    index + end - 1
                };
                if index + 1 < then_end {
                    collect_step_bindings(&steps[index + 1..then_end], bindings);
                }
                if *otherwise < end {
                    let else_start = index + *otherwise + 1;
                    let else_end = index + end - 1;
                    if else_start < else_end {
                        collect_step_bindings(&steps[else_start..else_end], bindings);
                    }
                }
            } else if matches!(
                step.kind,
                StepKind::Loop { .. }
                    | StepKind::TempScope { .. }
                    | StepKind::Try { .. }
                    | StepKind::OnInterrupt { .. }
            ) {
                let body_end = index + end - 1;
                if index + 1 < body_end {
                    collect_step_bindings(&steps[index + 1..body_end], bindings);
                }
            }
            index += end;
            continue;
        }
        match &step.kind {
            StepKind::Invoke { call, .. } | StepKind::CallProgram { call, .. } => {
                add_call_binding_rows(call, step.line, bindings)
            }
            StepKind::TaskGroup {
                binding,
                tasks,
                each,
                ..
            } => {
                bindings.push((binding.clone(), step.line));
                if let Some(each) = each {
                    bindings.push((each.binding.clone(), step.line));
                }
                for call in tasks {
                    add_call_binding_rows(call, step.line, bindings);
                }
            }
            StepKind::SpawnJob { name, stages } => {
                bindings.push((name.clone(), step.line));
                for stage in stages {
                    add_call_binding_rows(&stage.call, step.line, bindings);
                }
            }
            StepKind::InspectJob { binding, .. }
            | StepKind::Catch { binding, .. }
            | StepKind::TempScope { binding, .. }
            | StepKind::ParseJson { binding }
            | StepKind::BindValue { binding, .. }
            | StepKind::TakeJson { binding, .. }
            | StepKind::TakeJsonDynamic { binding, .. }
            | StepKind::CaptureBytes { binding } => bindings.push((binding.clone(), step.line)),
            StepKind::Read { binding, .. }
            | StepKind::ReadStdin { binding }
            | StepKind::ReadCallInput { binding } => {
                bindings.extend(binding.iter().cloned().map(|name| (name, step.line)));
            }
            StepKind::AwaitJob { result_binding, .. } => {
                bindings.extend(result_binding.iter().cloned().map(|name| (name, step.line)));
            }
            StepKind::Loop {
                kind: crate::ast::LoopKind::ForEach { binding, .. },
                ..
            } => bindings.push((binding.clone(), step.line)),
            _ => {}
        }
        index += 1;
    }
}

fn add_call_binding_rows(call: &Invocation, line: usize, bindings: &mut Vec<(String, usize)>) {
    bindings.extend(
        call.binding
            .iter()
            .chain(call.result_binding.iter())
            .cloned()
            .map(|name| (name, line)),
    );
}

fn collect_step_reads(steps: &[Step], reads: &mut BTreeSet<String>) {
    fn value(item_value: &Value, reads: &mut BTreeSet<String>) {
        match item_value {
            Value::Reference(name) => {
                reads.insert(name.clone());
            }
            Value::List(items) => {
                for item in items {
                    value(item, reads);
                }
            }
            Value::Record(items) => {
                for item in items.values() {
                    value(item, reads);
                }
            }
            _ => {}
        }
    }
    fn call(call: &Invocation, reads: &mut BTreeSet<String>) {
        for parameter in call.parameters.values() {
            value(parameter, reads);
        }
    }
    for step in steps {
        match &step.kind {
            StepKind::Invoke {
                call: invocation, ..
            }
            | StepKind::CallProgram {
                call: invocation, ..
            } => call(invocation, reads),
            StepKind::IfCommand {
                call: invocation, ..
            } => call(invocation, reads),
            StepKind::TaskGroup { tasks, each, .. } => {
                for invocation in tasks {
                    call(invocation, reads);
                }
                if let Some(each) = each {
                    value(&each.values, reads);
                }
            }
            StepKind::SpawnJob { stages, .. } => {
                for stage in stages {
                    call(&stage.call, reads);
                }
            }
            StepKind::InspectJob { name, .. }
            | StepKind::AwaitJob { name, .. }
            | StepKind::CancelJob { name } => {
                reads.insert(name.clone());
            }
            StepKind::UseBytes { binding } => {
                reads.insert(binding.clone());
            }
            _ => {}
        }
        let mut values: Vec<&Value> = Vec::new();
        match &step.kind {
            StepKind::If { condition, .. } => values.extend([&condition.left, &condition.right]),
            StepKind::Loop { kind, .. } => match kind {
                crate::ast::LoopKind::Repeat { count } => values.push(count),
                crate::ast::LoopKind::ForEach { values: list, .. } => values.push(list),
                crate::ast::LoopKind::While { condition } => {
                    values.extend([&condition.left, &condition.right])
                }
                crate::ast::LoopKind::WhileCommand { .. } => {}
            },
            StepKind::Exit { status } => values.push(status),
            StepKind::Fail { message, status } => {
                values.push(message);
                values.extend(status.iter());
            }
            StepKind::UseFolder { path } | StepKind::Read { path, .. } => values.push(path),
            StepKind::UseValue { source }
            | StepKind::BindValue { source, .. }
            | StepKind::EncodeJson { source }
            | StepKind::TakeJson { source, .. } => values.push(source),
            StepKind::TakeJsonDynamic {
                pointer, source, ..
            } => values.extend([pointer, source]),
            StepKind::Save { path, .. } | StepKind::Append { path } => values.push(path),
            StepKind::Environment { set, .. } => values.extend(set.values()),
            StepKind::LaunchDetached { output: Some(path) } => values.push(path),
            StepKind::Redirect {
                path: Some(path), ..
            } => values.push(path),
            _ => {}
        }
        for item in values {
            value(item, reads);
        }
    }
}

fn collect_step_lints(
    source: &str,
    steps: &[Step],
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    collect_unreachable_lints(source, steps, line_starts, diagnostics);
    for step in steps {
        if let StepKind::Loop {
            kind:
                crate::ast::LoopKind::Repeat {
                    count: Value::Integer(0),
                },
            ..
        } = &step.kind
        {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "lint.empty_loop".into(),
                message: "repeat 0 times never executes its body".into(),
                span: line_starts.span(source, step.line),
                fixes: Vec::new(),
            });
        }
    }
}

fn collect_unreachable_lints(
    source: &str,
    steps: &[Step],
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut index = 0;
    let mut exited = false;
    while index < steps.len() {
        let step = &steps[index];
        let nested_end = match &step.kind {
            StepKind::Loop { end, .. }
            | StepKind::If { end, .. }
            | StepKind::IfCommand { end, .. }
            | StepKind::Try { end, .. }
            | StepKind::TempScope { end, .. } => Some(*end),
            _ => None,
        };
        if exited
            && !matches!(
                step.kind,
                StepKind::EndLoop { .. }
                    | StepKind::EndIf
                    | StepKind::Otherwise { .. }
                    | StepKind::EndCall
                    | StepKind::EndTry
                    | StepKind::EndTempScope { .. }
            )
        {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "lint.unreachable".into(),
                message:
                    "this statement follows an unconditional `exit` or `fail` in the same body"
                        .into(),
                span: line_starts.span(source, step.line),
                fixes: Vec::new(),
            });
        }
        if matches!(step.kind, StepKind::Exit { .. } | StepKind::Fail { .. }) {
            exited = true;
        }
        if let Some(end) = nested_end.filter(|end| *end > 1 && index + *end <= steps.len()) {
            if let StepKind::If { otherwise, .. } = &step.kind {
                let then_end = if *otherwise < end {
                    index + *otherwise
                } else {
                    index + end - 1
                };
                if index + 1 < then_end {
                    collect_unreachable_lints(
                        source,
                        &steps[index + 1..then_end],
                        line_starts,
                        diagnostics,
                    );
                }
                if *otherwise < end {
                    let else_start = index + *otherwise + 1;
                    let else_end = index + end - 1;
                    if else_start < else_end {
                        collect_unreachable_lints(
                            source,
                            &steps[else_start..else_end],
                            line_starts,
                            diagnostics,
                        );
                    }
                }
            } else if matches!(
                step.kind,
                StepKind::Loop { .. } | StepKind::TempScope { .. }
            ) {
                let body_end = index + end - 1;
                if index + 1 < body_end {
                    collect_unreachable_lints(
                        source,
                        &steps[index + 1..body_end],
                        line_starts,
                        diagnostics,
                    );
                }
            }
            // Try/catch/finally transfer semantics are intentionally skipped here.
            index += end;
        } else {
            index += 1;
        }
    }
}

fn collect_model_lints(
    source: &str,
    steps: &[Step],
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut calls = Vec::new();
    collect_calls(steps, &mut calls);
    let reads = {
        let mut reads = BTreeSet::new();
        collect_step_reads(steps, &mut reads);
        reads
    };
    for (line, call) in &calls {
        if matches!(
            call.operation.as_str(),
            "jev.classify" | "jev.judge" | "jev.score"
        ) && !call.parameters.contains_key("minimum")
        {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "model.threshold_missing".into(),
                message: format!(
                    "{} omits `minimum`, so its default confidence threshold is zero",
                    call.operation
                ),
                span: find_phrase_span_at(source, *line, &call.operation, line_starts)
                    .unwrap_or_else(|| line_starts.span(source, *line)),
                fixes: Vec::new(),
            });
        }
    }

    for (line, call) in calls.iter().filter(|(_, call)| {
        matches!(
            call.operation.as_str(),
            "jev.classify" | "jev.judge" | "jev.score"
        )
    }) {
        let Some(raw_name) = call.binding.as_ref() else {
            continue;
        };
        let mut result_names = BTreeSet::from([raw_name.clone()]);
        if let Some(parse_binding) = steps.iter().find_map(|step| {
            (step.line > *line)
                .then_some(&step.kind)
                .and_then(|kind| match kind {
                    StepKind::ParseJson { binding } => Some(binding.clone()),
                    _ => None,
                })
        }) {
            result_names.insert(parse_binding);
        }
        let mut accepted_bindings = BTreeMap::<String, usize>::new();
        let mut inspected_result_bindings = BTreeSet::new();
        for step in steps {
            if let StepKind::TakeJson {
                pointer,
                source: Value::Reference(source_name),
                binding,
                ..
            } = &step.kind
            {
                if step.line <= *line || !result_names.contains(source_name) {
                    continue;
                }
                if pointer == "/accepted" {
                    accepted_bindings.insert(binding.clone(), step.line);
                }
                if matches!(pointer.as_str(), "/label" | "/decision" | "/score")
                    && reads.contains(binding)
                {
                    inspected_result_bindings.insert(binding.clone());
                }
            }
        }
        if inspected_result_bindings.is_empty() {
            continue;
        }
        let acceptance_checked = !has_ungated_model_result_use(
            steps,
            &accepted_bindings,
            &inspected_result_bindings,
            *line,
        );
        if !acceptance_checked {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "model.result_unchecked".into(),
                message: format!(
                    "model result {raw_name:?} is consumed without testing its `/accepted` field"
                ),
                span: line_starts.span(source, *line),
                fixes: Vec::new(),
            });
        }
    }

    let mut generated_text = BTreeSet::<String>::new();
    for (_, call) in &calls {
        if call.operation == "llm.ask" {
            generated_text.extend(call.binding.iter().cloned());
            if let Some(raw) = call.binding.as_ref() {
                let parsed = steps.iter().find_map(|step| {
                    (step.line
                        > calls
                            .iter()
                            .find(|(_, call)| call.binding.as_ref() == Some(raw))
                            .map_or(0, |(line, _)| *line))
                    .then_some(&step.kind)
                    .and_then(|kind| match kind {
                        StepKind::ParseJson { binding } => Some(binding.clone()),
                        _ => None,
                    })
                });
                if let Some(parsed) = parsed {
                    generated_text.extend(steps.iter().filter_map(|step| match &step.kind {
                        StepKind::TakeJson {
                            pointer,
                            source: Value::Reference(source),
                            binding,
                            ..
                        } if source == &parsed && pointer == "/text" => Some(binding.clone()),
                        _ => None,
                    }));
                }
            }
        } else if call.operation == "llm.extract" {
            if let Some(raw) = call.binding.as_ref() {
                let parse_line = calls
                    .iter()
                    .find(|(_, candidate)| candidate.binding.as_ref() == Some(raw))
                    .map_or(0, |(line, _)| *line);
                let parsed = steps.iter().find_map(|step| {
                    (step.line > parse_line)
                        .then_some(&step.kind)
                        .and_then(|kind| match kind {
                            StepKind::ParseJson { binding } => Some(binding.clone()),
                            _ => None,
                        })
                });
                if let Some(parsed) = parsed {
                    generated_text.extend(steps.iter().filter_map(|step| match &step.kind {
                        StepKind::TakeJson {
                            pointer,
                            source: Value::Reference(source_name),
                            binding,
                            ..
                        } if source_name == &parsed
                            && matches!(pointer.as_str(), "/value" | "/text") =>
                        {
                            Some(binding.clone())
                        }
                        _ => None,
                    }));
                }
                for step in steps {
                    if let StepKind::TakeJson {
                        pointer,
                        source: Value::Reference(source_name),
                        binding,
                        ..
                    } = &step.kind
                    {
                        if source_name == raw && pointer == "/value" {
                            generated_text.insert(binding.clone());
                        }
                    }
                }
            }
        }
    }
    if generated_text.is_empty() {
        return;
    }
    for (line, call) in calls {
        if !call.direct {
            continue;
        }
        let Some(Value::List(arguments)) = call.parameters.get("arguments") else {
            continue;
        };
        let shell_command = shell_command_argument_index(&call.operation, arguments)
            .is_some_and(|index| {
                matches!(arguments.get(index), Some(Value::Reference(name)) if generated_text.contains(name))
            });
        if shell_command {
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                code: "model.text_to_command".into(),
                message: "model-generated text is passed as a shell command; keep it as data and use fixed, reviewed commands".into(),
                span: parameter_value_span_at(source, line, "arguments", line_starts).unwrap_or_else(|| line_starts.span(source, line)),
                fixes: Vec::new(),
            });
        }
    }
}

fn has_ungated_model_result_use(
    steps: &[Step],
    accepted_bindings: &BTreeMap<String, usize>,
    result_bindings: &BTreeSet<String>,
    model_line: usize,
) -> bool {
    fn positive_guard(
        condition: &crate::ast::Condition,
        accepted: &BTreeMap<String, usize>,
        line: usize,
        model_line: usize,
    ) -> bool {
        !condition.negated
            && condition.operator == crate::ast::ConditionOperator::Equals
            && accepted.iter().any(|(name, accepted_line)| {
                *accepted_line > model_line
                    && *accepted_line < line
                    && ((matches!(&condition.left, Value::Reference(value) if value == name)
                        && matches!(&condition.right, Value::Bool(true)))
                        || (matches!(&condition.right, Value::Reference(value) if value == name)
                            && matches!(&condition.left, Value::Bool(true))))
            })
    }

    fn scan(
        steps: &[Step],
        accepted: &BTreeMap<String, usize>,
        results: &BTreeSet<String>,
        model_line: usize,
    ) -> bool {
        let mut index = 0;
        while index < steps.len() {
            let step = &steps[index];
            let nested_end = match &step.kind {
                StepKind::Loop { end, .. }
                | StepKind::If { end, .. }
                | StepKind::IfCommand { end, .. }
                | StepKind::Try { end, .. }
                | StepKind::OnInterrupt { end }
                | StepKind::TempScope { end, .. } => Some(*end),
                _ => None,
            };
            if let Some(end) = nested_end.filter(|end| *end > 1 && index + *end <= steps.len()) {
                let mut header_reads = BTreeSet::new();
                collect_step_reads(&steps[index..index + 1], &mut header_reads);
                if header_reads.iter().any(|name| results.contains(name)) {
                    return true;
                }
                if let StepKind::If {
                    condition,
                    otherwise,
                    ..
                } = &step.kind
                {
                    let then_end = if *otherwise < end {
                        index + *otherwise
                    } else {
                        index + end - 1
                    };
                    let else_start = index + *otherwise + 1;
                    let else_end = index + end - 1;
                    if !(step.line > model_line
                        && positive_guard(condition, accepted, step.line, model_line))
                        && index + 1 < then_end
                        && scan(&steps[index + 1..then_end], accepted, results, model_line)
                    {
                        return true;
                    }
                    if *otherwise < end
                        && else_start < else_end
                        && scan(&steps[else_start..else_end], accepted, results, model_line)
                    {
                        return true;
                    }
                } else if let StepKind::IfCommand { otherwise, .. } = &step.kind {
                    let then_end = if *otherwise < end {
                        index + *otherwise
                    } else {
                        index + end - 1
                    };
                    let else_start = index + *otherwise + 1;
                    let else_end = index + end - 1;
                    if index + 1 < then_end
                        && scan(&steps[index + 1..then_end], accepted, results, model_line)
                    {
                        return true;
                    }
                    if *otherwise < end
                        && else_start < else_end
                        && scan(&steps[else_start..else_end], accepted, results, model_line)
                    {
                        return true;
                    }
                } else {
                    let body_end = index + end - 1;
                    if index + 1 < body_end
                        && scan(&steps[index + 1..body_end], accepted, results, model_line)
                    {
                        return true;
                    }
                }
                index += end;
                continue;
            }
            let mut reads = BTreeSet::new();
            collect_step_reads(&steps[index..index + 1], &mut reads);
            if reads.iter().any(|name| results.contains(name)) {
                return true;
            }
            index += 1;
        }
        false
    }

    scan(steps, accepted_bindings, result_bindings, model_line)
}

fn has_shell_expansion(value: &str) -> bool {
    let bytes = value.as_bytes();
    if value.starts_with('~')
        && !is_backslash_escaped(bytes, 0)
        && value
            .chars()
            .skip(1)
            .next()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '/')
    {
        return true;
    }
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' && !is_backslash_escaped(bytes, index) {
            if bytes
                .get(index + 1)
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                || matches!(bytes.get(index + 1), Some(b'{') | Some(b'('))
            {
                return true;
            }
        }
        if bytes[index] == b'`' && !is_backslash_escaped(bytes, index) {
            return true;
        }
        index += 1;
    }
    false
}

fn is_backslash_escaped(bytes: &[u8], index: usize) -> bool {
    let mut cursor = index;
    let mut count = 0;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        count += 1;
        cursor -= 1;
    }
    count % 2 == 1
}

/// Resolve quoted executable calls without launching them. This is opt-in because
/// PATH is host-specific and ordinary static checking remains portable.
pub fn resolve_executable_diagnostics(
    source: &str,
    path: Option<&std::path::Path>,
    registry: &Registry,
) -> Vec<Diagnostic> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let access = crate::program_files::Access {
        registry,
        cwd: &cwd,
    };
    let Ok(program) = crate::program_files::parse_at(source, path, Some(access)) else {
        return Vec::new();
    };
    let line_starts = LineStarts::new(source);
    let mut calls = Vec::new();
    collect_resolve_calls(&program.steps, &cwd, registry, &mut calls);
    for (name, definition) in program.definitions.iter() {
        // Called files keep their own source text and line numbers. This API
        // returns diagnostics against the caller source, so don't attach a child
        // executable warning or fix to a caller span.
        if name.starts_with(crate::parser::PROGRAM_FILE_PREFIX) {
            continue;
        }
        collect_resolve_calls(&definition.steps, &cwd, registry, &mut calls);
    }
    let path_value = std::env::var_os("PATH").unwrap_or_default();
    let path_note = path_value.to_string_lossy();
    let mut diagnostics = calls.into_iter().filter_map(|(line, call, call_cwd)| {
        if !call.direct || crate::runtime::binary_path(&call.operation, &call_cwd).is_ok() {
            return None;
        }
        let path_names = path_executable_names(&path_value, &call_cwd);
        let path_suggestion = crate::suggest::closest(
            &call.operation,
            path_names.iter().map(String::as_str),
        );
        let applet_suggestion = crate::suggest::closest(
            &call.operation,
            registry.applet_names().into_iter(),
        );
        let start = line_starts.start(line);
        let end = source[start..]
            .find('\n')
            .map_or(source.len(), |offset| start + offset);
        let quoted = format!("\"{}\"", call.operation);
        let span = source[start..end]
            .find(&quoted)
            .map(|offset| {
                let token_start = start + offset + 1;
                line_starts.span_at(
                    source,
                    token_start,
                    token_start + call.operation.len(),
                    SpanPrecision::Token,
                )
            })
            .unwrap_or_else(|| line_starts.span(source, line));
        let message = match (path_suggestion, applet_suggestion) {
            (Some(name), _) => format!(
                "executable {:?} was not found; did you mean {name:?}? (PATH used: {path_note})",
                call.operation
            ),
            (None, Some(name)) => format!(
                "executable {:?} was not found (PATH used: {path_note}); catalog applet {name:?} is available when called without quotes using its declared parameters",
                call.operation
            ),
            (None, None) => format!(
                "executable {:?} was not found (PATH used: {path_note})",
                call.operation
            ),
        };
        let mut diagnostic = Diagnostic {
            severity: DiagnosticSeverity::Warning,
            code: "exec.not_found".into(),
            message,
            span: span.clone(),
            fixes: Vec::new(),
        };
        if let Some(name) = path_suggestion {
            diagnostic.fixes.push(SuggestedFix::replace(
                format!("replace executable {:?} with {name:?}", call.operation),
                span,
                name,
            ));
        }
        Some(diagnostic)
    })
    .collect::<Vec<_>>();
    diagnostics.sort_by(|left, right| {
        (left.span.start_byte, &left.code, &left.message).cmp(&(
            right.span.start_byte,
            &right.code,
            &right.message,
        ))
    });
    diagnostics.dedup_by(|left, right| {
        left.code == right.code && left.span.start_byte == right.span.start_byte
    });
    diagnostics
}

fn path_executable_names(path: &std::ffi::OsStr, cwd: &std::path::Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for directory in std::env::split_paths(path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let executable = entry
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && crate::platform::executable(&metadata));
            if !executable {
                continue;
            }
            if let Some(name) = executable_candidate_name(&file_name) {
                names.insert(name);
            }
        }
    }
    names
}

fn executable_candidate_name(file_name: &std::ffi::OsStr) -> Option<String> {
    let name = file_name.to_str()?;
    if cfg!(windows) {
        let extension = std::path::Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("");
        if extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd") {
            return None;
        }
        if extension.eq_ignore_ascii_case("exe") {
            return std::path::Path::new(name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned);
        }
    }
    Some(name.to_owned())
}

fn collect_resolve_calls<'a>(
    steps: &'a [Step],
    start_cwd: &std::path::Path,
    registry: &Registry,
    calls: &mut Vec<(usize, &'a Invocation, std::path::PathBuf)>,
) {
    let mut cwd = Some(start_cwd.to_path_buf());
    let mut index = 0;
    while index < steps.len() {
        let step = &steps[index];
        match &step.kind {
            StepKind::UseFolder { path } => {
                if let Value::String(folder) = path {
                    let folder = std::path::Path::new(folder);
                    if let Some(current) = cwd.as_ref() {
                        let folder = if folder.is_absolute() {
                            folder.to_path_buf()
                        } else {
                            current.join(folder)
                        };
                        cwd = std::fs::canonicalize(folder).ok();
                    }
                } else {
                    cwd = None;
                }
            }
            StepKind::Invoke { call, .. } => {
                if let Some(current) = &cwd {
                    calls.push((step.line, call, current.clone()));
                }
                if registry
                    .resolve_call(call)
                    .is_ok_and(|manifest| manifest.builtin == Some(crate::manifest::Builtin::Cd))
                {
                    match call.parameters.get("path") {
                        Some(Value::String(folder)) => {
                            if let Some(current) = &cwd {
                                let folder = std::path::Path::new(folder);
                                let folder = if folder.is_absolute() {
                                    folder.to_path_buf()
                                } else {
                                    current.join(folder)
                                };
                                cwd = std::fs::canonicalize(folder).ok();
                            }
                        }
                        _ => cwd = None,
                    }
                }
            }
            StepKind::TaskGroup { tasks, .. } => {
                if let Some(current) = &cwd {
                    calls.extend(tasks.iter().map(|call| (step.line, call, current.clone())));
                }
            }
            StepKind::SpawnJob { stages, .. } => {
                if let Some(current) = &cwd {
                    calls.extend(
                        stages
                            .iter()
                            .map(|stage| (stage.line, &stage.call, current.clone())),
                    );
                }
            }
            StepKind::IfCommand { call, .. }
            | StepKind::Loop {
                kind: crate::ast::LoopKind::WhileCommand { call, .. },
                ..
            } => {
                if let Some(current) = &cwd {
                    calls.push((step.line, call, current.clone()));
                }
            }
            StepKind::CallProgram { call, end, .. } => {
                let finish = index.saturating_add(*end).min(steps.len());
                if !call
                    .operation
                    .starts_with(crate::parser::PROGRAM_FILE_PREFIX)
                {
                    if let (Some(current), true) = (&cwd, finish > index + 1) {
                        collect_resolve_calls(
                            &steps[index + 1..finish - 1],
                            current,
                            registry,
                            calls,
                        );
                    }
                }
                index = finish;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
}
fn analyze_child_source(
    source: &str,
    path: &std::path::Path,
    registry: &Registry,
    #[cfg(feature = "lsp")] scope: Option<&crate::program_files::SourceScope<'_>>,
    snapshot: Option<&crate::program_files::SourceLoadSnapshot>,
) -> Vec<Diagnostic> {
    analyze_source_impl(
        source,
        Some(path),
        registry,
        #[cfg(feature = "lsp")]
        scope,
        snapshot,
    )
}

fn is_contextual_header(tokens: &[Token<'_>]) -> bool {
    let word = |index: usize, expected: &str| matches!(tokens.get(index), Some(Token::Word(value)) if *value == expected);
    word(0, "define")
        || word(0, "accept")
        || word(0, "every")
        || (word(0, "on") && word(1, "schedule"))
        || (word(0, "listen") && word(1, "to"))
        || word(0, "whenever")
        || word(0, "if")
        || word(0, "repeat")
        || word(0, "for")
        || word(0, "while")
        || word(0, "parallel")
        || word(0, "race")
        || word(0, "spawn")
        || word(0, "inspect")
        || word(0, "await")
        || word(0, "cancel")
        || word(0, "otherwise")
        || word(0, "break")
        || word(0, "continue")
        || word(0, "return")
        || word(0, "try")
        || word(0, "catch")
        || word(0, "finally")
}

fn indentation_tab(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index] == b' ' {
        index += 1;
    }
    (bytes.get(index) == Some(&b'\t')).then_some(index)
}

fn lexical_message(line: &str) -> String {
    if line.contains('"') && !has_closed_quotes(line) {
        "unterminated quoted string".into()
    } else {
        "malformed quoted string or invalid escape".into()
    }
}

fn has_closed_quotes(line: &str) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for byte in line.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        }
    }
    !quoted
}

fn syntax_code(message: &str) -> &'static str {
    let message = message
        .split_once(": ")
        .map_or(message, |(_, message)| message);
    if message.starts_with("tabs are not allowed for indentation")
        || message.starts_with("inconsistent indentation")
    {
        "parse.indentation"
    } else if message.starts_with("unsupported control") || message.starts_with("control nesting") {
        "parse.control"
    } else {
        "parse.syntax"
    }
}

/// Return a line-precision span for a validation error tied to a source line.
pub fn source_line_span(source: &str, line: usize) -> SourceSpan {
    LineStarts::new(source).span(source, line)
}

struct LineStarts {
    starts: Vec<usize>,
    ends: Vec<usize>,
}

impl LineStarts {
    fn new(source: &str) -> Self {
        let mut starts =
            Vec::with_capacity(source.bytes().filter(|byte| *byte == b'\n').count() + 1);
        starts.push(0);
        starts.extend(source.match_indices('\n').map(|(offset, _)| offset + 1));
        let ends = starts
            .iter()
            .enumerate()
            .map(|(index, start)| {
                let mut end = starts.get(index + 1).map_or(source.len(), |next| next - 1);
                if end > *start && source.as_bytes()[end - 1] == b'\r' {
                    end -= 1;
                }
                end
            })
            .collect();
        Self { starts, ends }
    }

    fn start(&self, line: usize) -> usize {
        self.starts
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or_else(|| *self.starts.last().unwrap_or(&0))
    }

    fn span(&self, source: &str, line: usize) -> SourceSpan {
        let start = self.start(line);
        let end = self
            .ends
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or(start);
        self.span_at(source, start, end, SpanPrecision::Line)
    }

    fn span_at(
        &self,
        source: &str,
        start: usize,
        end: usize,
        precision: SpanPrecision,
    ) -> SourceSpan {
        let start = start.min(source.len());
        let end = end.max(start).min(source.len());
        let (line, column) = self.position(source, start);
        let (end_line, end_column) = self.position(source, end);
        SourceSpan {
            start_byte: start,
            end_byte: end,
            line,
            column,
            end_line,
            end_column,
            precision,
            path: None,
            path_bytes: None,
        }
    }

    fn position(&self, source: &str, offset: usize) -> (usize, usize) {
        let line_index = self
            .starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let start = self.starts[line_index];
        (line_index + 1, source[start..offset].chars().count() + 1)
    }
}

fn is_invisible_format(character: char) -> bool {
    matches!(character as u32,
        0x061c | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2069 | 0xfeff
    )
}

fn find_line_number(message: &str) -> Option<usize> {
    let bytes = message.as_bytes();
    let mut index = 0;
    while index + 5 <= bytes.len() {
        if &bytes[index..index + 5] == b"line " {
            let mut end = index + 5;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end > index + 5 && bytes.get(end) == Some(&b':') {
                if let Ok(line) = message[index + 5..end].parse() {
                    return Some(line);
                }
            }
        }
        index += 1;
    }
    None
}

fn collect_program_diagnostics(
    source: &str,
    program: &Program,
    registry: &Registry,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    collect_job_reference_diagnostics(source, &program.steps, line_starts, diagnostics);
    collect_duplicate_call_bindings(source, &program.steps, line_starts, diagnostics);
    collect_program_input_diagnostics(source, &program.steps, line_starts, diagnostics);
    for definition in program.definitions.values() {
        collect_job_reference_diagnostics(source, &definition.steps, line_starts, diagnostics);
        collect_duplicate_call_bindings(source, &definition.steps, line_starts, diagnostics);
        collect_program_input_diagnostics(source, &definition.steps, line_starts, diagnostics);
    }
    let mut calls = Vec::new();
    collect_calls(&program.steps, &mut calls);
    if let Some(crate::ast::Trigger::Listen {
        source: crate::ast::ListenSource::Applet(call),
        line,
        ..
    }) = &program.trigger
    {
        calls.push((*line, call));
    }
    let top_level_call_count = calls.len();
    for definition in program.definitions.values() {
        collect_calls(&definition.steps, &mut calls);
    }

    let mut known_names = program
        .inputs
        .iter()
        .map(|input| input.name.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    known_names.extend(declared_input_names(source));
    collect_non_call_bindings(&program.steps, &mut known_names);
    for definition in program.definitions.values() {
        known_names.extend(definition.inputs.iter().map(|input| input.name.clone()));
        collect_non_call_bindings(&definition.steps, &mut known_names);
    }
    let mut known_types = BTreeMap::<String, OutputType>::new();
    let mut ambiguous_types = BTreeSet::<String>::new();
    let mut applet_suggestions = BTreeMap::<String, Option<String>>::new();
    for input in &program.inputs {
        let kind = match input.kind {
            crate::ast::InputType::Text => Some(OutputType::Text),
            crate::ast::InputType::Integer => Some(OutputType::Integer),
            crate::ast::InputType::Boolean => Some(OutputType::Boolean),
            crate::ast::InputType::TextList => Some(OutputType::List),
            crate::ast::InputType::IntegerList | crate::ast::InputType::JsonList => {
                Some(OutputType::List)
            }
            crate::ast::InputType::Json => Some(OutputType::Json),
            crate::ast::InputType::Bytes => Some(OutputType::Bytes),
        };
        if let Some(kind) = kind {
            known_types.insert(input.name.clone(), kind);
        }
    }
    let mut list_item_types = BTreeMap::<String, OutputType>::new();
    for input in &program.inputs {
        let item_kind = match input.kind {
            crate::ast::InputType::TextList => Some(OutputType::Text),
            crate::ast::InputType::IntegerList => Some(OutputType::Integer),
            crate::ast::InputType::JsonList | crate::ast::InputType::Json => Some(OutputType::Json),
            _ => None,
        };
        if let Some(kind) = item_kind {
            list_item_types.insert(input.name.clone(), kind);
        }
    }
    let mut scoped_types = InferredTypeScope {
        known_types: known_types.clone(),
        list_item_types: list_item_types.clone(),
    };
    let mut call_type_scopes = HashMap::new();
    collect_scoped_call_types(
        &program.steps,
        registry,
        &mut scoped_types,
        &mut call_type_scopes,
        false,
    );
    let mut loop_scoped_lines = BTreeSet::new();
    collect_loop_scoped_lines(&program.steps, &mut loop_scoped_lines);
    let mut polymorphic_outputs = BTreeMap::<String, OutputType>::new();
    // Resolve list item contracts before checking call sites. A few bounded
    // passes allow an applet result to feed another list applet and then a loop.
    for _ in 0..top_level_call_count.saturating_add(1) {
        let mut changed = false;
        for (_, call) in calls.iter().take(top_level_call_count).copied() {
            if call_type_scopes
                .get(&(call as *const Invocation as usize))
                .is_some_and(|scope| scope.inside_loop)
            {
                continue;
            }
            let Some(binding) = &call.binding else {
                continue;
            };
            let Ok(manifest) = registry.resolve_call(call) else {
                continue;
            };
            let Some(output) = manifest
                .execution_contract()
                .and_then(|contract| contract.output.as_ref())
            else {
                continue;
            };
            let Some(parameter) = output.same_as_parameter.as_deref() else {
                continue;
            };
            let Some(value) = call.parameters.get(parameter) else {
                continue;
            };
            let Some(item_kind) = known_list_item_type(value, &known_types, &list_item_types)
            else {
                continue;
            };
            if output.kind == OutputType::List {
                let item_kind = runtime_list_item_type(item_kind);
                changed |= list_item_types.insert(binding.clone(), item_kind) != Some(item_kind);
            } else {
                let item_kind = runtime_json_item_type(item_kind);
                changed |=
                    polymorphic_outputs.insert(binding.clone(), item_kind) != Some(item_kind);
            }
        }
        for (name, kind) in &polymorphic_outputs {
            let before = known_types.get(name).copied();
            known_types.insert(name.clone(), *kind);
            changed |= before != Some(*kind);
        }
        if !changed {
            break;
        }
    }
    let mut top_level_types = known_types.clone();
    for step in &program.steps {
        if loop_scoped_lines.contains(&step.line) {
            continue;
        }
        if let StepKind::BindValue { source, binding } = &step.kind {
            if let Some(kind) = known_value_type(source, &known_types) {
                record_binding_type(&mut known_types, &mut ambiguous_types, binding, kind);
            }
        }
    }
    top_level_types.clone_from(&known_types);
    for (index, (line, call)) in calls.iter().copied().enumerate() {
        for value in call.parameters.values() {
            collect_undefined_references(
                source,
                line,
                value,
                &known_names,
                line_starts,
                diagnostics,
            );
        }
        if call.direct {
            for name in call.parameters.keys() {
                if name == "arguments" {
                    continue;
                }
                let span = find_parameter_span_at(source, line, name, line_starts)
                    .unwrap_or_else(|| line_starts.span(source, line));
                let mut diagnostic = Diagnostic::error(
                    "check.parameter.direct",
                    format!(
                        "direct executable {:?} does not accept parameter {name:?}; use `arguments` for its argument list",
                        call.operation
                    ),
                    span.clone(),
                );
                if name == "args" && span.precision == SpanPrecision::Token {
                    diagnostic.message.push_str("; did you mean \"arguments\"?");
                    diagnostic.fixes.push(SuggestedFix::replace(
                        "rename `args` to `arguments`",
                        span,
                        "arguments",
                    ));
                }
                diagnostics.push(diagnostic);
            }
            add_call_bindings(call, &mut known_names);
            continue;
        }
        let manifest = match registry.resolve_call(call) {
            Ok(manifest) => manifest,
            Err(_) => {
                let span = find_phrase_span_at(source, line, &call.operation, line_starts)
                    .unwrap_or_else(|| line_starts.span(source, line));
                diagnostics.push(unknown_applet_diagnostic(
                    source,
                    line,
                    call,
                    span,
                    registry,
                    line_starts,
                    &mut applet_suggestions,
                ));
                add_call_bindings(call, &mut known_names);
                continue;
            }
        };
        let empty_types = BTreeMap::new();
        let active_types = if index < top_level_call_count {
            call_type_scopes
                .get(&(call as *const Invocation as usize))
                .map(|scope| &scope.scope.known_types)
                .unwrap_or(&known_types)
        } else {
            &empty_types
        };
        validate_call(
            source,
            line,
            call,
            manifest,
            active_types,
            line_starts,
            diagnostics,
        );
        if index < top_level_call_count {
            if call_type_scopes
                .get(&(call as *const Invocation as usize))
                .is_some_and(|scope| scope.inside_loop)
            {
                add_call_bindings(call, &mut known_names);
                continue;
            }
            if let Some(binding) = &call.binding {
                if let Some(kind) = polymorphic_outputs
                    .get(binding)
                    .copied()
                    .or_else(|| manifest_output_type(manifest))
                {
                    record_binding_type(&mut known_types, &mut ambiguous_types, binding, kind);
                }
            }
            top_level_types.clone_from(&known_types);
        }
        add_call_bindings(call, &mut known_names);
    }
    // Aliases can name typed results learned while walking calls. Resolve those
    // types before checking their later uses (diagnostics are deduplicated below).
    for step in &program.steps {
        if loop_scoped_lines.contains(&step.line) {
            continue;
        }
        if let StepKind::BindValue { source, binding } = &step.kind {
            if let Some(kind) = known_value_type(source, &top_level_types) {
                record_binding_type(&mut top_level_types, &mut ambiguous_types, binding, kind);
            }
        }
    }
    for (line, call) in calls.iter().take(top_level_call_count).copied() {
        if call.direct {
            continue;
        }
        if let Ok(manifest) = registry.resolve_call(call) {
            validate_call(
                source,
                line,
                call,
                manifest,
                call_type_scopes
                    .get(&(call as *const Invocation as usize))
                    .map(|scope| &scope.scope.known_types)
                    .unwrap_or(&top_level_types),
                line_starts,
                diagnostics,
            );
        }
    }
    collect_condition_type_mismatches(
        source,
        &program.steps,
        &top_level_types,
        line_starts,
        diagnostics,
    );
    let mut step_names = program
        .inputs
        .iter()
        .map(|input| input.name.clone())
        .collect::<BTreeSet<_>>();
    step_names.extend(declared_input_names(source));
    collect_non_call_bindings(&program.steps, &mut step_names);
    for definition in program.definitions.values() {
        step_names.extend(definition.inputs.iter().map(|input| input.name.clone()));
        collect_non_call_bindings(&definition.steps, &mut step_names);
    }
    collect_step_value_references(
        source,
        &program.steps,
        &mut step_names,
        line_starts,
        diagnostics,
    );
    for definition in program.definitions.values() {
        let mut definition_names = definition
            .inputs
            .iter()
            .map(|input| input.name.clone())
            .collect::<BTreeSet<_>>();
        definition_names.extend(declared_input_names(source));
        collect_step_value_references(
            source,
            &definition.steps,
            &mut definition_names,
            line_starts,
            diagnostics,
        );
    }
    collect_step_type_mismatches(
        source,
        &program.steps,
        &top_level_types,
        line_starts,
        diagnostics,
    );
}

fn collect_job_reference_diagnostics(
    source: &str,
    steps: &[Step],
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut jobs = BTreeSet::new();
    for step in steps {
        let (reference, consume) = match &step.kind {
            StepKind::SpawnJob { name, .. } => {
                if !jobs.insert(name.as_str()) {
                    let span = line_words_from_start(source, line_starts.start(step.line))
                        .into_iter()
                        .find(|word| !word.3 && word.0 == name.as_str())
                        .map(|word| {
                            line_starts.span_at(source, word.1, word.2, SpanPrecision::Token)
                        })
                        .unwrap_or_else(|| line_starts.span(source, step.line));
                    diagnostics.push(Diagnostic::error(
                        "check.duplicate_name",
                        format!("job name {name:?} is spawned more than once"),
                        span,
                    ));
                }
                (None, false)
            }
            StepKind::AwaitJob { name, .. } | StepKind::CancelJob { name } => {
                (Some(name.as_str()), true)
            }
            StepKind::InspectJob { name, .. } => (Some(name.as_str()), false),
            _ => (None, false),
        };
        let Some(name) = reference else { continue };
        let known = if consume {
            jobs.remove(name)
        } else {
            jobs.contains(name)
        };
        if known {
            continue;
        }
        let span = line_words_from_start(source, line_starts.start(step.line))
            .into_iter()
            .find(|word| !word.3 && word.0 == name)
            .map(|word| line_starts.span_at(source, word.1, word.2, SpanPrecision::Token))
            .unwrap_or_else(|| line_starts.span(source, step.line));
        diagnostics.push(Diagnostic::error(
            "check.scope",
            format!("job {name:?} must name a currently running job created by a prior spawn"),
            span,
        ));
    }
}

fn collect_duplicate_call_bindings(
    source: &str,
    steps: &[Step],
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut seen = BTreeSet::new();
    for step in steps {
        let calls: Vec<&Invocation> = match &step.kind {
            StepKind::Invoke { call, .. } | StepKind::CallProgram { call, .. } => vec![call],
            StepKind::TaskGroup { tasks, .. } => tasks.iter().collect(),
            StepKind::SpawnJob { stages, .. } => stages.iter().map(|stage| &stage.call).collect(),
            _ => Vec::new(),
        };
        for call in calls {
            for name in call
                .binding
                .iter()
                .chain(call.status_binding.iter())
                .chain(call.result_binding.iter())
            {
                if seen.insert(name.as_str()) {
                    continue;
                }
                let words = line_words_from_start(source, line_starts.start(step.line));
                let span = words
                    .windows(2)
                    .find(|pair| !pair[1].3 && pair[0].0 == "as" && pair[1].0 == name.as_str())
                    .map(|pair| {
                        line_starts.span_at(source, pair[1].1, pair[1].2, SpanPrecision::Token)
                    })
                    .unwrap_or_else(|| line_starts.span(source, step.line));
                diagnostics.push(Diagnostic::error(
                    "check.duplicate_name",
                    format!("result name {name:?} is bound more than once"),
                    span,
                ));
            }
        }
    }
}

fn collect_program_input_diagnostics(
    source: &str,
    steps: &[Step],
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for step in steps {
        let StepKind::CallProgram { call, inputs, .. } = &step.kind else {
            continue;
        };
        for name in call.parameters.keys() {
            if inputs.iter().any(|input| input.name == *name) {
                continue;
            }
            let span = find_parameter_span_at(source, step.line, name, line_starts)
                .unwrap_or_else(|| line_starts.span(source, step.line));
            let mut diagnostic = Diagnostic::error(
                "check.program.input.unknown",
                format!("program {:?} has no input {name:?}", call.operation),
                span.clone(),
            );
            if let Some(suggestion) = suggest::closest(
                name,
                inputs
                    .iter()
                    .map(|input| input.name.as_str())
                    .filter(|candidate| !call.parameters.contains_key(*candidate)),
            ) {
                diagnostic
                    .message
                    .push_str(&format!("; did you mean {suggestion:?}?"));
                if span.precision == SpanPrecision::Token {
                    diagnostic.fixes.push(SuggestedFix::replace(
                        format!("rename input {name:?} to {suggestion:?}"),
                        span,
                        suggestion,
                    ));
                }
            }
            diagnostics.push(diagnostic);
        }
        for input in inputs {
            if call.parameters.contains_key(&input.name) || input.default.is_some() {
                continue;
            }
            let span = find_phrase_span_at(source, step.line, &call.operation, line_starts)
                .unwrap_or_else(|| line_starts.span(source, step.line));
            let mut diagnostic = Diagnostic::error(
                "check.program.input.required",
                format!(
                    "program {:?} requires input {:?}",
                    call.operation, input.name
                ),
                span,
            );
            if let Some(edit) = missing_program_input_edit(
                source,
                step.line,
                &input.name,
                input.kind,
                input.choices.as_deref(),
                !call.parameters.is_empty(),
            ) {
                diagnostic.fixes.push(SuggestedFix {
                    message: format!("supply required input {:?}", input.name),
                    edits: vec![edit],
                });
            }
            diagnostics.push(diagnostic);
        }
    }
}

fn missing_program_input_edit(
    source: &str,
    line: usize,
    name: &str,
    kind: crate::ast::InputType,
    choices: Option<&[String]>,
    already_has_inputs: bool,
) -> Option<TextEdit> {
    let starts = LineStarts::new(source);
    let line_start = starts.start(line);
    let line_end = starts.ends.get(line.saturating_sub(1)).copied()?;
    let raw = source.get(line_start..line_end)?;
    let statement = parser::logical_lines(source)
        .ok()?
        .into_iter()
        .find(|statement| statement.number == line)?;
    let bom = if line == 1 && raw.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let physical = parser::strip_comment(raw.get(bom..)?).trim_start_matches(' ');
    if physical != statement.text {
        return None;
    }
    let layout = parser::repair_invocation_layout(&statement.text).ok()?;
    let offset = line_start + bom + statement.indent + layout.parameter_insert;
    let placeholder = match kind {
        crate::ast::InputType::Text => match choices {
            Some([first, ..]) => serde_json::to_string(first).ok()?,
            Some([]) => return None,
            None => "\"\"".to_owned(),
        },
        crate::ast::InputType::Integer => "0".to_owned(),
        crate::ast::InputType::Boolean => "false".to_owned(),
        crate::ast::InputType::TextList
        | crate::ast::InputType::IntegerList
        | crate::ast::InputType::JsonList => "[]".to_owned(),
        crate::ast::InputType::Json => "null".to_owned(),
        crate::ast::InputType::Bytes => return None,
    };
    let join = if already_has_inputs {
        " and "
    } else {
        " with "
    };
    let replacement = format!("{join}{name} {placeholder}");
    let span = starts.span_at(source, offset, offset, SpanPrecision::Token);
    Some(TextEdit { span, replacement })
}

fn collect_non_call_bindings(steps: &[Step], known_names: &mut BTreeSet<String>) {
    for step in steps {
        match &step.kind {
            StepKind::Loop {
                kind: crate::ast::LoopKind::ForEach { binding, .. },
                ..
            }
            | StepKind::Catch { binding, .. }
            | StepKind::TempScope { binding, .. }
            | StepKind::ParseJson { binding }
            | StepKind::BindValue { binding, .. }
            | StepKind::TakeJson { binding, .. }
            | StepKind::TakeJsonDynamic { binding, .. }
            | StepKind::CaptureBytes { binding } => {
                known_names.insert(binding.clone());
            }
            StepKind::Read { binding, .. }
            | StepKind::ReadStdin { binding }
            | StepKind::ReadCallInput { binding } => {
                known_names.extend(binding.iter().cloned());
            }
            StepKind::TaskGroup { binding, each, .. } => {
                known_names.insert(binding.clone());
                known_names.extend(each.iter().map(|task| task.binding.clone()));
            }
            StepKind::InspectJob { binding, .. } => {
                known_names.insert(binding.clone());
            }
            StepKind::CallProgram { call, .. } => add_call_bindings(call, known_names),
            StepKind::AwaitJob {
                status_binding,
                result_binding,
                ..
            } => {
                known_names.extend(status_binding.iter().cloned());
                known_names.extend(result_binding.iter().cloned());
            }
            _ => {}
        }
    }
}

fn declared_input_names(source: &str) -> BTreeSet<String> {
    parser::logical_lines(source)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|statement| {
            let tokens = parser::tokenize(&statement.text).ok()?;
            match tokens.as_slice() {
                [Token::Word("accept"), Token::Word(name), ..] => Some((*name).to_owned()),
                _ => None,
            }
        })
        .collect()
}

fn collect_step_type_mismatches(
    source: &str,
    steps: &[Step],
    known_types: &BTreeMap<String, OutputType>,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for step in steps {
        let expected_values = match &step.kind {
            StepKind::UseValue { source } => vec![(source, &[OutputType::Text][..], "text input")],
            StepKind::Environment { set, .. } => set
                .values()
                .map(|value| (value, &[OutputType::Text][..], "environment text"))
                .collect(),
            StepKind::Loop { kind, .. } => match kind {
                crate::ast::LoopKind::Repeat { count } => {
                    vec![(count, &[OutputType::Integer][..], "integer repeat count")]
                }
                crate::ast::LoopKind::ForEach { values, .. } => {
                    vec![(
                        values,
                        &[OutputType::List, OutputType::Json][..],
                        "list iteration values",
                    )]
                }
                _ => Vec::new(),
            },
            StepKind::UseFolder { path }
            | StepKind::Read { path, .. }
            | StepKind::Save { path, .. }
            | StepKind::Append { path } => vec![(
                path,
                &[OutputType::Text, OutputType::Path][..],
                "text or path value",
            )],
            StepKind::TakeJsonDynamic { pointer, .. } => {
                vec![(pointer, &[OutputType::Text][..], "text JSON pointer")]
            }
            _ => Vec::new(),
        };
        for (value, expected, context) in expected_values {
            let Some(actual) = known_value_type(value, known_types) else {
                continue;
            };
            if expected.contains(&actual) {
                continue;
            }
            let Value::Reference(name) = value else {
                continue;
            };
            let span = line_words_from_start(source, line_starts.start(step.line))
                .into_iter()
                .find(|word| !word.3 && word.0 == *name)
                .map(|word| line_starts.span_at(source, word.1, word.2, SpanPrecision::Token));
            if let Some(span) = span {
                diagnostics.push(Diagnostic::error(
                    "check.type_mismatch",
                    format!(
                        "{name:?} has type {}; {context} has an incompatible type",
                        output_type_name(actual)
                    ),
                    span,
                ));
            }
        }
    }
}

fn collect_step_value_references(
    source: &str,
    steps: &[Step],
    known_names: &mut BTreeSet<String>,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for step in steps {
        let mut values = Vec::new();
        match &step.kind {
            StepKind::If { condition, .. } => {
                values.extend([&condition.left, &condition.right]);
            }
            StepKind::Loop { kind, .. } => match kind {
                crate::ast::LoopKind::Repeat { count } => values.push(count),
                crate::ast::LoopKind::ForEach { values: list, .. } => values.push(list),
                crate::ast::LoopKind::While { condition } => {
                    values.extend([&condition.left, &condition.right]);
                }
                crate::ast::LoopKind::WhileCommand { .. } => {}
            },
            StepKind::Exit { status } => values.push(status),
            StepKind::Fail { message, status } => {
                values.push(message);
                values.extend(status.iter());
            }
            StepKind::UseFolder { path } | StepKind::Read { path, .. } => values.push(path),
            StepKind::UseValue { source }
            | StepKind::BindValue { source, .. }
            | StepKind::EncodeJson { source } => values.push(source),
            StepKind::TakeJson { source, .. } => values.push(source),
            StepKind::TakeJsonDynamic {
                pointer, source, ..
            } => {
                values.extend([pointer, source]);
            }
            StepKind::Save { path, .. } | StepKind::Append { path } => values.push(path),
            StepKind::Environment { set, .. } => values.extend(set.values()),
            _ => {}
        }
        for value in values {
            collect_undefined_references(
                source,
                step.line,
                value,
                known_names,
                line_starts,
                diagnostics,
            );
        }
        match &step.kind {
            StepKind::Invoke { call, .. } => add_call_bindings(call, known_names),
            StepKind::TaskGroup {
                binding,
                tasks,
                each,
                ..
            } => {
                known_names.insert(binding.clone());
                if let Some(each) = each {
                    known_names.insert(each.binding.clone());
                }
                for call in tasks {
                    add_call_bindings(call, known_names);
                }
            }
            StepKind::SpawnJob { name, stages } => {
                known_names.insert(name.clone());
                for stage in stages {
                    add_call_bindings(&stage.call, known_names);
                }
            }
            StepKind::InspectJob { binding, .. } => {
                known_names.insert(binding.clone());
            }
            StepKind::AwaitJob {
                status_binding,
                result_binding,
                ..
            } => {
                known_names.extend(status_binding.iter().cloned());
                known_names.extend(result_binding.iter().cloned());
            }
            StepKind::Catch { binding, .. }
            | StepKind::TempScope { binding, .. }
            | StepKind::CaptureBytes { binding } => {
                known_names.insert(binding.clone());
            }
            StepKind::Read { binding, .. } | StepKind::ReadStdin { binding } => {
                known_names.extend(binding.iter().cloned());
            }
            StepKind::ReadCallInput { binding } => {
                known_names.extend(binding.iter().cloned());
            }
            StepKind::ParseJson { binding }
            | StepKind::BindValue { binding, .. }
            | StepKind::TakeJson { binding, .. }
            | StepKind::TakeJsonDynamic { binding, .. } => {
                known_names.insert(binding.clone());
            }
            StepKind::Loop {
                kind: crate::ast::LoopKind::ForEach { binding, .. },
                ..
            } => {
                known_names.insert(binding.clone());
            }
            StepKind::CallProgram { call, .. } => add_call_bindings(call, known_names),
            _ => {}
        }
    }
}

fn record_binding_type(
    known_types: &mut BTreeMap<String, OutputType>,
    ambiguous_types: &mut BTreeSet<String>,
    name: &str,
    kind: OutputType,
) {
    if ambiguous_types.contains(name) {
        return;
    }
    match known_types.get(name) {
        Some(previous) if *previous != kind => {
            known_types.remove(name);
            ambiguous_types.insert(name.to_owned());
        }
        _ => {
            known_types.insert(name.to_owned(), kind);
        }
    }
}

fn collect_condition_type_mismatches(
    source: &str,
    steps: &[Step],
    known_types: &BTreeMap<String, OutputType>,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for step in steps {
        let condition = match &step.kind {
            StepKind::If { condition, .. } => Some(condition),
            StepKind::Loop {
                kind: crate::ast::LoopKind::While { condition },
                ..
            } => Some(condition),
            _ => None,
        };
        if let Some(condition) = condition {
            let left = known_value_type(&condition.left, known_types);
            let right = known_value_type(&condition.right, known_types);
            if let (Some(left), Some(right)) = (left, right) {
                if !compatible_value_types(left, right) {
                    let reference = match (&condition.left, &condition.right) {
                        (Value::Reference(name), _) => Some(name.as_str()),
                        (_, Value::Reference(name)) => Some(name.as_str()),
                        _ => None,
                    };
                    if let Some(name) = reference {
                        if let Some(word) =
                            line_words_from_start(source, line_starts.start(step.line))
                                .into_iter()
                                .find(|word| !word.3 && word.0 == name)
                        {
                            diagnostics.push(Diagnostic::error(
                                "check.type_mismatch",
                                format!(
                                    "condition compares {} with {}; values must have the same type",
                                    output_type_name(left),
                                    output_type_name(right)
                                ),
                                line_starts.span_at(source, word.1, word.2, SpanPrecision::Token),
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn known_value_type(
    value: &Value,
    known_types: &BTreeMap<String, OutputType>,
) -> Option<OutputType> {
    match value {
        Value::Reference(name) => known_types.get(name).copied(),
        // Native text parameters and runtime text-list bindings cannot carry
        // NUL. Keep NUL-containing strings on the JSON side of the boundary,
        // including when they are first captured into a list binding.
        Value::String(text) if text.contains('\0') => Some(OutputType::Json),
        Value::String(_) => Some(OutputType::Text),
        Value::Integer(_) => Some(OutputType::Integer),
        Value::Number(_) => Some(OutputType::Number),
        Value::Bool(_) => Some(OutputType::Boolean),
        Value::List(_) => Some(OutputType::List),
        Value::Null | Value::Record(_) => Some(OutputType::Json),
    }
}

fn known_list_item_type(
    value: &Value,
    known_types: &BTreeMap<String, OutputType>,
    list_item_types: &BTreeMap<String, OutputType>,
) -> Option<OutputType> {
    match value {
        Value::Reference(name) => list_item_types.get(name).copied().or_else(|| {
            (known_types.get(name) == Some(&OutputType::Json)).then_some(OutputType::Json)
        }),
        Value::List(items) => {
            if items.is_empty() {
                return Some(OutputType::Json);
            }
            let Some(kinds) = items
                .iter()
                .map(|item| known_value_type(item, known_types))
                .collect::<Option<Vec<_>>>()
            else {
                return Some(OutputType::Json);
            };
            let first = kinds[0];
            Some(if kinds.iter().all(|kind| *kind == first) {
                first
            } else {
                OutputType::Json
            })
        }
        _ => None,
    }
}

/// Runtime list outputs retain precise item types for text and integer lists.
/// Other homogeneous JSON arrays remain conservatively JSON.
fn runtime_list_item_type(kind: OutputType) -> OutputType {
    match kind {
        OutputType::Text | OutputType::Integer => kind,
        _ => OutputType::Json,
    }
}

/// Match the runtime's scalar type for a JSON-valued applet result.
fn runtime_json_item_type(kind: OutputType) -> OutputType {
    match kind {
        OutputType::Number | OutputType::List | OutputType::Record => OutputType::Json,
        other => other,
    }
}

/// Match `runtime::loops::element_type`: text, integer, and boolean arrays
/// retain scalar precision; empty, mixed, floating-point, and nested arrays
/// are JSON values.
fn known_for_each_item_type(
    value: &Value,
    known_types: &BTreeMap<String, OutputType>,
    list_item_types: &BTreeMap<String, OutputType>,
) -> Option<OutputType> {
    let item_kind = |item: &Value| match item {
        Value::String(text) if !text.contains('\0') => OutputType::Text,
        Value::String(_) => OutputType::Json,
        Value::Integer(_) => OutputType::Integer,
        Value::Bool(_) => OutputType::Boolean,
        Value::Reference(name) => match known_types.get(name).copied() {
            Some(OutputType::Text) => OutputType::Text,
            Some(OutputType::Integer) => OutputType::Integer,
            Some(OutputType::Boolean) => OutputType::Boolean,
            None => OutputType::Text,
            _ => OutputType::Json,
        },
        _ => OutputType::Json,
    };
    match value {
        Value::Reference(name) => list_item_types
            .get(name)
            .copied()
            .map(runtime_list_item_type)
            .or_else(|| {
                (known_types.get(name) == Some(&OutputType::Json)).then_some(OutputType::Json)
            }),
        Value::List(items) if items.is_empty() => Some(OutputType::Json),
        Value::List(items) => {
            let first = item_kind(&items[0]);
            Some(if items.iter().all(|item| item_kind(item) == first) {
                first
            } else {
                OutputType::Json
            })
        }
        _ => None,
    }
}

#[derive(Clone, Default)]
struct InferredTypeScope {
    known_types: BTreeMap<String, OutputType>,
    list_item_types: BTreeMap<String, OutputType>,
}

#[derive(Clone)]
struct CallTypeScope {
    scope: InferredTypeScope,
    inside_loop: bool,
}

/// Infer call-site types while walking the flattened AST with lexical loop
/// scopes. Loop-local bindings and types are discarded after the body.
fn collect_scoped_call_types(
    steps: &[Step],
    registry: &Registry,
    scope: &mut InferredTypeScope,
    calls: &mut HashMap<usize, CallTypeScope>,
    inside_loop: bool,
) {
    let mut index = 0;
    while index < steps.len() {
        let step = &steps[index];
        match &step.kind {
            StepKind::Loop { kind, end } => {
                if let crate::ast::LoopKind::WhileCommand { call, .. } = kind {
                    calls.insert(
                        call as *const Invocation as usize,
                        CallTypeScope {
                            scope: scope.clone(),
                            inside_loop,
                        },
                    );
                }
                let finish = index.saturating_add(*end);
                if finish > index + 1 && finish <= steps.len() {
                    let mut body_scope = scope.clone();
                    if let crate::ast::LoopKind::ForEach { binding, values } = kind {
                        if let Some(kind) = known_for_each_item_type(
                            values,
                            &body_scope.known_types,
                            &body_scope.list_item_types,
                        ) {
                            body_scope.known_types.insert(binding.clone(), kind);
                        } else {
                            body_scope.known_types.remove(binding);
                        }
                        body_scope.list_item_types.remove(binding);
                    }
                    collect_scoped_call_types(
                        &steps[index + 1..finish - 1],
                        registry,
                        &mut body_scope,
                        calls,
                        true,
                    );
                    index = finish;
                    continue;
                }
            }
            StepKind::Invoke { call, .. } => {
                calls.insert(
                    call as *const Invocation as usize,
                    CallTypeScope {
                        scope: scope.clone(),
                        inside_loop,
                    },
                );
                infer_call_output_type(call, registry, scope);
                if let Some(binding) = &call.status_binding {
                    set_scoped_binding_type(scope, binding, OutputType::Integer);
                }
                if let Some(binding) = &call.result_binding {
                    set_scoped_binding_type(scope, binding, OutputType::Json);
                }
            }
            StepKind::IfCommand { call, .. } => {
                calls.insert(
                    call as *const Invocation as usize,
                    CallTypeScope {
                        scope: scope.clone(),
                        inside_loop,
                    },
                );
            }
            StepKind::TaskGroup {
                binding,
                tasks,
                each,
                ..
            } => {
                let mut task_scope = scope.clone();
                let task_inside_loop = each.is_some();
                if let Some(each) = each {
                    if let Some(kind) = known_for_each_item_type(
                        &each.values,
                        &task_scope.known_types,
                        &task_scope.list_item_types,
                    ) {
                        task_scope.known_types.insert(each.binding.clone(), kind);
                    } else {
                        task_scope.known_types.remove(&each.binding);
                    }
                    task_scope.list_item_types.remove(&each.binding);
                }
                for task in tasks {
                    calls.insert(
                        task as *const Invocation as usize,
                        CallTypeScope {
                            scope: task_scope.clone(),
                            inside_loop: inside_loop || task_inside_loop,
                        },
                    );
                    if !task_inside_loop {
                        infer_call_output_type(task, registry, scope);
                    }
                }
                set_scoped_binding_type(scope, binding, OutputType::Json);
            }
            StepKind::BindValue { source, binding } => {
                if let Some(kind) = known_value_type(source, &scope.known_types) {
                    scope.known_types.insert(binding.clone(), kind);
                    if let Value::Reference(name) = source {
                        if let Some(item_kind) = scope.list_item_types.get(name).copied() {
                            scope.list_item_types.insert(binding.clone(), item_kind);
                        }
                    } else if let Some(item_kind) =
                        known_list_item_type(source, &scope.known_types, &scope.list_item_types)
                    {
                        scope.list_item_types.insert(binding.clone(), item_kind);
                    }
                } else {
                    scope.known_types.remove(binding);
                    scope.list_item_types.remove(binding);
                }
            }
            StepKind::Read { binding, .. }
            | StepKind::ReadStdin { binding }
            | StepKind::ReadCallInput { binding } => {
                if let Some(binding) = binding {
                    set_scoped_binding_type(scope, binding, OutputType::Text);
                }
            }
            StepKind::TempScope { binding, .. } => {
                set_scoped_binding_type(scope, binding, OutputType::Text);
            }
            StepKind::ParseJson { binding }
            | StepKind::Catch { binding, .. }
            | StepKind::InspectJob { binding, .. } => {
                set_scoped_binding_type(scope, binding, OutputType::Json);
            }
            StepKind::TakeJson { kind, binding, .. }
            | StepKind::TakeJsonDynamic { kind, binding, .. } => {
                set_scoped_binding_type(scope, binding, output_type_for_input(*kind));
            }
            StepKind::CaptureBytes { binding } => {
                set_scoped_binding_type(scope, binding, OutputType::Bytes);
            }
            StepKind::AwaitJob {
                status_binding,
                result_binding,
                ..
            } => {
                if let Some(binding) = status_binding {
                    set_scoped_binding_type(scope, binding, OutputType::Integer);
                }
                if let Some(binding) = result_binding {
                    set_scoped_binding_type(scope, binding, OutputType::Json);
                }
            }
            StepKind::CallProgram { call, .. } => {
                if let Some(binding) = &call.binding {
                    set_scoped_binding_type(scope, binding, OutputType::Text);
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn set_scoped_binding_type(scope: &mut InferredTypeScope, binding: &str, kind: OutputType) {
    scope.known_types.insert(binding.to_owned(), kind);
    scope.list_item_types.remove(binding);
}

fn output_type_for_input(kind: crate::ast::InputType) -> OutputType {
    match kind {
        crate::ast::InputType::Text => OutputType::Text,
        crate::ast::InputType::Integer => OutputType::Integer,
        crate::ast::InputType::Boolean => OutputType::Boolean,
        crate::ast::InputType::TextList
        | crate::ast::InputType::IntegerList
        | crate::ast::InputType::JsonList => OutputType::List,
        crate::ast::InputType::Json => OutputType::Json,
        crate::ast::InputType::Bytes => OutputType::Bytes,
    }
}

fn infer_call_output_type(call: &Invocation, registry: &Registry, scope: &mut InferredTypeScope) {
    let Some(binding) = &call.binding else {
        return;
    };
    let Ok(manifest) = registry.resolve_call(call) else {
        return;
    };
    let output = manifest
        .execution_contract()
        .and_then(|contract| contract.output.as_ref());
    if let Some(output) = output {
        if let Some(parameter) = output.same_as_parameter.as_deref() {
            if let Some(value) = call.parameters.get(parameter) {
                if let Some(item_kind) =
                    known_list_item_type(value, &scope.known_types, &scope.list_item_types)
                {
                    if output.kind == OutputType::List {
                        scope
                            .list_item_types
                            .insert(binding.clone(), runtime_list_item_type(item_kind));
                    } else {
                        scope
                            .known_types
                            .insert(binding.clone(), runtime_json_item_type(item_kind));
                    }
                }
            }
        }
    }
    if output.is_some() {
        if let Some(kind) = manifest_output_type(manifest) {
            scope.known_types.insert(binding.clone(), kind);
        }
    }
}

fn collect_loop_scoped_lines(steps: &[Step], lines: &mut BTreeSet<usize>) {
    let mut index = 0;
    while index < steps.len() {
        let StepKind::Loop { end, .. } = &steps[index].kind else {
            index += 1;
            continue;
        };
        let finish = index.saturating_add(*end);
        if finish <= index + 1 || finish > steps.len() {
            index += 1;
            continue;
        }
        let body = &steps[index + 1..finish - 1];
        lines.extend(body.iter().map(|step| step.line));
        collect_loop_scoped_lines(body, lines);
        index = finish;
    }
}

fn compatible_value_types(left: OutputType, right: OutputType) -> bool {
    left == right
        || matches!(
            (left, right),
            (OutputType::Integer, OutputType::Number) | (OutputType::Number, OutputType::Integer)
        )
}

fn add_call_bindings(call: &Invocation, known_names: &mut BTreeSet<String>) {
    known_names.extend(call.binding.iter().cloned());
    known_names.extend(call.status_binding.iter().cloned());
    known_names.extend(call.result_binding.iter().cloned());
}

fn collect_undefined_references(
    source: &str,
    line: usize,
    value: &Value,
    known_names: &BTreeSet<String>,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match value {
        Value::Reference(name) if !known_names.contains(name) => {
            let Some(span) = line_words_from_start(source, line_starts.start(line))
                .into_iter()
                .find(|word| !word.3 && word.0 == *name)
                .map(|word| line_starts.span_at(source, word.1, word.2, SpanPrecision::Token))
            else {
                return;
            };
            let mut diagnostic = Diagnostic::error(
                "check.undefined_name",
                format!("undefined result {name:?}"),
                span.clone(),
            );
            if let Some(suggestion) = suggest::closest(name, known_names.iter().map(String::as_str))
            {
                diagnostic
                    .message
                    .push_str(&format!("; did you mean {suggestion:?}?"));
                diagnostic.fixes.push(SuggestedFix::replace(
                    format!("replace {name:?} with {suggestion:?}"),
                    span,
                    suggestion,
                ));
            }
            diagnostics.push(diagnostic);
        }
        Value::List(values) => {
            for value in values {
                collect_undefined_references(
                    source,
                    line,
                    value,
                    known_names,
                    line_starts,
                    diagnostics,
                );
            }
        }
        Value::Record(values) => {
            for value in values.values() {
                collect_undefined_references(
                    source,
                    line,
                    value,
                    known_names,
                    line_starts,
                    diagnostics,
                );
            }
        }
        _ => {}
    }
}

/// An unknown applet phrase, with at most one repair. Priority: a misspelled
/// statement keyword, then a near applet name, then quoting the phrase as a direct
/// executable. The quote repair needs a phrase of plain words and nothing else on
/// the line, so its replacement is exact.
fn unknown_applet_diagnostic(
    source: &str,
    line: usize,
    call: &Invocation,
    span: SourceSpan,
    registry: &Registry,
    line_starts: &LineStarts,
    applet_suggestions: &mut BTreeMap<String, Option<String>>,
) -> Diagnostic {
    let phrase = call.operation.as_str();
    let words = phrase.split_whitespace().collect::<Vec<_>>();
    let token = span.precision == SpanPrecision::Token;
    let mut message = format!(
        "unknown applet {phrase:?}; use a declared applet name or quote a direct executable"
    );
    let mut fix = None;

    if let (true, Some(first)) = (words.len() > 1 && token, words.first()) {
        if let Some(keyword) = suggest::keyword_for(first) {
            if source[span.start_byte..].starts_with(first) {
                let first_span = line_starts.span_at(
                    source,
                    span.start_byte,
                    span.start_byte + first.len(),
                    SpanPrecision::Token,
                );
                message = format!(
                    "unknown applet {phrase:?}; did you mean the statement keyword {keyword:?}?"
                );
                fix = Some(SuggestedFix::replace(
                    format!("replace {first:?} with {keyword:?}"),
                    first_span,
                    keyword,
                ));
            }
        }
    }

    if fix.is_none() && token {
        let suggestion = applet_suggestions
            .entry(phrase.to_owned())
            .or_insert_with(|| suggest::closest(phrase, registry.applet_names()).map(str::to_owned))
            .clone();
        if let Some(name) = suggestion {
            message = format!("unknown applet {phrase:?}; did you mean {name:?}?");
            fix = Some(SuggestedFix::replace(
                format!("use applet {name:?}"),
                span.clone(),
                name,
            ));
        }
    }

    if fix.is_none() && token {
        let rest = source[span.end_byte..].split('\n').next().unwrap_or("");
        let plain = source[span.start_byte..span.end_byte] == *phrase
            && !phrase.contains(['"', '\\'])
            && parser::strip_comment(rest).trim().is_empty();
        if let (true, Some(executable)) = (plain, words.first()) {
            let mut replacement = format!("\"{executable}\"");
            if words.len() > 1 {
                let arguments = words[1..]
                    .iter()
                    .map(|word| format!("\"{word}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                replacement.push_str(&format!(" with arguments [{arguments}]"));
            }
            let at_statement_start = source[line_starts.start(line)..span.start_byte]
                .trim()
                .is_empty();
            if at_statement_start {
                replacement = format!("run {replacement}");
            }
            message = format!(
                "unknown applet {phrase:?}; to run it as an executable, quote its name: {replacement}"
            );
            fix = Some(SuggestedFix::replace(
                format!("quote {executable:?} as a direct executable"),
                span.clone(),
                replacement,
            ));
        }
    }

    let mut diagnostic = Diagnostic::error("applet.unknown", message, span);
    diagnostic.fixes.extend(fix);
    diagnostic
}

fn collect_calls<'a>(steps: &'a [Step], calls: &mut Vec<(usize, &'a Invocation)>) {
    for step in steps {
        match &step.kind {
            StepKind::Invoke { call, .. } => calls.push((step.line, call)),
            StepKind::CallProgram { .. } => {}
            StepKind::TaskGroup { tasks, .. } => {
                calls.extend(tasks.iter().map(|call| (step.line, call)));
            }
            StepKind::SpawnJob { stages, .. } => {
                calls.extend(stages.iter().map(|stage| (stage.line, &stage.call)));
            }
            StepKind::IfCommand { call, .. }
            | StepKind::Loop {
                kind: crate::ast::LoopKind::WhileCommand { call, .. },
                ..
            } => calls.push((step.line, call)),
            _ => {}
        }
    }
}

fn collect_file_calls<'a>(steps: &'a [Step], calls: &mut Vec<(usize, &'a Invocation)>) {
    for step in steps {
        match &step.kind {
            StepKind::CallProgram { call, .. } => calls.push((step.line, call)),
            StepKind::TaskGroup { tasks, .. } => {
                calls.extend(tasks.iter().map(|call| (step.line, call)))
            }
            _ => {}
        }
    }
}

fn validate_call(
    source: &str,
    line: usize,
    call: &Invocation,
    manifest: &Manifest,
    known_types: &BTreeMap<String, OutputType>,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let description = manifest.contract();
    let properties = description
        .pointer("/parameters_schema/properties")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();
    let required = description
        .pointer("/parameters_schema/required")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValue::as_str)
        .collect::<BTreeSet<_>>();

    for (name, value) in &call.parameters {
        let Some(schema) = properties.get(name) else {
            let span = find_parameter_span_at(source, line, name, line_starts)
                .unwrap_or_else(|| line_starts.span(source, line));
            let mut diagnostic = Diagnostic::error(
                "applet.parameter.unknown",
                format!("applet {:?} has no parameter {name:?}", manifest.name),
                span.clone(),
            );
            if let Some(suggestion) = suggest::closest(
                name,
                properties
                    .keys()
                    .map(String::as_str)
                    .filter(|candidate| !call.parameters.contains_key(*candidate)),
            ) {
                diagnostic
                    .message
                    .push_str(&format!("; did you mean {suggestion:?}?"));
                if span.precision == SpanPrecision::Token {
                    diagnostic.fixes.push(SuggestedFix::replace(
                        format!("rename parameter {name:?} to {suggestion:?}"),
                        span,
                        suggestion,
                    ));
                }
            }
            diagnostics.push(diagnostic);
            continue;
        };
        if let Value::Reference(reference) = value {
            if let Some(kind) = known_types.get(reference) {
                if !output_type_matches_parameter(*kind, schema) {
                    diagnostics.push(Diagnostic::error(
                        "applet.parameter.type",
                        format!(
                            "parameter {name:?} of applet {:?} expects {}, but result {reference:?} is {}",
                            manifest.name,
                            schema_type(schema),
                            output_type_name(*kind),
                        ),
                        parameter_value_span_at(source, line, name, line_starts)
                            .unwrap_or_else(|| line_starts.span(source, line)),
                    ));
                }
            }
        } else if !value_matches_schema(value, schema) {
            diagnostics.push(Diagnostic::error(
                "applet.parameter.type",
                format!(
                    "parameter {name:?} of applet {:?} expects {}, but this value has type {}",
                    manifest.name,
                    schema_type(schema),
                    value_type(value),
                ),
                parameter_value_span_at(source, line, name, line_starts)
                    .unwrap_or_else(|| line_starts.span(source, line)),
            ));
        }
    }

    let dispatches_wait_until = (manifest.execution_backend() == AppletBackend::InProcess
        && manifest.name_for(call) == "wait.until")
        || manifest.execute_applet_operation() == Some("wait.until");
    if dispatches_wait_until
        && matches!(call.parameters.get("probe"), Some(Value::String(probe)) if probe == "tcp_port_open")
    {
        if let Some(Value::String(host)) = call.parameters.get("host") {
            if let Ok(ip) = host.parse::<IpAddr>() {
                if !ip.to_canonical().is_loopback() {
                    diagnostics.push(Diagnostic::error(
                        "applet.parameter.constraint",
                        "parameter \"host\" of applet \"wait.until\" must be a loopback IP address for probe \"tcp_port_open\"",
                        parameter_value_span_at(source, line, "host", line_starts)
                            .unwrap_or_else(|| line_starts.span(source, line)),
                    ));
                }
            }
        }
    }

    for name in required {
        if !call.parameters.contains_key(name) {
            diagnostics.push(Diagnostic::error(
                "applet.parameter.required",
                format!("applet {:?} requires parameter {name:?}", manifest.name),
                find_phrase_span_at(source, line, &call.operation, line_starts)
                    .unwrap_or_else(|| line_starts.span(source, line)),
            ));
        }
    }
}

fn value_matches_schema(value: &Value, schema: &JsonValue) -> bool {
    if let Some(variants) = schema.get("anyOf").and_then(JsonValue::as_array) {
        return variants
            .iter()
            .any(|variant| value_matches_schema(value, variant));
    }
    if let Some(variants) = schema.get("oneOf").and_then(JsonValue::as_array) {
        return variants
            .iter()
            .filter(|variant| value_matches_schema(value, variant))
            .count()
            == 1;
    }
    if let Some(kinds) = schema.get("type").and_then(JsonValue::as_array) {
        return kinds.iter().filter_map(JsonValue::as_str).any(|kind| {
            let mut variant = schema.clone();
            variant["type"] = JsonValue::String(kind.to_owned());
            value_matches_schema(value, &variant)
        });
    }
    let kind = schema.get("type").and_then(JsonValue::as_str).unwrap_or("");
    let matches_kind = match (kind, value) {
        ("string", Value::String(_)) => true,
        ("integer", Value::Integer(_)) => true,
        ("number", Value::Integer(_) | Value::Number(_)) => true,
        ("boolean", Value::Bool(_)) => true,
        ("array", Value::List(values)) => schema.get("items").is_none_or(|item_schema| {
            values.iter().all(|item| {
                matches!(item, Value::Reference(_)) || value_matches_schema(item, item_schema)
            })
        }),
        ("object", Value::Record(values)) => schema
            .get("properties")
            .and_then(JsonValue::as_object)
            .is_none_or(|fields| {
                values.iter().all(|(name, value)| {
                    fields
                        .get(name)
                        .is_some_and(|field_schema| value_matches_schema(value, field_schema))
                })
            }),
        ("null", Value::Null) => true,
        ("", _) => true,
        _ => false,
    };
    if !matches_kind {
        return false;
    }
    if let Value::Record(values) = value {
        if let Some(required) = schema.get("required").and_then(JsonValue::as_array) {
            if required
                .iter()
                .filter_map(JsonValue::as_str)
                .any(|name| !values.contains_key(name))
            {
                return false;
            }
        }
    }
    true
}

fn manifest_output_type(manifest: &Manifest) -> Option<OutputType> {
    let contract = manifest.contract();
    match contract.pointer("/io/output_schema/type")?.as_str()? {
        "bytes" => Some(OutputType::Bytes),
        "text" => Some(OutputType::Text),
        "path" => Some(OutputType::Path),
        "integer" => Some(OutputType::Integer),
        "number" => Some(OutputType::Number),
        "boolean" => Some(OutputType::Boolean),
        "json" => Some(OutputType::Json),
        "object" | "record" => Some(OutputType::Record),
        "array" | "list" => Some(OutputType::List),
        _ => None,
    }
}

fn output_type_matches_parameter(kind: OutputType, schema: &JsonValue) -> bool {
    let Some(types) = schema.get("type") else {
        return true;
    };
    let accepts = |name: &str| match types {
        JsonValue::String(expected) => expected == name,
        JsonValue::Array(expected) => expected.iter().any(|value| value.as_str() == Some(name)),
        _ => true,
    };
    match kind {
        OutputType::Bytes => accepts("bytes"),
        OutputType::Text | OutputType::Path => accepts("string"),
        OutputType::Integer => accepts("integer") || accepts("number"),
        OutputType::Number => accepts("number"),
        OutputType::Boolean => accepts("boolean"),
        OutputType::Record => accepts("object"),
        OutputType::List => accepts("array"),
        OutputType::Json => true,
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "text",
        Value::Integer(_) => "integer",
        Value::Bool(_) => "boolean",
        Value::List(_) => "list",
        Value::Record(_) => "record",
        Value::Null => "null",
        Value::Number(_) => "number",
        Value::Reference(_) => "reference",
    }
}

fn schema_type(schema: &JsonValue) -> String {
    if let Some(kinds) = schema.get("type").and_then(JsonValue::as_array) {
        return kinds
            .iter()
            .filter_map(JsonValue::as_str)
            .collect::<Vec<_>>()
            .join(" or ");
    }
    if let Some(variants) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(JsonValue::as_array)
    {
        let descriptions = variants.iter().map(schema_type).collect::<Vec<_>>();
        return descriptions.join(" or ");
    }
    schema
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("any")
        .to_owned()
}

fn collect_pipeline_diagnostics(
    source: &str,
    program: &Program,
    registry: &Registry,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    collect_pipeline_step_pairs(source, &program.steps, registry, line_starts, diagnostics);
    for definition in program.definitions.values() {
        collect_pipeline_step_pairs(
            source,
            &definition.steps,
            registry,
            line_starts,
            diagnostics,
        );
    }
}

fn collect_pipeline_step_pairs(
    source: &str,
    steps: &[Step],
    registry: &Registry,
    line_starts: &LineStarts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for pair in steps.windows(2) {
        let (
            StepKind::Invoke { call: producer, .. },
            StepKind::Invoke {
                call: consumer,
                connection: Connection::Pipe,
            },
        ) = (&pair[0].kind, &pair[1].kind)
        else {
            continue;
        };
        if producer.direct || consumer.direct {
            continue;
        }
        let (Ok(producer_manifest), Ok(consumer_manifest)) = (
            registry.resolve_call(producer),
            registry.resolve_call(consumer),
        ) else {
            continue;
        };
        if producer_manifest.execution_contract().is_none() {
            continue;
        }
        if consumer_manifest
            .ports()
            .and_then(|ports| ports.inputs.get("stdin"))
            .is_none()
        {
            continue;
        }
        let reasons = producer_manifest.native_pipe_compatibility_issues(consumer_manifest);
        if reasons.is_empty() {
            continue;
        }
        diagnostics.push(Diagnostic::error(
            "applet.pipeline.incompatible",
            format!(
                "pipeline connects {:?} stdout to {:?} stdin, but their declared schemas or encodings differ: {}",
                producer_manifest.name,
                consumer_manifest.name,
                reasons.join("; "),
            ),
            line_starts.span(source, pair[1].line),
        ));
    }
}

/// Explain why values from a declared output cannot flow into an input schema
/// without an explicit conversion. This compares semantic values only; native
/// pipe callers must also compare the producer and consumer wire encodings.
pub fn schema_mismatch_reasons(
    output: &ValueSchema,
    input: &ValueSchema,
    output_name: &str,
    input_name: &str,
) -> Vec<String> {
    let mut reasons = Vec::new();
    compare_schema(
        output,
        input,
        &format!("{output_name} → {input_name}"),
        &mut reasons,
    );
    reasons
}

/// Whether values described by `output` can be supplied to `input` without
/// conversion. This does not compare byte framing or output encodings. A
/// nonnullable producer safely satisfies a nullable input; a nullable producer
/// cannot satisfy an input that rejects null.
pub fn schemas_compatible(output: &ValueSchema, input: &ValueSchema) -> bool {
    schema_mismatch_reasons(output, input, "output", "input").is_empty()
}

fn compare_schema(
    output: &ValueSchema,
    input: &ValueSchema,
    path: &str,
    reasons: &mut Vec<String>,
) {
    if output.kind != input.kind {
        if !output.is_assignable_to(input) {
            reasons.push(format!(
                "{path} is {}, expected {}",
                output_type_name(output.kind),
                output_type_name(input.kind)
            ));
        }
        return;
    }
    // `Json` always admits JSON null; nullable adds meaning only to the typed
    // JSON value schemas. This follows ValueSchema::validate_value exactly.
    let output_may_be_null = output.nullable || output.kind == OutputType::Json;
    let input_accepts_null = input.nullable || input.kind == OutputType::Json;
    if output_may_be_null && !input_accepts_null {
        reasons.push(format!(
            "{path} may be null, but the input does not accept null"
        ));
    }
    if output.kind == OutputType::Text
        && !enum_constraint_assignable(output.enum_values.as_deref(), input.enum_values.as_deref())
    {
        reasons.push(format!(
            "{path} does not guarantee one of the input's allowed enum values"
        ));
    }
    match output.kind {
        OutputType::List => match (&output.items, &input.items) {
            (Some(output_items), Some(input_items)) => {
                compare_schema(output_items, input_items, &format!("{path}[]"), reasons)
            }
            (None, Some(_)) => reasons.push(format!("{path} has no declared item schema")),
            (Some(_), None) => {
                reasons.push(format!("{path} declares items but the input does not"))
            }
            (None, None) => {}
        },
        OutputType::Record => {
            for (name, output_field) in &output.fields {
                match input.fields.get(name) {
                    Some(input_field) => compare_schema(
                        output_field,
                        input_field,
                        &format!("{path}.{name}"),
                        reasons,
                    ),
                    None => reasons.push(format!("{path} has extra output field {name:?}")),
                }
            }
            let output_required = output.required.iter().collect::<BTreeSet<_>>();
            for name in &input.required {
                if !output_required.contains(name) {
                    reasons.push(format!(
                        "{path} does not guarantee required input field {name:?}"
                    ));
                }
            }
        }
        _ => {}
    }
}

fn output_type_name(kind: OutputType) -> &'static str {
    match kind {
        OutputType::Bytes => "bytes",
        OutputType::Text => "text",
        OutputType::Path => "path",
        OutputType::Integer => "integer",
        OutputType::Number => "number",
        OutputType::Boolean => "boolean",
        OutputType::Json => "json",
        OutputType::Record => "record",
        OutputType::List => "list",
    }
}

fn enum_constraint_assignable(output: Option<&[String]>, input: Option<&[String]>) -> bool {
    match (output, input) {
        (_, None) => true,
        (Some(output), Some(input)) => output.iter().all(|value| input.contains(value)),
        (None, Some(_)) => false,
    }
}

fn find_phrase_span_at(
    source: &str,
    line: usize,
    phrase: &str,
    line_starts: &LineStarts,
) -> Option<SourceSpan> {
    let expected = phrase.split_whitespace().collect::<Vec<_>>();
    if expected.is_empty() {
        return None;
    }
    let words = line_words_from_start(source, line_starts.start(line));
    words.windows(expected.len()).find_map(|window| {
        if window
            .iter()
            .zip(&expected)
            .all(|(word, expected)| !word.3 && word.0 == *expected)
        {
            Some(line_starts.span_at(source, window[0].1, window.last()?.2, SpanPrecision::Token))
        } else {
            None
        }
    })
}

fn line_words_from_start(source: &str, start: usize) -> Vec<(String, usize, usize, bool)> {
    let end = source[start..]
        .find('\n')
        .map_or(source.len(), |offset| start + offset);
    let raw = source.get(start..end).unwrap_or("");
    let line_text = raw.strip_suffix('\r').unwrap_or(raw);
    let content = parser::strip_comment(line_text);
    let mut words = Vec::new();
    let mut index = 0;
    let bytes = content.as_bytes();
    while index < bytes.len() {
        if matches!(
            bytes[index],
            b' ' | b'\t' | b'\r' | b'[' | b']' | b'{' | b'}' | b':' | b','
        ) {
            index += 1;
            continue;
        }
        let token_start = index;
        let quoted = bytes[index] == b'"';
        if quoted {
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
            }
        } else {
            while index < bytes.len()
                && !matches!(
                    bytes[index],
                    b' ' | b'\t' | b'\r' | b'[' | b']' | b'{' | b'}' | b':' | b',' | b'"'
                )
            {
                index += 1;
            }
        }
        words.push((
            content[token_start..index].to_owned(),
            start + token_start,
            start + index,
            quoted,
        ));
    }
    words
}

fn parameter_value_span_at(
    source: &str,
    line: usize,
    name: &str,
    line_starts: &LineStarts,
) -> Option<SourceSpan> {
    let words = line_words_from_start(source, line_starts.start(line));
    let parameter_index = find_parameter_index(&words, name)?;
    let value = words.get(parameter_index + 1)?;
    Some(line_starts.span_at(source, value.1, value.2, SpanPrecision::Token))
}

fn find_parameter_span_at(
    source: &str,
    line: usize,
    name: &str,
    line_starts: &LineStarts,
) -> Option<SourceSpan> {
    let words = line_words_from_start(source, line_starts.start(line));
    let index = find_parameter_index(&words, name)?;
    let token = &words[index];
    Some(line_starts.span_at(source, token.1, token.2, SpanPrecision::Token))
}

fn find_parameter_index(words: &[(String, usize, usize, bool)], name: &str) -> Option<usize> {
    let mut index = words.iter().position(|word| !word.3 && word.0 == "with")? + 1;
    while index < words.len() {
        let (candidate, _, _, quoted) = &words[index];
        if !quoted && candidate == name {
            return Some(index);
        }
        if *quoted || candidate == "and" || candidate == "cache" {
            return None;
        }
        index += 1;
        while index < words.len() && (words[index].3 || words[index].0 != "and") {
            index += 1;
        }
        if index < words.len() && words[index].0 == "and" {
            index += 1;
            if words.get(index).is_some_and(|word| word.0 == "cache") {
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{OutputType, ValueSchema};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[cfg(windows)]
    #[test]
    fn resolution_suggestions_skip_batch_files_and_match_exe_stems() {
        assert_eq!(
            executable_candidate_name(std::ffi::OsStr::new("tool.bat")),
            None
        );
        assert_eq!(
            executable_candidate_name(std::ffi::OsStr::new("tool.cmd")),
            None
        );
        assert_eq!(
            executable_candidate_name(std::ffi::OsStr::new("printf.exe")),
            Some("printf".into())
        );
    }

    #[test]
    fn independent_line_errors_have_stable_line_spans() {
        let source = "run\nread file\n";
        let diagnostics = analyze_source(source, &Registry::default());
        assert!(diagnostics.len() >= 2, "{diagnostics:#?}");
        assert_eq!(diagnostics[0].span.start_byte, 0);
        assert_eq!(diagnostics[0].span.line, 1);
        assert_eq!(diagnostics[0].span.precision, SpanPrecision::Line);
        assert_eq!(diagnostics[1].span.line, 2);
    }

    #[test]
    fn logical_statements_labels_suffixes_and_raw_blocks_validate() {
        let sources = [
            "fetch: run \"printf\" with arguments [\"hello\\n\"]\n",
            "run \"printf\" with arguments [\n  \"%s %s\\n\",\n  \"a\",\n  \"b\"\n]\n",
            "use value \"\"\"\nsay \"hi\" # not a comment\n  indented [\n\"\"\"\nthen cat\n",
            "run \"ls\" with arguments [\"-v\"] and stderr to \"e.log\"\n",
            "run \"env\" and environment {\"A\": \"1\"}\n",
        ];
        for source in sources {
            let errors = analyze_source(source, &Registry::default())
                .into_iter()
                .filter(|diagnostic| diagnostic.code.starts_with("parse."))
                .collect::<Vec<_>>();
            assert!(errors.is_empty(), "{source:?}: {errors:#?}");
        }
    }

    #[test]
    fn stray_word_does_not_choose_a_published_parse_code() {
        let diagnostics = analyze_source("run \"printf\" tabs\n", &Registry::default());
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "parse.syntax"));
    }

    #[test]
    fn invisible_format_controls_are_reported_with_token_spans() {
        let source = "run \"printf\" with arguments [\"x\u{202e}y\"]\n";
        let diagnostic = analyze_source(source, &Registry::default())
            .into_iter()
            .find(|diagnostic| diagnostic.code == "source.invisible_format")
            .expect("format control is diagnosed");
        assert_eq!(diagnostic.span.precision, SpanPrecision::Token);
        assert_eq!(
            &source[diagnostic.span.start_byte..diagnostic.span.end_byte],
            "\u{202e}"
        );
    }

    #[test]
    fn schema_mismatches_explain_nested_types() {
        let output = ValueSchema {
            kind: OutputType::Record,
            nullable: false,
            items: None,
            fields: BTreeMap::from([(
                "count".into(),
                ValueSchema {
                    kind: OutputType::Text,
                    nullable: false,
                    items: None,
                    fields: BTreeMap::new(),
                    required: vec![],
                    enum_values: None,
                },
            )]),
            required: vec!["count".into()],
            enum_values: None,
        };
        let input = ValueSchema {
            kind: OutputType::Record,
            nullable: false,
            items: None,
            fields: BTreeMap::from([(
                "count".into(),
                ValueSchema {
                    kind: OutputType::Integer,
                    nullable: false,
                    items: None,
                    fields: BTreeMap::new(),
                    required: vec![],
                    enum_values: None,
                },
            )]),
            required: vec!["count".into()],
            enum_values: None,
        };
        assert_eq!(
            schema_mismatch_reasons(&output, &input, "stdout", "stdin"),
            vec!["stdout → stdin.count is text, expected integer"]
        );
    }

    #[test]
    fn nullable_output_flows_only_to_nullable_or_nonnullable_inputs_safely() {
        let nonnullable = ValueSchema {
            kind: OutputType::Text,
            nullable: false,
            items: None,
            fields: BTreeMap::new(),
            required: vec![],
            enum_values: None,
        };
        let nullable = ValueSchema {
            nullable: true,
            ..nonnullable.clone()
        };
        assert!(schemas_compatible(&nonnullable, &nullable));
        assert!(schemas_compatible(&nonnullable, &nonnullable));
        assert!(!schemas_compatible(&nullable, &nonnullable));
        assert!(
            schema_mismatch_reasons(&nullable, &nonnullable, "source", "target")
                .iter()
                .any(|reason| reason.contains("may be null"))
        );
    }

    #[test]
    fn compatibility_uses_directional_json_subtypes_and_record_width_variance() {
        let text = ValueSchema {
            kind: OutputType::Text,
            nullable: false,
            items: None,
            fields: BTreeMap::new(),
            required: vec![],
            enum_values: None,
        };
        let json = ValueSchema {
            kind: OutputType::Json,
            ..text.clone()
        };
        let bytes = ValueSchema {
            kind: OutputType::Bytes,
            ..text.clone()
        };
        assert!(schemas_compatible(&text, &json));
        assert!(!schemas_compatible(&json, &text));
        assert!(!schemas_compatible(&bytes, &json));

        let text_list = ValueSchema {
            kind: OutputType::List,
            items: Some(Box::new(text.clone())),
            ..text.clone()
        };
        let json_list = ValueSchema {
            items: Some(Box::new(json)),
            ..text_list.clone()
        };
        assert!(schemas_compatible(&text_list, &json_list));
        assert!(!schemas_compatible(&json_list, &text_list));

        let output_record = ValueSchema {
            kind: OutputType::Record,
            fields: BTreeMap::from([("name".into(), text.clone())]),
            required: vec!["name".into()],
            ..text.clone()
        };
        let input_record = ValueSchema {
            fields: BTreeMap::from([
                ("name".into(), text.clone()),
                ("optional".into(), text.clone()),
            ]),
            required: vec![],
            ..output_record.clone()
        };
        assert!(schemas_compatible(&output_record, &input_record));
        assert!(output_record.is_assignable_to(&input_record));
        assert!(schemas_compatible(&output_record, &output_record));

        let input_requires_unproduced = ValueSchema {
            required: vec!["optional".into()],
            ..input_record.clone()
        };
        assert!(!output_record.is_assignable_to(&input_requires_unproduced));
        assert!(!schemas_compatible(
            &output_record,
            &input_requires_unproduced
        ));

        let output_with_optional = ValueSchema {
            fields: BTreeMap::from([
                ("name".into(), text.clone()),
                ("optional".into(), text.clone()),
            ]),
            required: vec!["name".into()],
            ..output_record.clone()
        };
        assert!(!schemas_compatible(&output_with_optional, &output_record));
        assert!(!output_with_optional.is_assignable_to(&output_record));
    }

    #[test]
    fn numeric_and_enum_schemas_are_directional_refinements() {
        let schema = |kind, enum_values| ValueSchema {
            kind,
            nullable: false,
            items: None,
            fields: BTreeMap::new(),
            required: Vec::new(),
            enum_values,
        };
        let integer = schema(OutputType::Integer, None);
        let number = schema(OutputType::Number, None);
        let json = schema(OutputType::Json, None);
        assert!(schemas_compatible(&integer, &number));
        assert!(!schemas_compatible(&number, &integer));
        assert!(schemas_compatible(&number, &json));
        assert!(schemas_compatible(&integer, &json));
        assert!(!schemas_compatible(&json, &number));

        let unrestricted = schema(OutputType::Text, None);
        let choices = schema(OutputType::Text, Some(vec!["red".into(), "blue".into()]));
        let subset = schema(OutputType::Text, Some(vec!["red".into()]));
        assert!(!schemas_compatible(&unrestricted, &choices));
        assert!(schemas_compatible(&choices, &unrestricted));
        assert!(schemas_compatible(&subset, &choices));
        assert!(!schemas_compatible(&choices, &subset));
    }

    #[test]
    fn parameter_type_checks_honor_nullable_and_any_of_schemas() {
        let nullable_text = json!({ "type": ["string", "null"] });
        assert!(value_matches_schema(
            &Value::String("value".into()),
            &nullable_text
        ));
        assert!(value_matches_schema(&Value::Null, &nullable_text));
        assert!(!value_matches_schema(&Value::Integer(4), &nullable_text));

        let union = json!({ "anyOf": [
            { "type": "integer" },
            { "type": "boolean" }
        ] });
        assert!(value_matches_schema(&Value::Bool(false), &union));
        assert!(!value_matches_schema(
            &Value::String("false".into()),
            &union
        ));
        assert_eq!(schema_type(&union), "integer or boolean");
    }

    #[test]
    fn wait_tcp_constraint_follows_effective_runtime_dispatch() {
        let source = "run wait.until with probe \"tcp_port_open\" and host \"8.8.8.8\" and port 443 and timeout_ms 0\n";
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let bundled =
            Registry::load_default(&manifest_dir, crate::manifest::ManifestCheck::Full).unwrap();
        assert!(analyze_source(source, &bundled)
            .iter()
            .any(|diagnostic| diagnostic.code == "applet.parameter.constraint"));

        let custom_command = json!({
            "name": "wait.until",
            "binary": "custom-wait",
            "parameters": {
                "probe": { "type": "string" },
                "host": { "type": "string" },
                "port": { "type": "integer" },
                "timeout_ms": { "type": "integer" }
            },
            "arguments": [{ "literal": "custom-operation" }]
        })
        .to_string();
        let custom = Registry::from_sources(&[custom_command.clone()]).unwrap();
        assert!(!analyze_source(source, &custom)
            .iter()
            .any(|diagnostic| diagnostic.code == "applet.parameter.constraint"));

        let scope_origin = tempfile::tempdir().unwrap();
        let scoped = bundled
            .with_scoped_manifest_sources(&[custom_command], scope_origin.path())
            .unwrap();
        assert!(!analyze_source(source, &scoped)
            .iter()
            .any(|diagnostic| diagnostic.code == "applet.parameter.constraint"));

        // Command manifests can route through the generic helper too. Only a
        // fixed operation literal establishes that it dispatches to wait.until.
        let helper_manifest = json!({
            "name": "fixture.waiter",
            "binary": "@execute-applet",
            "parameters": {
                "probe": { "type": "string" },
                "host": { "type": "string" },
                "port": { "type": "integer" },
                "timeout_ms": { "type": "integer" }
            },
            "arguments": [
                { "literal": "wait.until" },
                { "literal": "--args" },
                { "json_arguments": true }
            ]
        })
        .to_string();
        let helper = Registry::from_sources(&[helper_manifest]).unwrap();
        let helper_source = source.replace("wait.until", "fixture.waiter");
        assert!(analyze_source(&helper_source, &helper)
            .iter()
            .any(|diagnostic| diagnostic.code == "applet.parameter.constraint"));

        let dynamic_helper_manifest = json!({
            "name": "fixture.dynamic_waiter",
            "binary": "@execute-applet",
            "parameters": {
                "operation": { "type": "string", "required": true },
                "probe": { "type": "string" },
                "host": { "type": "string" },
                "port": { "type": "integer" },
                "timeout_ms": { "type": "integer" }
            },
            "arguments": [
                { "parameter": "operation" },
                { "literal": "--args" },
                { "json_arguments": true }
            ]
        })
        .to_string();
        let dynamic_helper = Registry::from_sources(&[dynamic_helper_manifest]).unwrap();
        let dynamic_source = source
            .replace("wait.until", "fixture.dynamic_waiter")
            .replace("with probe", "with operation \"wait.until\" and probe");
        assert!(!analyze_source(&dynamic_source, &dynamic_helper)
            .iter()
            .any(|diagnostic| diagnostic.code == "applet.parameter.constraint"));
    }

    #[test]
    fn reports_multiple_applet_parameter_problems_with_token_ranges() {
        let manifest = json!({
            "name": "fixture.reader",
            "binary": "fixture-reader",
            "parameters": { "path": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "path" }]
        });
        let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
        let source = "run fixture.reader with other \"file.txt\"\nrun fixture.reader\n";
        let diagnostics = analyze_source(source, &registry);
        assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
        let unknown = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "applet.parameter.unknown")
            .unwrap();
        assert_eq!(unknown.span.precision, SpanPrecision::Token);
        assert_eq!(
            &source[unknown.span.start_byte..unknown.span.end_byte],
            "other"
        );
        let line_two_required = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code == "applet.parameter.required" && diagnostic.span.line == 2
            })
            .unwrap();
        assert_eq!(line_two_required.span.line, 2);
    }

    #[test]
    fn loop_local_take_json_types_reach_static_applet_validation() {
        let fixture = json!({
            "name": "fixture.boolean",
            "binary": "fixture-boolean",
            "parameters": {"flag": {"type": "boolean", "required": true}},
            "arguments": [{"parameter": "flag"}]
        });
        let registry = Registry::from_sources(&[fixture.to_string()]).unwrap();
        let source = "for each item in [{\"n\":1}]\n  take integer at \"/n\" from item as n\n  run fixture.boolean with flag n\n";
        let diagnostics = analyze_source(source, &registry);
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "applet.parameter.type"
                    && diagnostic.message.contains("result \"n\" is integer")
            }),
            "loop-local typed take was not propagated: {diagnostics:#?}"
        );
    }

    #[test]
    fn parallel_for_each_template_uses_its_iteration_binding_type() {
        let fixture = json!({
            "name": "fixture.boolean",
            "binary": "fixture-boolean",
            "parameters": {"flag": {"type": "boolean", "required": true}},
            "arguments": [{"parameter": "flag"}]
        });
        let registry = Registry::from_sources(&[fixture.to_string()]).unwrap();
        let source = "parallel for each item in [\"x\"] at most 2 as results\n  run fixture.boolean with flag item\n";
        let diagnostics = analyze_source(source, &registry);
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "applet.parameter.type"
                    && diagnostic.message.contains("result \"item\" is text")
            }),
            "parallel loop template skipped its item schema check: {diagnostics:#?}"
        );
    }

    #[test]
    fn loop_item_inference_treats_nul_text_as_json_like_runtime() {
        let item = Value::String("text\0with-nul".into());
        let items = Value::List(vec![item]);
        let types = BTreeMap::new();
        assert_eq!(
            known_for_each_item_type(&items, &types, &types),
            Some(OutputType::Json)
        );
    }

    #[test]
    fn nul_list_item_type_matches_inline_bound_and_applet_list_sources() {
        let nul_items = Value::List(vec![Value::String("text\0with-nul".into())]);
        let mut known = BTreeMap::new();
        let mut list_items = BTreeMap::new();

        // A value binding and an applet's same-as-parameter list output both
        // infer their items through known_list_item_type.
        assert_eq!(known_value_type(&nul_items, &known), Some(OutputType::List));
        let item_kind = known_list_item_type(&nul_items, &known, &list_items).unwrap();
        assert_eq!(item_kind, OutputType::Json);
        known.insert("bound".into(), OutputType::List);
        list_items.insert("bound".into(), item_kind);
        assert_eq!(
            known_for_each_item_type(&Value::Reference("bound".into()), &known, &list_items,),
            Some(OutputType::Json)
        );

        let fixture = json!({
            "name": "fixture.boolean",
            "binary": "fixture-boolean",
            "parameters": {"flag": {"type": "boolean", "required": true}},
            "arguments": [{"parameter": "flag"}]
        });
        let list_fixture = json!({
            "name": "fixture.list",
            "binary": "fixture-list",
            "parameters": {"items": {"type": "json", "required": true}},
            "arguments": [{"json_arguments": true}],
            "contract": {
                "version": 1,
                "output": {
                    "type": "list",
                    "encoding": "json",
                    "items": {"type": "json"},
                    "same_as_parameter": "items"
                }
            }
        });
        let registry =
            Registry::from_sources(&[fixture.to_string(), list_fixture.to_string()]).unwrap();
        for source in [
            "parallel for each item in [\"\\u0000\"] at most 2 as results\n  run fixture.boolean with flag item\n",
            "use value [\"\\u0000\"] as bound\nparallel for each item in bound at most 2 as results\n  run fixture.boolean with flag item\n",
            "run fixture.list with items [\"\\u0000\"] as bound\nparallel for each item in bound at most 2 as results\n  run fixture.boolean with flag item\n",
        ] {
            let program = parser::parse(source)
                .unwrap_or_else(|error| panic!("could not parse {source:?}: {error:#}"));
            let mut call_scopes = HashMap::new();
            collect_scoped_call_types(
                &program.steps,
                &registry,
                &mut InferredTypeScope::default(),
                &mut call_scopes,
                false,
            );
            let template = program.steps.iter().find_map(|step| match &step.kind {
                StepKind::TaskGroup {
                    tasks,
                    each: Some(_),
                    ..
                } => tasks.first(),
                _ => None,
            }).unwrap();
            assert_eq!(
                call_scopes
                    .get(&(template as *const Invocation as usize))
                    .and_then(|scope| scope.scope.known_types.get("item")),
                Some(&OutputType::Json),
                "NUL-containing item must stay JSON-typed for {source:?}"
            );
            let diagnostics = analyze_source(source, &registry);
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.code != "applet.parameter.type"),
                "generic JSON loop item should defer schema validation for {source:?}: {diagnostics:#?}"
            );
        }
    }

    #[test]
    fn parameter_rename_fix_is_withheld_when_target_is_already_supplied() {
        let manifest = json!({
            "name": "text.trim_final_lf",
            "binary": "fixture-text",
            "parameters": { "text": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "text" }]
        });
        let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
        let diagnostics = analyze_source(
            "run text.trim_final_lf with text \"x\" and txet \"y\"\n",
            &registry,
        );
        let unknown = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "applet.parameter.unknown")
            .unwrap();
        assert!(unknown.fixes.is_empty());
        assert_eq!(unknown.span.precision, SpanPrecision::Token);
    }

    #[test]
    fn undefined_result_has_a_token_span_stable_code_and_suggestion() {
        let source = "accept name as text\nrun \"printf\" with arguments [\"%s\", nme]\n";
        let diagnostic = analyze_source(source, &Registry::default())
            .into_iter()
            .find(|diagnostic| diagnostic.code == "check.undefined_name")
            .expect("undefined binding diagnostic");
        assert_eq!(diagnostic.span.precision, SpanPrecision::Token);
        assert_eq!(
            &source[diagnostic.span.start_byte..diagnostic.span.end_byte],
            "nme"
        );
        assert_eq!(diagnostic.fixes[0].edits[0].replacement, "name");
    }

    #[test]
    fn known_applet_result_type_is_checked_at_reference_use() {
        let producer = json!({
            "name": "fixture.add",
            "binary": "fixture-add",
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "output": { "type": "number", "encoding": "json" } }
        });
        let consumer = json!({
            "name": "fixture.text",
            "binary": "fixture-text",
            "parameters": { "text": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "text" }]
        });
        let registry =
            Registry::from_sources(&[producer.to_string(), consumer.to_string()]).unwrap();
        let diagnostics = analyze_source(
            "run fixture.add as total\nrun fixture.text with text total\n",
            &registry,
        );
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "applet.parameter.type" && diagnostic.span.line == 2
            }),
            "{diagnostics:#?}"
        );
        let condition = analyze_source(
            "run fixture.add as total\nif total is \"3\"\n  run \"printf\" with arguments [\"three\"]\n",
            &registry,
        );
        assert!(
            condition.iter().any(|diagnostic| {
                diagnostic.code == "check.type_mismatch" && diagnostic.span.line == 2
            }),
            "{condition:#?}"
        );
    }

    #[test]
    fn token_spans_after_astral_text_keep_utf8_offsets_and_scalar_columns() {
        let manifest = json!({
            "name": "fixture.reader",
            "binary": "fixture-reader",
            "parameters": { "path": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "path" }]
        });
        let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
        let source = "run fixture.reader with note \"🍎\" and path 5";
        let diagnostic = analyze_source(source, &registry)
            .into_iter()
            .find(|diagnostic| diagnostic.code == "applet.parameter.type")
            .expect("static literal type mismatch");
        let byte = source.rfind('5').unwrap();
        assert_eq!(diagnostic.span.precision, SpanPrecision::Token);
        assert_eq!(diagnostic.span.start_byte, byte);
        assert_eq!(diagnostic.span.end_byte, byte + 1);
        assert_eq!(diagnostic.span.column, source[..byte].chars().count() + 1);
        assert_eq!(diagnostic.span.column, byte - 3 + 1);
    }

    #[test]
    fn diagnoses_incompatible_declared_pipeline_contracts() {
        let producer = json!({
            "name": "fixture.producer",
            "binary": "producer",
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "output": { "type": "text", "encoding": "raw" } }
        });
        let consumer = json!({
            "name": "fixture.consumer",
            "binary": "consumer",
            "accepts_stdin": true,
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "ports": { "inputs": {
                "stdin": { "schema": { "type": "integer" }, "delivery": "single", "encoding": "json" }
            } } }
        });
        let registry =
            Registry::from_sources(&[producer.to_string(), consumer.to_string()]).unwrap();
        let source = "run fixture.producer\npipe to fixture.consumer\n";
        let diagnostics = analyze_source(source, &registry);
        let mismatch = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "applet.pipeline.incompatible")
            .expect("typed pipeline mismatch");
        assert_eq!(mismatch.span.line, 2);
        assert!(mismatch.message.contains("is text, expected integer"));
    }

    #[test]
    fn diagnoses_pipeline_encoding_mismatch_even_when_types_match() {
        let producer = json!({
            "name": "fixture.producer",
            "binary": "producer",
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "output": { "type": "text", "encoding": "raw" } }
        });
        let consumer = json!({
            "name": "fixture.consumer",
            "binary": "consumer",
            "accepts_stdin": true,
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "ports": { "inputs": {
                "stdin": { "schema": { "type": "text" }, "delivery": "single", "encoding": "json" }
            } } }
        });
        let registry =
            Registry::from_sources(&[producer.to_string(), consumer.to_string()]).unwrap();
        let diagnostics = analyze_source(
            "run fixture.producer\npipe to fixture.consumer\n",
            &registry,
        );
        let mismatch = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "applet.pipeline.incompatible")
            .expect("pipeline encoding mismatch");
        assert!(mismatch.message.contains("stdout encoding is Raw"));
        assert!(mismatch.message.contains("expected stdin encoding Json"));
    }

    #[test]
    fn diagnoses_incompatible_pipeline_inside_reusable_program() {
        let producer = json!({
            "name": "fixture.producer",
            "binary": "producer",
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "output": { "type": "text", "encoding": "raw" } }
        });
        let consumer = json!({
            "name": "fixture.consumer",
            "binary": "consumer",
            "accepts_stdin": true,
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "ports": { "inputs": {
                "stdin": { "schema": { "type": "integer" }, "delivery": "single", "encoding": "json" }
            } } }
        });
        let registry =
            Registry::from_sources(&[producer.to_string(), consumer.to_string()]).unwrap();
        let source =
            "define a program named convert\n  run fixture.producer\n  pipe to fixture.consumer\n";
        let diagnostics = analyze_source(source, &registry);
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "applet.pipeline.incompatible" && diagnostic.span.line == 3
            }),
            "{diagnostics:#?}"
        );
    }

    #[test]
    fn diagnostic_replay_uses_captured_callee_source_after_file_removal() {
        let root = tempfile::tempdir().unwrap();
        let caller = root.path().join("caller.program");
        let callee = root.path().join("callee.program");
        let source = "run program file \"callee.program\"\n";
        std::fs::write(&caller, source).unwrap();
        std::fs::write(
            &callee,
            "accept theme as text\nrun \"printf\" with arguments [them]\n",
        )
        .unwrap();
        let canonical_callee = std::fs::canonicalize(&callee).unwrap();
        let registry = Registry::default();
        let access = crate::program_files::Access {
            registry: &registry,
            cwd: root.path(),
        };
        let (parsed, snapshot) =
            crate::program_files::parse_at_with_snapshot(source, Some(&caller), Some(access), None);
        parsed.unwrap();
        std::fs::remove_file(&callee).unwrap();

        let diagnostics = analyze_source_impl(
            source,
            Some(&caller),
            &registry,
            #[cfg(feature = "lsp")]
            None,
            Some(&snapshot.replay()),
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "check.undefined_name"
                && diagnostic.span.path.as_deref() == canonical_callee.to_str()
                && diagnostic.span.line == 2
        }), "{diagnostics:#?}");
    }
}
