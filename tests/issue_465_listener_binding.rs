//! A `whenever ... as <name>` listener binds `<name>` for each source record. The static
//! checker must let the handler body read that binding, while a name that is truly
//! undefined inside a handler is still reported.

use flow::diagnostics::{analyze_source, Diagnostic};
use flow::manifest::Registry;
use std::path::Path;

fn undefined_result_errors(source: &str) -> Vec<Diagnostic> {
    analyze_source(source, &Registry::default())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == "check.undefined_name")
        .collect()
}

fn example_source(name: &str) -> String {
    // Program text only: nothing here runs an example binary.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("examples/{name}"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

#[test]
fn listener_handler_sees_trigger_binding() {
    let source = "listen to printf with message \"first\\nsecond\"\nwhenever a line arrives as record\n  run printf with message record\n";
    let errors = undefined_result_errors(source);
    assert!(
        errors.is_empty(),
        "trigger binding reported as undefined: {:?}",
        errors
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn listener_examples_report_no_undefined_result() {
    for name in [
        "file_listener.program",
        "native-log-listener.program",
        "json-event-names.program",
    ] {
        let source = example_source(name);
        let errors = undefined_result_errors(&source);
        assert!(
            errors.is_empty(),
            "{name} reports undefined results: {:?}",
            errors
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn undefined_name_inside_listener_handler_is_still_reported() {
    let source = "listen to printf with message \"first\"\nwhenever a line arrives as record\n  run printf with message missing\n";
    let errors = undefined_result_errors(source);
    assert_eq!(errors.len(), 1, "expected one undefined result: {errors:?}");
    assert!(
        errors[0].message.contains("\"missing\""),
        "message should name the undefined result: {}",
        errors[0].message
    );
}
