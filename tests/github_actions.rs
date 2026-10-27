//! GitHub Actions hardening end to end: forged workflow commands are
//! neutralized in streamed output, final output and captured stderr under
//! `GITHUB_ACTIONS=true`, registered secrets emit `::add-mask::` lines, and
//! output stays byte-identical without the marker. Unix-only because the
//! fixtures drive `/bin/sh` through the default registry.
#![cfg(unix)]

use std::fs;
use std::process::{Command, Output};

const ZWSP: &str = "\u{200b}";

fn run_source(source: &str, args: &[&str], github_actions: bool) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("job.program");
    fs::write(&program, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.args(["run"]).arg(&program).args(args);
    command.current_dir(directory.path());
    // Keep the working tree hermetic: never inherit the ambient CI marker.
    // Hardening under test is opt-in per command below.
    command.env_remove("GITHUB_ACTIONS");
    if github_actions {
        command.env("GITHUB_ACTIONS", "true");
    }
    command.output().unwrap()
}

fn run_with_manifests(
    source: &str,
    manifests: &std::path::Path,
    github_actions: bool,
) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("job.program");
    fs::write(&program, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(["run"])
        .arg(&program)
        .args(["--manifest-dir"])
        .arg(manifests)
        .current_dir(directory.path());
    // Same hermetic marker handling as `run_source`.
    command.env_remove("GITHUB_ACTIONS");
    if github_actions {
        command.env("GITHUB_ACTIONS", "true");
    }
    command.output().unwrap()
}

fn inherit_manifests() -> tempfile::TempDir {
    // One applet whose manifest declares `inherit` stdout: without hardening
    // the child writes straight to the terminal descriptor.
    let root = tempfile::tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    fs::write(
        manifests.join("forge-inherit.json"),
        r#"{"name":"forge-inherit","binary":"sh","arguments":[{"literal":"-c"},{"literal":"printf '::error::via-inherit\n'"}],"stdio":{"stdout":"inherit"}}"#,
    )
    .unwrap();
    root
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_neutralized(text: &str, forged: &str) {
    // The visible text is preserved apart from the zero-width break.
    assert!(
        text.replace(ZWSP, "").contains(forged),
        "stripping the break should restore {forged:?} in {text:?}"
    );
    if let Some(offset) = forged.find("##[") {
        // Legacy commands are searched for anywhere in the line: the output
        // must not contain the raw `##[` window at all.
        assert!(
            !text.contains("##["),
            "legacy command leaked in {text:?}"
        );
        let mut neutralized = forged.to_owned();
        neutralized.replace_range(
            offset..offset + 3,
            &format!("#{ZWSP}#["),
        );
        assert!(
            text.contains(&neutralized),
            "neutralized command missing: {neutralized:?} in {text:?}"
        );
        return;
    }
    // Modern commands only run when the trimmed line starts with `::`: break
    // the opening prefix and the runner never reaches the rest, so only the
    // first pair is rewritten while the `::` before the data stays literal.
    assert!(
        !text.contains(forged),
        "forged command leaked: {forged:?} in {text:?}"
    );
    let neutralized = forged.replacen("::", &format!(":{ZWSP}:"), 1);
    assert!(
        text.contains(&neutralized),
        "neutralized command missing: {neutralized:?} in {text:?}"
    );
}

#[test]
fn streamed_child_forgery_is_neutralized_under_ci() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf '::stop-commands::x\\n::error::boom\\n'\"]\n";
    let output = run_source(source, &[], true);
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert_neutralized(&text, "::stop-commands::x");
    assert_neutralized(&text, "::error::boom");
}

#[test]
fn final_program_output_forgery_is_neutralized_under_ci() {
    // The second step captures, so its bytes leave through the final-output
    // write rather than the streamed path. Both share one line-state.
    let source = concat!(
        "run \"sh\" with arguments [\"-c\", \"printf 'first\\n'\"]\n",
        "run \"sh\" with arguments [\"-c\", \"printf '::error::final\\n'\"] as out\n",
        "use value out\n",
    );
    let output = run_source(source, &[], true);
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert_neutralized(&text, "::error::final");
    assert!(text.contains("first\n"), "{text:?}");
}

#[test]
fn successful_stderr_forgery_is_preserved_sanitized_under_ci() {
    let source =
        "run \"sh\" with arguments [\"-c\", \"printf '::error::via-stderr\\n' >&2\"]\n";
    let output = run_source(source, &[], true);
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stderr(&output);
    assert_neutralized(&text, "::error::via-stderr");
    // The success-path stderr is re-emitted, not swallowed.
    assert!(!text.is_empty(), "stderr was hidden instead of sanitized");
}

#[test]
fn multiline_and_midline_pairs_only_neutralize_line_leading_prefixes() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf 'ok\\n::notice::hi\\nmid a::b\\n  ::error::indented\\n'\"]\n";
    let output = run_source(source, &[], true);
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert_neutralized(&text, "::notice::hi");
    // The runner trims leading whitespace, so the indented command is
    // exploitable and must be neutralized too.
    assert_neutralized(&text, "::error::indented");
    // Mid-line pairs (Rust paths, URLs) are not commands and stay raw.
    assert!(text.contains("mid a::b\n"), "{text:?}");
}

#[test]
fn leading_whitespace_forgery_is_neutralized_under_ci() {
    // Spaces and tabs before `::` do not save the forgery: the runner trims
    // them before checking the prefix.
    let source = "run \"sh\" with arguments [\"-c\", \"printf '   ::error::spaces\\n\\t::warning::tab\\n'\"]\n";
    let output = run_source(source, &[], true);
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert_neutralized(&text, "::error::spaces");
    assert_neutralized(&text, "::warning::tab");
}

#[test]
fn legacy_hash_command_is_neutralized_anywhere_under_ci() {
    // The legacy runner searches for `##[` anywhere in the line.
    let source = "run \"sh\" with arguments [\"-c\", \"printf '##[error]legacy\\nmid ##[warning]x\\n'\"]\n";
    let output = run_source(source, &[], true);
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert_neutralized(&text, "##[error]legacy");
    assert_neutralized(&text, "##[warning]x");
    assert!(!text.contains("##["), "legacy window survived in {text:?}");
}

#[test]
fn registered_secret_emits_an_escaped_mask_line_under_ci() {
    // `%` must escape as `%25` in the mask line; the program output still
    // carries the raw value so the runner (not Execute) redacts it.
    let canary = "MASK-PCT-499-%-END";
    let source = concat!(
        "accept token as text from secret prompt \"Token?\"\n",
        "run \"sh\" with arguments [\"-c\", \"printf '%s\\n' \\\"$TOKEN\\\"\"] and environment {\"TOKEN\": token}\n",
    );
    let output = run_source(
        source,
        &[ "--input", &format!("token={canary}") ],
        true,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    let escaped = canary.replace('%', "%25");
    assert!(
        text.contains(&format!("::add-mask::{escaped}")),
        "escaped mask line missing in {text:?}"
    );
    // The mask line itself is an intentional workflow command and stays raw.
    assert!(text.contains("::add-mask::"), "{text:?}");
}

#[test]
fn output_stays_raw_without_the_ci_marker() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf '::error::raw\\n'\"]\n";
    let output = run_source(source, &[], false);
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("::error::raw\n"), "{text:?}");
    assert!(!text.contains(ZWSP), "non-CI output was rewritten: {text:?}");
}

#[test]
fn once_final_output_forgery_is_neutralized_under_ci() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("nightly.program");
    fs::write(
        &program,
        "on schedule \"30 9 * * 1-5\"\n  run \"sh\" with arguments [\"-c\", \"printf '::error::once-final\\n'\"] as out\n  use value out\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "--once"])
        .arg(&program)
        .current_dir(directory.path())
        .env_remove("GITHUB_ACTIONS")
        .env("GITHUB_ACTIONS", "true")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_neutralized(&text, "::error::once-final");
}

#[test]
fn manifest_inherit_forgery_is_neutralized_under_ci() {
    // A manifest `inherit` disposition must not bypass sanitization: under
    // hardening the stream is force-captured and re-emitted filtered.
    let root = inherit_manifests();
    let output = run_with_manifests(
        "run forge-inherit\n",
        &root.path().join("manifests"),
        true,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    assert_neutralized(&stdout(&output), "::error::via-inherit");
}

#[test]
fn manifest_inherit_output_stays_raw_without_the_ci_marker() {
    let root = inherit_manifests();
    let output = run_with_manifests(
        "run forge-inherit\n",
        &root.path().join("manifests"),
        false,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("::error::via-inherit\n"), "{text:?}");
    assert!(!text.contains(ZWSP), "non-CI output was rewritten: {text:?}");
}

#[test]
fn handover_is_refused_under_ci_before_the_child_runs() {
    // Post-`exec` bytes cannot be sanitized, so hardening fails closed: the
    // run is refused before any step runs and the child never starts.
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("handover-ran.txt");
    let program = directory.path().join("job.program");
    fs::write(
        &program,
        format!(
            "hand over to \"/bin/sh\" with arguments [\"-c\", \"printf '::error::handover-raw\\n'; touch '{}'\"]\n",
            marker.display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run"])
        .arg(&program)
        .current_dir(directory.path())
        .env_remove("GITHUB_ACTIONS")
        .env("GITHUB_ACTIONS", "true")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "handover ran under hardening: {output:?}"
    );
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("`hand over to` is refused"),
        "refusal missing in {diagnostic:?}"
    );
    assert!(!marker.exists(), "the refused child must never run");
    assert!(
        !stdout(&output).contains("::error::handover-raw"),
        "forged command reached the log: {:?}",
        stdout(&output)
    );
}
