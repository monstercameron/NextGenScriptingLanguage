//! Issue #378: `redact.text` exposes the engine's second pass as `require_clean`.
//! When the redacted output still contains a detectable value the applet fails
//! without echoing the input or the matched value.

use flow::applets::execute_in_cwd;
use serde_json::{json, Map, Value};
use std::path::Path;

fn redact(args: Value) -> Result<Value, flow::applets::AppletError> {
    let map: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    execute_in_cwd("redact.text", &map, b"", Path::new("."))
}

#[test]
fn require_clean_passes_clean_output() {
    let output = redact(json!({
        "text": "mail ops@example.com key secret-x",
        "literals": ["secret-x"],
        "require_clean": true,
    }))
    .expect("clean output must pass");
    assert_eq!(output["text"], "mail <email-1> key <literal-1>");
}

#[test]
fn require_clean_fails_when_a_value_survives_masking() {
    // The masked text becomes "value <literal-1> here", which still contains the
    // literal text, so the second pass must refuse it.
    let error = match redact(json!({
        "text": "value literal-1 here",
        "literals": ["literal-1"],
        "require_clean": true,
    })) {
        Ok(output) => panic!("leftover must fail the step, got {output}"),
        Err(error) => error,
    };
    assert_eq!(error.code, "not_clean");
    assert!(
        !error.message.contains("literal-1"),
        "the failure must not echo the matched value: {}",
        error.message
    );
}

#[test]
fn require_clean_defaults_to_off() {
    let output = redact(json!({
        "text": "value literal-1 here",
        "literals": ["literal-1"],
    }))
    .expect("without require_clean the redacted text is returned");
    assert_eq!(output["text"], "value <literal-1> here");
}

#[test]
fn require_clean_must_be_a_boolean() {
    assert!(redact(json!({
        "text": "plain",
        "require_clean": "yes",
    }))
    .is_err());
}
