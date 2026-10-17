//! Keeps docs/grammar.md honest: every ```program example parses with the real parser,
//! and the rejections the modifier table states are rejected.

use std::fs;
use std::path::Path;

fn grammar() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/grammar.md");
    fs::read_to_string(path).unwrap()
}

/// The bodies of ```program fences, with the line where each fence opens.
fn program_examples(text: &str) -> Vec<(usize, String)> {
    let mut examples = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        match &mut current {
            None if line.trim_end() == "```program" => current = Some((index + 1, String::new())),
            Some((start, body)) if line.trim_end() == "```" => {
                examples.push((*start, std::mem::take(body)));
                current = None;
            }
            Some((_, body)) => {
                body.push_str(line);
                body.push('\n');
            }
            None => {}
        }
    }
    assert!(current.is_none(), "an unterminated ```program fence");
    examples
}

#[test]
fn every_program_example_parses_with_the_real_parser() {
    let examples = program_examples(&grammar());
    assert!(
        examples.len() >= 10,
        "expected the grammar's examples, found {}",
        examples.len()
    );
    let mut failures = Vec::new();
    for (line, source) in &examples {
        if let Err(error) = flow::parser::parse(source) {
            failures.push(format!("example at line {line}: {error:#}\n{source}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

#[test]
fn pipe_to_as_binds_the_terminal_stage_output_and_runs() {
    // `as NAME` on a `pipe to` stage binds the pipe group's final output (the terminal
    // stage's stdout). The group therefore prints nothing, and a later statement can
    // read NAME. See tests/lane_b_streaming.rs for the streaming rule this depends on.
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("pipe-binding.program");
    fs::write(
        &program,
        "run printf with message \"a\"\n\
         pipe to tr with from \"[:lower:]\" and to \"[:upper:]\" as shouted\n\
         run printf with message shouted\n",
    )
    .unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "A\n");
}

#[test]
fn applets_reject_and_discard_stderr_at_check_time() {
    // `and discard stderr` applies to direct executables only. Named applets are
    // rejected by `check` against the manifest, not by the parser.
    let directory = std::env::temp_dir().join(format!("grammar-doc-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let program = directory.join("applet.program");
    fs::write(
        &program,
        "run text.trim_final_lf with text \"x\" and discard stderr\n",
    )
    .unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&manifests)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&directory);
    assert!(
        !output.status.success(),
        "applet accepted `and discard stderr`"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("has no parameter"), "{stderr}");
}

#[test]
fn the_extractor_finds_each_fence_once() {
    let sample = "text\n```program\nrun \"true\"\n```\n```text\nnot a program\n```\n";
    let examples = program_examples(sample);
    assert_eq!(examples, vec![(2, "run \"true\"\n".to_owned())]);
}
