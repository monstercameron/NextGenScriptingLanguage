//! `execute help FILE [--json]`: usage generated from a program's accept
//! declarations. The command parses the program and runs nothing. It reports
//! each input's name, type, whether it is required or its default, and its
//! source. A `described as` text would be shown here once the grammar carries it;
//! until then every input reports `description: null`.

use crate::ast::{InputDeclaration, InputSource, InputType, Program, Value as ProgramValue};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.help/1";
const USAGE: &str = "usage: execute help FILE [--json]";

/// Run the `help` subcommand and return the process exit status.
pub fn cli(arguments: &[String]) -> Result<i32> {
    let mut as_json = false;
    let mut file = None;
    for argument in arguments {
        match argument.as_str() {
            "--json" => as_json = true,
            other if other.starts_with("--") => bail!("unknown option {other}; {USAGE}"),
            other => {
                if file.replace(other).is_some() {
                    bail!("help takes one program file; {USAGE}");
                }
            }
        }
    }
    let file = file.ok_or_else(|| anyhow!("{USAGE}"))?;
    let program = crate::program_files::parse_file(Path::new(file))?;
    let description = describe(&program, Path::new(file).to_string_lossy().as_ref());
    if as_json {
        println!("{}", serde_json::to_string_pretty(&description)?);
    } else {
        print!("{}", render_text(&description));
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
        "usage": format!("execute {program_path} [--input NAME=VALUE ...]"),
        "inputs": inputs,
    })
}

fn input_entry(input: &InputDeclaration) -> Value {
    json!({
        "name": input.name,
        "type": type_label(input.kind),
        "required": input.default.is_none(),
        "default": input.default.as_ref().map(plain_json),
        "source": input.source.as_ref().map(source_label),
        "description": null,
        "line": input.line,
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
        InputType::Boolean => "boolean",
        InputType::TextList => "list of text",
        InputType::Json => "json",
        InputType::Bytes => "bytes",
    }
}

/// Exhaustive on purpose: a new source must be labelled before it compiles.
fn source_label(source: &InputSource) -> String {
    match source {
        InputSource::Environment(name) => format!("environment {name}"),
        InputSource::Argument(index) => format!("argument {index}"),
        InputSource::Arguments => "all arguments".to_owned(),
        InputSource::ProgramDirectory => "program directory".to_owned(),
        InputSource::ProgramPath => "program path".to_owned(),
    }
}

fn render_text(description: &Value) -> String {
    let mut text = format!("{}\n", description["usage"].as_str().unwrap_or(""));
    let inputs = description["inputs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if inputs.is_empty() {
        text.push_str("\nInputs: none\n");
        return text;
    }
    text.push_str("\nInputs:\n");
    for input in &inputs {
        let requirement = if input["required"] == true {
            "required".to_owned()
        } else {
            format!("default {}", input["default"])
        };
        text.push_str(&format!(
            "  {}  {}  {}",
            input["name"].as_str().unwrap_or(""),
            input["type"].as_str().unwrap_or(""),
            requirement
        ));
        if let Some(source) = input["source"].as_str() {
            text.push_str(&format!("  (from {source})"));
        }
        text.push('\n');
    }
    text
}
