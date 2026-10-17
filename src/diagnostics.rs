//! Structured static diagnostics for `.program` source.
//!
//! Spans use UTF-8 byte offsets in a half-open range and one-based Unicode-scalar
//! columns. A `line` span is intentionally broad when the parser only identifies
//! a line; token spans are emitted only when the token can be located reliably.

use crate::ast::{Connection, Invocation, Program, Step, StepKind, Value};
use crate::contracts::{OutputType, ValueSchema};
use crate::manifest::{Manifest, Registry};
use crate::parser::{self, Token};
use crate::suggest;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;

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
}

/// Every code `execute check --json` can emit. A code is never renamed. A new
/// meaning gets a new code, and the family is documented per code.
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: "parse.lexical",
        family: "parse",
        meaning: "a line has an unterminated or invalid quoted or bracketed token",
    },
    CodeInfo {
        code: "parse.syntax",
        family: "parse",
        meaning: "a statement does not match the grammar",
    },
    CodeInfo {
        code: "parse.indentation",
        family: "parse",
        meaning: "a line is indented with tabs, or its indentation does not nest",
    },
    CodeInfo {
        code: "parse.control",
        family: "parse",
        meaning: "a control construct is unsupported or nested beyond the supported depth",
    },
    CodeInfo {
        code: "applet.unknown",
        family: "resolve",
        meaning: "the operation names no declared applet and is not a quoted executable",
    },
    CodeInfo {
        code: "applet.parameter.unknown",
        family: "resolve",
        meaning: "the applet declares no parameter with this name",
    },
    CodeInfo {
        code: "applet.parameter.type",
        family: "type",
        meaning: "a literal value does not match the parameter's declared type",
    },
    CodeInfo {
        code: "applet.parameter.required",
        family: "validate",
        meaning: "a required parameter is missing from the call",
    },
    CodeInfo {
        code: "applet.pipeline.incompatible",
        family: "type",
        meaning: "a pipeline stage's declared output does not satisfy the next stage's input",
    },
    CodeInfo {
        code: "data_flow_policy_not_configured",
        family: "policy",
        meaning: "a capability policy is set without a data-flow policy, so remote egress and persistence are unprotected",
    },
    CodeInfo {
        code: "check_failed",
        family: "validate",
        meaning: "validation failed without a static diagnostic; the causes list explains why",
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
    let mut diagnostics = Vec::new();
    let mut local_error_lines = BTreeSet::new();

    for (index, raw_line) in source.split('\n').enumerate() {
        let line_number = index + 1;
        let without_cr = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let uncommented = parser::strip_comment(without_cr);
        if uncommented.trim().is_empty() {
            continue;
        }

        if let Some(tab) = indentation_tab(uncommented) {
            diagnostics.push(Diagnostic::error(
                "parse.indentation",
                "tabs are not allowed for indentation",
                span_at(
                    source,
                    line_start(source, line_number) + tab,
                    line_start(source, line_number) + tab + 1,
                    SpanPrecision::Token,
                ),
            ));
            local_error_lines.insert(line_number);
            continue;
        }

        let content = uncommented.trim_start_matches(' ');
        let Ok(tokens) = parser::tokenize(content) else {
            diagnostics.push(Diagnostic::error(
                "parse.lexical",
                lexical_message(content),
                line_span(source, line_number),
            ));
            local_error_lines.insert(line_number);
            continue;
        };
        if tokens.is_empty() || is_contextual_header(&tokens) {
            continue;
        }
        if let Err(error) = parser::parse_line(&tokens) {
            let message = format!("{error:#}");
            let mut diagnostic = Diagnostic::error(
                syntax_code(&message),
                message,
                line_span(source, line_number),
            );
            let line_start_offset = line_start(source, line_number);
            let content_start = line_start_offset + (uncommented.len() - content.len());
            if let Some(first) = content.split_whitespace().next() {
                if let Some(keyword) = suggest::keyword_for(first) {
                    let span = span_at(
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
    match crate::program_files::parse_at(source, path, Some(access)) {
        Ok(program) => {
            collect_program_diagnostics(source, &program, registry, &mut diagnostics);
            collect_pipeline_diagnostics(source, &program, registry, &mut diagnostics);
        }
        Err(error) => {
            let message = format!("{error:#}");
            let error_line = find_line_number(&message).unwrap_or(1);
            if !local_error_lines.contains(&error_line) {
                diagnostics.push(Diagnostic::error(
                    syntax_code(&message),
                    message,
                    line_span(source, error_line),
                ));
            }
        }
    }

    diagnostics.sort_by(|left, right| {
        (left.span.start_byte, &left.code, &left.message).cmp(&(
            right.span.start_byte,
            &right.code,
            &right.message,
        ))
    });
    diagnostics.dedup_by(|left, right| {
        left.code == right.code
            && left.message == right.message
            && left.span.start_byte == right.span.start_byte
            && left.span.end_byte == right.span.end_byte
    });
    diagnostics
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
    if message.contains("indent") || message.contains("tabs") {
        "parse.indentation"
    } else if message.contains("unsupported control") || message.contains("control nesting") {
        "parse.control"
    } else {
        "parse.syntax"
    }
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

fn line_start(source: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    source
        .match_indices('\n')
        .nth(line - 2)
        .map_or(source.len(), |(offset, _)| offset + 1)
}

fn line_span(source: &str, line: usize) -> SourceSpan {
    let start = line_start(source, line);
    let mut end = source[start..]
        .find('\n')
        .map_or(source.len(), |offset| start + offset);
    if end > start && source.as_bytes()[end - 1] == b'\r' {
        end -= 1;
    }
    span_at(source, start, end, SpanPrecision::Line)
}

fn span_at(source: &str, start: usize, end: usize, precision: SpanPrecision) -> SourceSpan {
    let start = start.min(source.len());
    let end = end.max(start).min(source.len());
    let (line, column) = position_for_byte(source, start);
    let (end_line, end_column) = position_for_byte(source, end);
    SourceSpan {
        start_byte: start,
        end_byte: end,
        line,
        column,
        end_line,
        end_column,
        precision,
    }
}

fn position_for_byte(source: &str, offset: usize) -> (usize, usize) {
    let prefix = &source[..offset.min(source.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_prefix = prefix.rsplit_once('\n').map_or(prefix, |(_, tail)| tail);
    (line, line_prefix.chars().count() + 1)
}

fn collect_program_diagnostics(
    source: &str,
    program: &Program,
    registry: &Registry,
    diagnostics: &mut Vec<Diagnostic>,
) {
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
    for definition in program.definitions.values() {
        collect_calls(&definition.steps, &mut calls);
    }

    for (line, call) in calls {
        if call.direct {
            continue;
        }
        let manifest = match registry.resolve_call(call) {
            Ok(manifest) => manifest,
            Err(_) => {
                let span = find_phrase_span(source, line, &call.operation)
                    .unwrap_or_else(|| line_span(source, line));
                diagnostics.push(unknown_applet_diagnostic(
                    source, line, call, span, registry,
                ));
                continue;
            }
        };
        validate_call(source, line, call, manifest, diagnostics);
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
                let first_span = span_at(
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
        if let Some(name) = suggest::closest(
            phrase,
            registry
                .manifests()
                .iter()
                .map(|manifest| manifest.name.as_str()),
        ) {
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
            let at_statement_start = source[line_start(source, line)..span.start_byte]
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

fn validate_call(
    source: &str,
    line: usize,
    call: &Invocation,
    manifest: &Manifest,
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
            let span =
                find_parameter_span(source, line, name).unwrap_or_else(|| line_span(source, line));
            let mut diagnostic = Diagnostic::error(
                "applet.parameter.unknown",
                format!("applet {:?} has no parameter {name:?}", manifest.name),
                span.clone(),
            );
            if let Some(suggestion) = suggest::closest(name, properties.keys().map(String::as_str))
            {
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
        if !matches!(value, Value::Reference(_)) && !value_matches_schema(value, schema) {
            diagnostics.push(Diagnostic::error(
                "applet.parameter.type",
                format!(
                    "parameter {name:?} of applet {:?} expects {}, but this value has type {}",
                    manifest.name,
                    schema_type(schema),
                    value_type(value),
                ),
                parameter_value_span(source, line, name).unwrap_or_else(|| line_span(source, line)),
            ));
        }
    }

    for name in required {
        if !call.parameters.contains_key(name) {
            diagnostics.push(Diagnostic::error(
                "applet.parameter.required",
                format!("applet {:?} requires parameter {name:?}", manifest.name),
                find_phrase_span(source, line, &call.operation)
                    .unwrap_or_else(|| line_span(source, line)),
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
            values
                .iter()
                .all(|item| value_matches_schema(item, item_schema))
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
    diagnostics: &mut Vec<Diagnostic>,
) {
    collect_pipeline_step_pairs(source, &program.steps, registry, diagnostics);
    for definition in program.definitions.values() {
        collect_pipeline_step_pairs(source, &definition.steps, registry, diagnostics);
    }
}

fn collect_pipeline_step_pairs(
    source: &str,
    steps: &[Step],
    registry: &Registry,
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
            line_span(source, pair[1].line),
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

fn find_phrase_span(source: &str, line: usize, phrase: &str) -> Option<SourceSpan> {
    let expected = phrase.split_whitespace().collect::<Vec<_>>();
    if expected.is_empty() {
        return None;
    }
    let words = line_words(source, line);
    words.windows(expected.len()).find_map(|window| {
        if window
            .iter()
            .zip(&expected)
            .all(|(word, expected)| !word.3 && word.0 == *expected)
        {
            Some(span_at(
                source,
                window[0].1,
                window.last()?.2,
                SpanPrecision::Token,
            ))
        } else {
            None
        }
    })
}

fn line_words(source: &str, line: usize) -> Vec<(String, usize, usize, bool)> {
    let start = line_start(source, line);
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
        if matches!(bytes[index], b' ' | b'\t' | b'\r' | b'[' | b']' | b',') {
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
                    b' ' | b'\t' | b'\r' | b'[' | b']' | b',' | b'"'
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

fn parameter_value_span(source: &str, line: usize, name: &str) -> Option<SourceSpan> {
    let words = line_words(source, line);
    let parameter_index = find_parameter_index(&words, name)?;
    let value = words.get(parameter_index + 1)?;
    Some(span_at(source, value.1, value.2, SpanPrecision::Token))
}

fn find_parameter_span(source: &str, line: usize, name: &str) -> Option<SourceSpan> {
    let words = line_words(source, line);
    let index = find_parameter_index(&words, name)?;
    let token = &words[index];
    Some(span_at(source, token.1, token.2, SpanPrecision::Token))
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
}
