//! Declared inputs from environment, positional arguments, flags, options,
//! program location, and prompts, plus the command-line input type boundary.
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn invoke(root: &Path, source: &str, args: &[&str]) -> Output {
    fs::write(root.join("job.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("job.program")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn environment_and_positional_sources_bind_and_named_inputs_override_them() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("job.program"),
        "accept env as text from environment \"EXECUTE_INPUTS_ENV_CASE\"\
         \naccept first as text from argument 1\
         \naccept second as text from argument 2\
         \naccept tail as list of text from arguments\
         \nthen encode [env, first, second, tail] as json\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "job.program", "--", "first", "second"])
        .current_dir(root.path())
        .env("EXECUTE_INPUTS_ENV_CASE", "from-env")
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        serde_json::from_str::<Value>(&text(&output)).unwrap(),
        serde_json::json!(["from-env", "first", "second", ["first", "second"]])
    );

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            "job.program",
            "--input",
            "env=explicit",
            "--input",
            "first=named",
            "--",
            "positional",
            "other",
        ])
        .current_dir(root.path())
        .env("EXECUTE_INPUTS_ENV_CASE", "ambient")
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        serde_json::from_str::<Value>(&text(&output)).unwrap(),
        serde_json::json!(["explicit", "named", "other", ["positional", "other"]])
    );
}

#[test]
fn flags_options_aliases_and_defaults_are_parsed_after_double_dash() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept verbose as boolean from flag \"--verbose\" alias \"-v\"\
                  \naccept output as text from option \"--output\" alias \"-o\" with default \"out.txt\"\
                  \naccept count as integer from option \"--count\"\
                  \nthen encode [verbose, output, count] as json\n";
    let output = invoke(
        root.path(),
        source,
        &["--", "-v", "-o", "a b.txt", "--count=7"],
    );
    success(&output);
    assert_eq!(
        serde_json::from_str::<Value>(&text(&output)).unwrap(),
        serde_json::json!([true, "a b.txt", 7])
    );
    let output = invoke(root.path(), source, &["--", "--count", "3"]);
    success(&output);
    assert_eq!(
        serde_json::from_str::<Value>(&text(&output)).unwrap(),
        serde_json::json!([false, "out.txt", 3])
    );
}

#[test]
fn location_inputs_are_the_program_file_directory_and_path_and_can_be_overridden() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept here as text from program directory\
                  \naccept self as text from program path\
                  \nthen encode [here, self] as json\n";
    let output = invoke(root.path(), source, &[]);
    success(&output);
    let values: Value = serde_json::from_str(&text(&output)).unwrap();
    // `canonicalize` returns the verbatim `\\?\` spelling on Windows; the language
    // reports the ordinary spelling of the same path.
    let plain = |path: std::path::PathBuf| {
        let text = path.canonicalize().unwrap().to_string_lossy().into_owned();
        text.strip_prefix(r"\\?\")
            .map(str::to_owned)
            .unwrap_or(text)
    };
    assert_eq!(values[0], plain(root.path().to_path_buf()).as_str());
    assert_eq!(values[1], plain(root.path().join("job.program")).as_str());
    let output = invoke(root.path(), source, &["--input", "here=override"]);
    success(&output);
    let values: Value = serde_json::from_str(&text(&output)).unwrap();
    assert_eq!(values[0], "override");
}

#[test]
fn command_line_input_schema_errors_prevent_effects() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        ("accept count as integer\n", "count=not-an-integer"),
        ("accept enabled as boolean\n", "enabled=yes"),
        ("accept items as list of text\n", "items=[1]"),
        ("accept data as json\n", "data={broken"),
    ];
    for (declaration, input) in cases {
        let source = format!(
            "{declaration}run \"printf\" with arguments [\"effect\"]\nthen save to \"marker.txt\"\n"
        );
        let output = invoke(root.path(), &source, &["--input", input]);
        assert!(!output.status.success(), "{input}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(input.split('=').nth(1).unwrap()),
            "invalid supplied value should not be echoed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.path().join("marker.txt").exists(), "{input}");
    }
}

#[test]
fn an_unanswered_secret_prompt_fails_without_a_terminal_before_any_step() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("job.program"),
        "accept token as text from secret prompt \"Access token?\"\nuse value \"ran\"\nthen save to \"marker.txt\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "job.program"])
        .current_dir(root.path())
        .env("EXECUTE_NO_PROMPT", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Access token?"), "{error}");
    assert!(!root.path().join("marker.txt").exists());
}
