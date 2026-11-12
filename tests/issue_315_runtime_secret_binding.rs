//! Issue #315: a step result bound with `and keep secret` is registered with the
//! shared secret redactor when it is bound, so every later redaction point (records,
//! traces, live views, diagnostics) shows the placeholder instead of the value.
//! Measured through the public runtime API, so it runs on Windows and Unix.
use std::sync::{atomic::AtomicBool, Arc};

const CANARY: &str = "CANARY-315-RUNTIME-SECRET";

fn secret_output_source() -> String {
    // The step result is the command's output; the trailing newline is not part of the value.
    let command = if cfg!(windows) {
        format!("run \"cmd\" with arguments [\"/C\", \"echo {CANARY}\"] as token and keep secret\n")
    } else {
        format!("run \"sh\" with arguments [\"-c\", \"printf '%s' {CANARY}\"] as token and keep secret\n")
    };
    format!("{command}use value token\n")
}

#[test]
fn runtime_bound_secret_output_is_registered_before_later_steps() {
    let folder = tempfile::tempdir().unwrap();
    let program = flow::parser::parse(&secret_output_source()).unwrap();
    let registry = flow::manifest::Registry::default();
    let options = flow::runtime::RunOptions::new(folder.path().to_path_buf());
    flow::runtime::run_with_context(
        &program,
        &registry,
        &options,
        Default::default(),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
    )
    .expect("secret-marked command runs");

    let redactor = flow::security::secret_redactor(&registry);
    assert_eq!(
        redactor.redact_text(&format!("token={CANARY}")),
        "token=[REDACTED]"
    );
}
