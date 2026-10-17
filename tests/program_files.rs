//! `run program file "PATH"` and `from program directory|path`: a program's location
//! comes from its own file, and a called file runs with explicit inputs only.
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn execute(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A canonical path as the loader reports it, without the Windows verbatim prefix.
fn canonical(path: &Path) -> String {
    let text = fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let stripped = text.strip_prefix(r"\\?\").map(str::to_owned);
    stripped.unwrap_or(text)
}

fn inspect(cwd: &Path, program: &Path, extra: &[&str]) -> Value {
    let output = execute(
        cwd,
        &[&["inspect", program.to_str().unwrap()], extra].concat(),
    );
    assert!(output.status.success(), "{}", stderr_of(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn location_inputs_come_from_the_program_file_wherever_it_runs() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "app/main.program",
        "accept here as text from program directory\naccept where as text from program path\nuse value here\nthen save to \"out.txt\"\n",
    );
    let main = root.path().join("app/main.program");
    let elsewhere = root.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();

    let report = inspect(&elsewhere, &main, &[]);
    let inputs = report["inputs"].as_array().unwrap();
    assert_eq!(inputs[0]["value"], canonical(&root.path().join("app")));
    assert_eq!(inputs[0]["source"]["kind"], "program_directory");
    assert_eq!(inputs[1]["value"], canonical(&main));
    assert_eq!(inputs[1]["source"]["kind"], "program_path");
    assert_eq!(report["inputs_complete"], true);

    let overridden = inspect(&elsewhere, &main, &["--input", "here=custom"]);
    assert_eq!(overridden["inputs"][0]["value"], "custom");

    let output = execute(&elsewhere, &["run", main.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(
        fs::read_to_string(elsewhere.join("out.txt")).unwrap(),
        canonical(&root.path().join("app"))
    );
}

#[test]
fn a_called_file_gets_explicit_inputs_and_runs_in_order() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "main.program",
        "accept name as text with default \"world\"\nrun program file \"lib/echo.program\" with who name as echoed\nuse value echoed\nthen save to \"out.txt\"\n",
    );
    write(
        root.path(),
        "lib/echo.program",
        "accept who as text\nuse value who\nthen save to \"echo.txt\"\n",
    );
    let output = execute(root.path(), &["run", "main.program"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "world"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("echo.txt")).unwrap(),
        "world"
    );

    let output = execute(root.path(), &["run", "main.program", "--input", "name=ada"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "ada"
    );
}

#[test]
fn a_called_file_resolves_its_own_location() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "main.program",
        "run program file \"lib/where.program\"\n",
    );
    write(
        root.path(),
        "lib/where.program",
        "accept here as text from program directory\nuse value here\nthen save to \"where.txt\"\n",
    );
    let elsewhere = root.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let output = execute(
        &elsewhere,
        &["run", root.path().join("main.program").to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(
        fs::read_to_string(elsewhere.join("where.txt")).unwrap(),
        canonical(&root.path().join("lib"))
    );
}

#[test]
fn a_bad_call_input_is_refused_before_the_first_effect() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "main.program",
        "use value \"started\"\nthen save to \"before.txt\"\nrun program file \"lib/typed.program\" with n \"oops\" as r\n",
    );
    write(
        root.path(),
        "lib/typed.program",
        "accept n as integer\nuse value \"x\"\nthen save to \"typed.txt\"\n",
    );
    let output = execute(root.path(), &["run", "main.program"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("integer"),
        "{}",
        stderr_of(&output)
    );
    assert!(!root.path().join("before.txt").exists());
    assert!(!root.path().join("typed.txt").exists());
}

#[test]
fn file_call_cycles_are_refused_when_the_program_loads() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "main.program",
        "run program file \"lib/a.program\"\n",
    );
    write(
        root.path(),
        "lib/a.program",
        "run program file \"b.program\"\n",
    );
    write(
        root.path(),
        "lib/b.program",
        "run program file \"a.program\"\n",
    );
    let output = execute(root.path(), &["run", "main.program"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("cycle"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn called_files_keep_their_own_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        (
            "lib/trigger.program",
            "every 1 hour\n  use value \"x\"\n  then save to \"tick.txt\"\n",
            "cannot declare a trigger",
        ),
        (
            "lib/env.program",
            "accept who as text from environment \"EXECUTE_FILE_CALL_TEST\"\nuse value who\n",
            "must be passed explicitly",
        ),
        (
            "lib/defs.program",
            "define a program named helper\n  use value \"x\"\nrun program helper\n",
            "cannot define programs",
        ),
        ("lib/plain.program", "use value \"x\"\n", ""),
    ];
    for (file, body, expected) in cases {
        write(root.path(), file, body);
        let name = file.trim_start_matches("lib/").trim_end_matches(".program");
        write(
            root.path(),
            "main.program",
            &format!("run program file \"{file}\"\n"),
        );
        let output = execute(root.path(), &["run", "main.program"]);
        if expected.is_empty() {
            assert!(output.status.success(), "{name}: {}", stderr_of(&output));
        } else {
            assert!(!output.status.success(), "{name} should be refused");
            assert!(
                stderr_of(&output).contains(expected),
                "{name}: {}",
                stderr_of(&output)
            );
        }
    }
    assert!(!root.path().join("tick.txt").exists());
}

#[test]
fn a_missing_called_file_is_named() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "main.program",
        "run program file \"lib/nope.program\"\n",
    );
    let output = execute(root.path(), &["run", "main.program"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("not found"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn source_text_without_its_file_refuses_file_calls_and_bad_location_inputs() {
    let refused = flow::parser::parse("run program file \"lib/x.program\"\n").unwrap_err();
    assert!(
        format!("{refused:#}").contains("needs the program loaded from its file"),
        "{refused:#}"
    );
    let default = flow::parser::parse(
        "accept here as text from program directory with default \"x\"\nuse value here\n",
    )
    .unwrap_err();
    assert!(
        format!("{default:#}").contains("cannot declare a default"),
        "{default:#}"
    );
    let integer = flow::parser::parse("accept here as integer from program path\n").unwrap_err();
    assert!(
        format!("{integer:#}").contains("must be text"),
        "{integer:#}"
    );
}
