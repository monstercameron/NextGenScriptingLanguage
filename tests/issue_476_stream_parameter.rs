//! Issue #476: a misspelt parameter in a stream applet call gets the finite validator's
//! `applet.parameter.unknown` diagnostic, with a span in the stream source and a fix.

use flow::diagnostics::Diagnostic;
use flow::manifest::Registry;

const FILTER: &str = r#"{
  "name": "fixture.filter",
  "binary": "cat",
  "accepts_stdin": true,
  "cacheable": false,
  "parameters": {
    "arguments": { "type": "list", "items": "string" }
  },
  "arguments": [
    {
      "when_present": "arguments",
      "append": [ { "spread": "arguments" } ]
    }
  ],
  "contract": {
    "version": 1,
    "backend": "command",
    "lifecycle": { "kind": "oneshot" },
    "output": { "type": "text", "encoding": "raw" },
    "capabilities": ["process.exec"],
    "effects": []
  }
}"#;

fn registry() -> Registry {
    Registry::from_sources(&[FILTER.to_owned()]).unwrap()
}

fn unknown_parameters(source: &str) -> Vec<Diagnostic> {
    flow::stream::parameter_diagnostics(source, &registry())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == "applet.parameter.unknown")
        .collect()
}

#[test]
fn misspelt_stream_stage_parameter_gets_a_span_and_a_fix() {
    let source = "stream from fixture.filter with arguments [\"a\"]\npipe to fixture.filter with argumnts [\"a\"]\nshow each line\n";
    let found = unknown_parameters(source);
    assert_eq!(found.len(), 1, "{found:#?}");
    let diagnostic = &found[0];
    assert_eq!(diagnostic.span.line, 2);
    assert_eq!(
        &source[diagnostic.span.start_byte..diagnostic.span.end_byte],
        "argumnts"
    );
    assert!(diagnostic.message.contains("did you mean \"arguments\""));
    let fix = diagnostic.fixes.first().expect("a suggested fix");
    assert_eq!(fix.edits.len(), 1);
    assert_eq!(fix.edits[0].replacement, "arguments");
    assert_eq!(
        &source[fix.edits[0].span.start_byte..fix.edits[0].span.end_byte],
        "argumnts"
    );
}

#[test]
fn misspelt_parameter_on_the_header_maps_to_the_stream_source() {
    let source = "stream from fixture.filter with argumnts [\"a\"]\nshow each line\n";
    let found = unknown_parameters(source);
    assert_eq!(found.len(), 1, "{found:#?}");
    let span = &found[0].span;
    assert_eq!(span.line, 1);
    assert_eq!(&source[span.start_byte..span.end_byte], "argumnts");
    assert_eq!(span.column, source.find("argumnts").unwrap() + 1);
    let edit = &found[0].fixes[0].edits[0];
    assert_eq!(
        &source[edit.span.start_byte..edit.span.end_byte],
        "argumnts"
    );
    assert_eq!(edit.replacement, "arguments");
}

#[test]
fn valid_stream_has_no_unknown_parameter_diagnostics() {
    let source = "stream from fixture.filter with arguments [\"a\"]\npipe to fixture.filter with arguments [\"b\"]\nshow each line\n";
    assert!(unknown_parameters(source).is_empty());
}

#[test]
fn unparsable_stream_gives_no_parameter_diagnostics() {
    assert!(unknown_parameters("stream from fixture.filter\n").is_empty());
}
