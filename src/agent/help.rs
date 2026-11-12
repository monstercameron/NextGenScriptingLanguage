//! `execute help FILE [--json | --format omarchy-metadata]`: usage generated from a
//! program's accept declarations. The command parses the program and runs nothing. It reports
//! each input's name, type, whether it is required or its default, and its
//! source, and shows each input's `described as` text. `--format omarchy-metadata`
//! prints the `# omarchy:` command-metadata lines that the declarations can supply.

use crate::ast::{InputDeclaration, InputSource, InputType, Program, Value as ProgramValue};
use crate::terminal_text::{escape_controls, escape_terminal_field};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.help/1";
const USAGE: &str = "usage: execute help FILE [--json | --format omarchy-metadata]";
/// The `--format` value that prints Omarchy command-metadata comment lines.
const OMARCHY_METADATA: &str = "omarchy-metadata";
/// Omarchy reads its metadata from the first 80 lines of a command file.
const OMARCHY_WINDOW: usize = 80;

/// Run the `help` subcommand and return the process exit status.
pub fn cli(arguments: &[String]) -> Result<i32> {
    let registry = crate::manifest::Registry::default();
    let cwd = std::env::current_dir()?;
    cli_with_access(
        arguments,
        crate::program_files::Access {
            registry: &registry,
            cwd: &cwd,
        },
    )
}

/// Run help with the same host-read authority as the caller's program loader.
pub fn cli_with_access(
    arguments: &[String],
    access: crate::program_files::Access<'_>,
) -> Result<i32> {
    let mut as_json = false;
    let mut omarchy_metadata = false;
    let mut file = None;
    let mut remaining = arguments.iter();
    while let Some(argument) = remaining.next() {
        match argument.as_str() {
            "--json" => as_json = true,
            "--format" => match remaining.next().map(String::as_str) {
                Some(OMARCHY_METADATA) => omarchy_metadata = true,
                Some(other) => bail!("unknown format {other}; {USAGE}"),
                None => bail!("--format needs a value; {USAGE}"),
            },
            other if other.starts_with("--") => bail!("unknown option {other}; {USAGE}"),
            other => {
                if file.replace(other).is_some() {
                    bail!("help takes one program file; {USAGE}");
                }
            }
        }
    }
    if as_json && omarchy_metadata {
        bail!("--json and --format are different outputs; {USAGE}");
    }
    let file = file.ok_or_else(|| anyhow!("{USAGE}"))?;
    let registry = access.registry;
    let program = crate::program_files::parse_file(Path::new(file), Some(access))?;
    let source = std::fs::read_to_string(file)?;
    super::errors::static_gate_without_manifests(&source, Path::new(file), &program, registry)?;
    if omarchy_metadata {
        io::stdout().write_all(render_omarchy_metadata(&program).as_bytes())?;
        return Ok(0);
    }
    let description = describe(&program, Path::new(file).to_string_lossy().as_ref());
    if as_json {
        writeln!(
            io::stdout(),
            "{}",
            serde_json::to_string_pretty(&description)?
        )?;
    } else {
        io::stdout().write_all(render_text(&description).as_bytes())?;
    }
    Ok(0)
}

/// The same text `execute help FILE` prints, for a script's own `--help`.
pub fn render_program_help(program: &Program, program_path: &str) -> String {
    render_text(&describe(program, program_path))
}

/// The help object for one parsed program.
pub fn describe(program: &Program, program_path: &str) -> Value {
    let inputs: Vec<Value> = program.inputs.iter().map(input_entry).collect();
    json!({
        "protocol": PROTOCOL,
        "program": program_path,
        "usage": format!("execute {program_path} [--input NAME=VALUE ...] [-- OPTIONS]"),
        "inputs": inputs,
    })
}

fn input_entry(input: &InputDeclaration) -> Value {
    json!({
        "name": input.name,
        "type": type_label(input.kind),
        // A flag without a default is simply false when absent.
        "required": input.default.is_none() && !matches!(input.source, Some(InputSource::Flag { .. })),
        "default": input
            .default
            .as_ref()
            .map(plain_json)
            .or_else(|| matches!(input.source, Some(InputSource::Flag { .. })).then(|| json!(false))),
        "source": input.source.as_ref().map(|source| source_label(source, input.public)),
        "spelling": flag_spelling(input),
        "description": input.description,
        "line": input.line,
    })
}

/// The command-line spelling of a flag or option, for example `--output VALUE, -o VALUE`.
fn flag_spelling(input: &InputDeclaration) -> Option<String> {
    let (name, alias, value) = match &input.source {
        Some(InputSource::Flag { name, alias }) => (name, alias, ""),
        Some(InputSource::Option { name, alias }) => (name, alias, " VALUE"),
        _ => return None,
    };
    Some(match alias {
        Some(alias) => format!("{name}{value}, {alias}{value}"),
        None => format!("{name}{value}"),
    })
}

/// A program default as plain JSON. The program's own serde form is tagged, so it
/// is converted here, and every variant is named.
fn plain_json(value: &ProgramValue) -> Value {
    match value {
        ProgramValue::String(text) => json!(text),
        ProgramValue::Integer(number) => json!(number),
        ProgramValue::Bool(flag) => json!(flag),
        ProgramValue::Number(number) => Value::Number(number.clone()),
        ProgramValue::Null => Value::Null,
        ProgramValue::List(items) => Value::Array(items.iter().map(plain_json).collect()),
        ProgramValue::Record(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, item)| (name.clone(), plain_json(item)))
                .collect(),
        ),
        ProgramValue::Reference(name) => json!({ "reference": name }),
    }
}

/// Exhaustive on purpose: a new input type must be labelled before it compiles.
fn type_label(kind: InputType) -> &'static str {
    match kind {
        InputType::Text => "text",
        InputType::Integer => "integer",
        InputType::UnsignedInteger => "unsigned integer",
        InputType::Usize => "usize",
        InputType::Boolean => "boolean",
        InputType::TextList => "list of text",
        InputType::IntegerList => "list of integer",
        InputType::UnsignedIntegerList => "list of unsigned integer",
        InputType::UsizeList => "list of usize",
        InputType::JsonList => "list of json",
        InputType::Json => "json",
        InputType::Never => "uninhabited empty-loop item",
        InputType::Bytes => "bytes",
    }
}

/// Exhaustive on purpose: a new source must be labelled before it compiles.
fn source_label(source: &InputSource, public: bool) -> String {
    match source {
        InputSource::Environment(name) if public => format!("public environment {name}"),
        InputSource::Environment(name) => format!("environment {name}"),
        InputSource::Argument(index) => format!("argument {index}"),
        InputSource::Arguments => "all arguments".to_owned(),
        InputSource::ProgramDirectory => "program directory".to_owned(),
        InputSource::ProgramPath => "program path".to_owned(),
        InputSource::Prompt { secret: true, .. } => "secret prompt".to_owned(),
        InputSource::Prompt { secret: false, .. } => "prompt".to_owned(),
        InputSource::Flag { name, .. } => format!("flag {name}"),
        InputSource::Option { name, .. } => format!("option {name}"),
    }
}

fn render_text(description: &Value) -> String {
    let usage = escape_terminal_field(description["usage"].as_str().unwrap_or(""));
    let mut text = format!("{usage}\n");
    let inputs = description["inputs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if inputs.is_empty() {
        text.push_str("\nInputs: none\n");
        return text;
    }
    let (options, arguments): (Vec<&Value>, Vec<&Value>) = inputs
        .iter()
        .partition(|input| input["spelling"].is_string());
    if !options.is_empty() {
        text.push_str("\nOptions:\n");
        for option in options {
            let spelling = escape_terminal_field(option["spelling"].as_str().unwrap_or(""));
            text.push_str(&format!(
                "  {spelling:<30}  {}  {}",
                option["type"].as_str().unwrap_or(""),
                requirement(option)
            ));
            push_description(&mut text, option);
        }
        text.push_str("  -h, --help                      show this help\n");
    }
    if !arguments.is_empty() {
        text.push_str("\nInputs:\n");
        for input in arguments {
            let name = escape_terminal_field(input["name"].as_str().unwrap_or(""));
            text.push_str(&format!(
                "  {}  {}  {}",
                name,
                input["type"].as_str().unwrap_or(""),
                requirement(input)
            ));
            if let Some(source) = input["source"].as_str() {
                text.push_str(&format!("  (from {})", escape_terminal_field(source)));
            }
            push_description(&mut text, input);
        }
    }
    escape_controls(&text)
}

fn requirement(input: &Value) -> String {
    if input["required"] == true {
        "required".to_owned()
    } else {
        escape_terminal_field(&format!("default {}", input["default"]))
    }
}

fn push_description(text: &mut String, input: &Value) {
    if let Some(description) = input["description"].as_str() {
        text.push_str(&format!("  {}", escape_terminal_field(description)));
    }
    text.push('\n');
}

/// The `# omarchy:` lines the declarations can supply, one per line. `summary` joins
/// the `described as` texts, and `args` lists the positional inputs. Keys that need
/// information the language does not declare (group, alias, hidden, requires-sudo)
/// are not emitted.
pub fn render_omarchy_metadata(program: &Program) -> String {
    let mut lines = Vec::new();
    let summary: Vec<String> = program
        .inputs
        .iter()
        .filter_map(|input| input.description.as_deref().map(header_value))
        .collect();
    if !summary.is_empty() {
        lines.push(format!("# omarchy:summary={}", summary.join("; ")));
    }
    let args: Vec<String> = program.inputs.iter().filter_map(argument_token).collect();
    if !args.is_empty() {
        lines.push(format!("# omarchy:args={}", args.join(" ")));
    }
    lines.iter().map(|line| format!("{line}\n")).collect()
}

/// Problems with `# omarchy:` lines already in a command file: a line past the first
/// 80 lines, or a line that differs from what `render_omarchy_metadata` generates.
pub fn omarchy_metadata_problems(source: &str, program: &Program) -> Vec<String> {
    let generated = render_omarchy_metadata(program);
    let expected: BTreeMap<&str, &str> = generated
        .lines()
        .filter_map(|line| Some((metadata_key(line)?, line)))
        .collect();
    let mut problems = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let Some(key) = metadata_key(line) else {
            continue;
        };
        let number = index + 1;
        if index >= OMARCHY_WINDOW {
            problems.push(format!(
                "line {number}: `# omarchy:` metadata must sit within the first {OMARCHY_WINDOW} lines"
            ));
        }
        if expected.get(key).copied() != Some(line) {
            problems.push(format!(
                "line {number}: `# omarchy:{key}` does not match the program's declarations"
            ));
        }
    }
    problems
}

/// The key of an `# omarchy:KEY=VALUE` line, or `None` for any other line.
fn metadata_key(line: &str) -> Option<&str> {
    line.strip_prefix("# omarchy:")?
        .split_once('=')
        .map(|(key, _)| key)
}

/// One positional input as Omarchy writes it: `<name>` when required, `[name]` when
/// it has a default, and `a|b|c` for `one of` choices. A variadic input is `[name...]`.
fn argument_token(input: &InputDeclaration) -> Option<String> {
    let body = match &input.source {
        Some(InputSource::Argument(_)) => match input.choices.as_deref() {
            Some(choices) if !choices.is_empty() => choices
                .iter()
                .map(|choice| header_value(choice))
                .collect::<Vec<_>>()
                .join("|"),
            _ => header_value(&input.name),
        },
        Some(InputSource::Arguments) => format!("{}...", header_value(&input.name)),
        _ => return None,
    };
    let variadic = matches!(input.source, Some(InputSource::Arguments));
    Some(if input.default.is_none() && !variadic {
        format!("<{body}>")
    } else {
        format!("[{body}]")
    })
}

/// Makes text safe inside one `#` comment line: control characters and line
/// separators become spaces, so no text can end the comment early.
fn header_value(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{describe, render_text};
    use serde_json::json;

    #[test]
    fn generated_usage_places_flags_after_the_argument_separator() {
        let program =
            crate::parser::parse("accept verbose as boolean from flag \"--verbose\"\n").unwrap();
        let description = describe(&program, "tool.program");
        assert_eq!(
            description["usage"],
            "execute tool.program [--input NAME=VALUE ...] [-- OPTIONS]"
        );
    }

    #[test]
    fn help_text_escapes_untrusted_descriptions_and_input_names() {
        let description = json!({
            "usage": "execute program",
            "inputs": [{
                "type": "text",
                "required": true,
                "default": null,
                "description": "clear\u{001b}\u{202e}",
                "name": "x\u{009b}",
                "source": null
            }]
        });
        let rendered = render_text(&description);
        assert!(!rendered.contains('\u{001b}'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(rendered.contains(r"\u{1b}"));
        assert!(rendered.contains(r"\u{202e}"));
        assert!(rendered.contains(r"\u{9b}"));
    }

    #[test]
    fn help_fields_escape_line_controls_without_changing_template_layout() {
        let description = json!({
            "usage": "execute program\nInjected",
            "inputs": [{
                "type": "text",
                "required": true,
                "default": null,
                "description": "first\nsecond\tcolumn\u{2028}line\u{2029}paragraph",
                "name": "name\nInjected",
                "source": "environment NAME\nInjected"
            }]
        });

        let rendered = render_text(&description);
        assert_eq!(rendered.matches('\n').count(), 4, "{rendered:?}");
        assert!(rendered.contains(r"execute program\u{a}Injected"));
        assert!(rendered.contains(r"name\u{a}Injected"));
        assert!(rendered.contains(r"environment NAME\u{a}Injected"));
        assert!(rendered.contains(r"first\u{a}second\u{9}column\u{2028}line\u{2029}paragraph"));
    }

    #[test]
    fn omarchy_metadata_lists_described_inputs_and_positional_arguments() {
        let program = crate::parser::parse(
            "accept name as text from argument 1 described as \"Web app name\"\n\
             accept mode as text from argument 2 with default \"toggle\" described as \"Mode to apply\"\n",
        )
        .unwrap();
        assert_eq!(
            super::render_omarchy_metadata(&program),
            "# omarchy:summary=Web app name; Mode to apply\n# omarchy:args=<name> [mode]\n"
        );
    }

    #[test]
    fn omarchy_metadata_is_empty_without_described_or_positional_inputs() {
        let program =
            crate::parser::parse("accept verbose as boolean from flag \"--verbose\"\n").unwrap();
        assert_eq!(super::render_omarchy_metadata(&program), "");
    }

    #[test]
    fn omarchy_metadata_text_cannot_end_the_comment_line() {
        assert_eq!(super::header_value("a\nb\u{2028}c\r"), "a b c");
    }

    #[test]
    fn omarchy_lint_accepts_matching_lines_within_the_window() {
        let program = crate::parser::parse(
            "accept name as text from argument 1 described as \"Web app name\"\n",
        )
        .unwrap();
        let source = "#!/bin/sh\n# omarchy:summary=Web app name\n# omarchy:args=<name>\n";
        assert!(super::omarchy_metadata_problems(source, &program).is_empty());
    }

    #[test]
    fn omarchy_lint_reports_disagreeing_and_late_lines() {
        let program = crate::parser::parse(
            "accept name as text from argument 1 described as \"Web app name\"\n",
        )
        .unwrap();
        let disagreeing = "# omarchy:summary=Other text\n";
        let problems = super::omarchy_metadata_problems(disagreeing, &program);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("line 1"), "{problems:?}");

        let late = format!("{}# omarchy:summary=Web app name\n", "\n".repeat(80));
        let problems = super::omarchy_metadata_problems(&late, &program);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("first 80 lines"), "{problems:?}");
    }
}
