#![allow(dead_code)]

use flow::manifest::Registry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct InvalidSource {
    pub source: String,
    pub expected_code: &'static str,
    pub expected_token: String,
}

fn fixture_manifests() -> Vec<Value> {
    vec![
        json!({
            "name": "fixture.reader",
            "binary": "fixture-reader",
            "accepts_stdin": false,
            "cacheable": false,
            "parameters": { "arguments": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "arguments" }]
        }),
        json!({
            "name": "fixture.count",
            "binary": "fixture-count",
            "accepts_stdin": false,
            "cacheable": false,
            "parameters": { "count": { "type": "integer", "required": true } },
            "arguments": [{ "parameter": "count" }]
        }),
    ]
}

pub fn registry() -> Registry {
    let sources = fixture_manifests()
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    Registry::from_sources(&sources).unwrap()
}

pub fn write_manifests(root: &Path) -> PathBuf {
    let directory = root.join("diagnostic-manifests");
    std::fs::create_dir_all(&directory).unwrap();
    for manifest in fixture_manifests() {
        let name = manifest["name"].as_str().unwrap().replace('.', "_");
        std::fs::write(
            directory.join(format!("{name}.json")),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }
    directory
}

/// Representative invalid sources shared by analyzer-level and real-CLI tests.
/// Ten cases from each of twelve diagnostic paths meet the maintained corpus
/// threshold without making the corpus a hundred copies of one failure.
pub fn representative_invalid_sources() -> Vec<InvalidSource> {
    let mut corpus = Vec::new();
    for index in 0..10 {
        let name = format!("missing_{index}");
        corpus.push(InvalidSource {
            source: format!("use value {name}\n"),
            expected_code: "check.undefined_name",
            expected_token: name,
        });

        let name = format!("job_{index}");
        corpus.push(InvalidSource {
            source: format!("await {name}\n"),
            expected_code: "check.scope",
            expected_token: name,
        });

        let value = 300 + index;
        corpus.push(InvalidSource {
            source: format!("exit with status {value}\n"),
            expected_code: "check.range",
            expected_token: "status".to_owned(),
        });

        let name = format!("result_{index}");
        corpus.push(InvalidSource {
            source: format!("run \"true\" as {name}\nrun \"true\" as {name}\n"),
            expected_code: "check.duplicate_name",
            expected_token: name,
        });

        let name = format!("input_{index}");
        corpus.push(InvalidSource {
            source: format!("accept {name} as text\naccept {name} as text\n"),
            expected_code: "check.duplicate_name",
            expected_token: name,
        });

        let name = format!("step_{index}");
        corpus.push(InvalidSource {
            source: format!("{name}: run \"true\"\n{name}: run \"true\"\n"),
            expected_code: "check.duplicate_name",
            expected_token: name,
        });

        let program = format!("worker_{index}");
        let unknown = format!("workre_{index}");
        corpus.push(InvalidSource {
            source: format!(
                "define a program named {program}\n  run \"true\"\nrun program {unknown}\n"
            ),
            expected_code: "check.program.unknown",
            expected_token: unknown,
        });

        let program = format!("input_worker_{index}");
        let unknown_input = format!("inpt_{index}");
        corpus.push(InvalidSource {
            source: format!(
                "define a program named {program}\n  accept input as text\n  use value input\nrun program {program} with {unknown_input} \"x\"\n"
            ),
            expected_code: "check.program.input.unknown",
            expected_token: unknown_input,
        });

        let program = format!("required_worker_{index}");
        corpus.push(InvalidSource {
            source: format!(
                "define a program named {program}\n  accept required_{index} as text\n  use value required_{index}\nrun program {program}\n"
            ),
            expected_code: "check.program.input.required",
            expected_token: program,
        });

        let parameter = format!("parameter_{index}");
        corpus.push(InvalidSource {
            source: format!("run \"fixture-reader\" with {parameter} \"x\"\n"),
            expected_code: "check.parameter.direct",
            expected_token: parameter,
        });

        let parameter = format!("unknown_{index}");
        corpus.push(InvalidSource {
            source: format!("run fixture.count with {parameter} 1\n"),
            expected_code: "applet.parameter.unknown",
            expected_token: parameter,
        });

        let value = format!("\"wrong_{index}\"");
        corpus.push(InvalidSource {
            source: format!("run fixture.count with count {value}\n"),
            expected_code: "applet.parameter.type",
            expected_token: value,
        });
    }
    corpus
}
