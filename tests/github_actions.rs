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

fn run_with_manifests(source: &str, manifests: &std::path::Path, github_actions: bool) -> Output {
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

fn run_merged_source(source: &str) -> String {
    // Both descriptors share one file sink, the way the runner's merged job
    // log shares lines across stdout and stderr. Separate pipes would hide a
    // mask glued across streams; the shared file shows the merged byte order.
    use std::process::Stdio;
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("job.program");
    fs::write(&program, source).unwrap();
    let merged_path = directory.path().join("merged.log");
    let stdout_file = fs::File::create(&merged_path).unwrap();
    let stderr_file = stdout_file.try_clone().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run"])
        .arg(&program)
        .current_dir(directory.path())
        .env_remove("GITHUB_ACTIONS")
        .env("GITHUB_ACTIONS", "true")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .status()
        .unwrap();
    assert!(status.success());
    fs::read_to_string(&merged_path).unwrap()
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

/// Minimal model of the GitHub runner (V2) workflow-command parser: it
/// splits the log into lines and recognizes a `::` command only when the
/// line starts with `::` after trimming leading whitespace. A mask glued
/// onto a previous partial line (`lead::add-mask::...`) is trailing garbage
/// the runner never executes, so the secret would stay unmasked. Neutralized
/// prefixes (`:` + zero-width space + `:`) do not parse, which is the point.
fn runner_commands(text: &str) -> Vec<(String, String)> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("::")?;
            match rest.find("::") {
                Some(offset) => Some((rest[..offset].to_owned(), rest[offset + 2..].to_owned())),
                None => Some((rest.to_owned(), String::new())),
            }
        })
        .collect()
}

/// The log without runner mask lines: mask *data* may legitimately hold
/// secret bytes — including forged-looking ones from masked literals —
/// because the runner treats the whole line as opaque data for one
/// `add-mask` command. Neutralization is checked on the other lines.
fn non_mask_text(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("::add-mask::"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `::add-mask::` occurrence in the log must open its own line (after
/// trimming leading whitespace), or the runner never executes it. A mask
/// glued onto a previous partial line (`lead::add-mask::...`) is the
/// regression this guards against.
fn assert_no_glued_masks(text: &str) {
    for line in text.lines() {
        if line.contains("::add-mask::") {
            assert!(
                line.trim_start().starts_with("::add-mask::"),
                "mask glued mid-line in {text:?}"
            );
        }
    }
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
        assert!(!text.contains("##["), "legacy command leaked in {text:?}");
        let mut neutralized = forged.to_owned();
        neutralized.replace_range(offset..offset + 3, &format!("#{ZWSP}#["));
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
    let source =
        "run \"sh\" with arguments [\"-c\", \"printf '::stop-commands::x\\n::error::boom\\n'\"]\n";
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
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
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert_neutralized(&text, "::error::final");
    assert!(text.contains("first\n"), "{text:?}");
}

#[test]
fn successful_stderr_forgery_is_preserved_sanitized_under_ci() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf '::error::via-stderr\\n' >&2\"]\n";
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stderr(&output);
    assert_neutralized(&text, "::error::via-stderr");
    // The success-path stderr is re-emitted, not swallowed.
    assert!(!text.is_empty(), "stderr was hidden instead of sanitized");
}

#[test]
fn multiline_and_midline_pairs_only_neutralize_line_leading_prefixes() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf 'ok\\n::notice::hi\\nmid a::b\\n  ::error::indented\\n'\"]\n";
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
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
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert_neutralized(&text, "::error::spaces");
    assert_neutralized(&text, "::warning::tab");
}

#[test]
fn legacy_hash_command_is_neutralized_anywhere_under_ci() {
    // The legacy runner searches for `##[` anywhere in the line.
    let source =
        "run \"sh\" with arguments [\"-c\", \"printf '##[error]legacy\\nmid ##[warning]x\\n'\"]\n";
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
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
    let output = run_source(source, &["--input", &format!("token={canary}")], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
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
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("::error::raw\n"), "{text:?}");
    assert!(
        !text.contains(ZWSP),
        "non-CI output was rewritten: {text:?}"
    );
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
    let output = run_with_manifests("run forge-inherit\n", &root.path().join("manifests"), true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_neutralized(&stdout(&output), "::error::via-inherit");
}

#[test]
fn manifest_inherit_output_stays_raw_without_the_ci_marker() {
    let root = inherit_manifests();
    let output = run_with_manifests("run forge-inherit\n", &root.path().join("manifests"), false);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("::error::via-inherit\n"), "{text:?}");
    assert!(
        !text.contains(ZWSP),
        "non-CI output was rewritten: {text:?}"
    );
}

#[test]
fn sensitive_streamed_value_is_masked_before_it_is_echoed() {
    // A sensitive step's bytes are registered (mask first) before the value
    // can reach the log. The `keep secret` literals are masked too, so their
    // mask *data* legitimately holds forged-looking bytes; the runner treats
    // each mask line as opaque data, and no forged command may parse.
    let source = concat!(
        "run \"sh\" with arguments [\"-c\", \"printf 'lead'\"]\n",
        "run \"sh\" with arguments [\"-c\", \"printf '::error::masked-forgery\\n'\"] as token and keep secret\n",
    );
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert_no_glued_masks(&text);
    let commands = runner_commands(&text);
    assert!(
        commands.iter().any(|(name, _)| name == "add-mask"),
        "no mask recognized in {text:?}"
    );
    assert!(
        commands.iter().all(|(name, _)| name == "add-mask"),
        "forgery parsed as a command in {text:?} (parsed: {commands:?})"
    );
}

#[test]
fn masks_start_their_own_lines_after_partial_output() {
    // Regression for the mid-line mask (`lead::add-mask::...`): a step
    // streaming `lead` with no trailing newline is followed by a sensitive
    // step whose value is itself a forged command, then by a plain step
    // streaming another forged command. Every mask must be its own log line
    // or the runner never executes it and the secret stays unmasked, while
    // neither forged value may parse: the post-mask one is neutralized
    // because the out-of-band mask resynced the sanitizer to line-start.
    let source = concat!(
        "run \"sh\" with arguments [\"-c\", \"printf 'lead'\"]\n",
        "run \"sh\" with arguments [\"-c\", \"printf '::error::masked-forgery\\n'\"] as token and keep secret\n",
        "run \"sh\" with arguments [\"-c\", \"printf '::error::after-mask\\n'\"]\n",
    );
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert_no_glued_masks(&text);
    assert!(
        text.contains("lead\n::add-mask::::error::masked-forgery%0A\n"),
        "mask did not open its own line after the partial write in {text:?}"
    );
    let commands = runner_commands(&text);
    // The escaped sensitive value round-trips through its mask command.
    assert!(
        commands.contains(&(
            "add-mask".to_owned(),
            "::error::masked-forgery%0A".to_owned()
        )),
        "value mask missing in {text:?} (parsed: {commands:?})"
    );
    // Neither forged command parses: the literals only travel as mask data,
    // and the post-mask copy is neutralized.
    assert!(
        commands.iter().all(|(name, _)| name == "add-mask"),
        "forgery parsed as a command in {text:?} (parsed: {commands:?})"
    );
    assert_neutralized(&non_mask_text(&text), "::error::after-mask");
}

#[test]
fn masks_start_a_fresh_line_after_partial_stderr() {
    // The merged job log shares lines across streams: a partial stderr line
    // must not glue the next stdout mask mid-line either, even though
    // stdout alone never left its line start. The break terminates the
    // stderr line in the merged view (a blank line in the isolated stdout
    // pipe), and the mask still parses.
    let source = concat!(
        "run \"sh\" with arguments [\"-c\", \"printf 'err-lead' >&2\"]\n",
        "run \"sh\" with arguments [\"-c\", \"printf 'value\\n'\"] as token and keep secret\n",
    );
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stderr(&output), "err-lead");
    let text = stdout(&output);
    assert_no_glued_masks(&text);
    assert!(
        text.contains("\n::add-mask::value%0A\n"),
        "mask did not open a fresh line in {text:?}"
    );
    let commands = runner_commands(&text);
    assert!(
        commands.contains(&("add-mask".to_owned(), "value%0A".to_owned())),
        "value mask missing in {text:?} (parsed: {commands:?})"
    );
    assert!(
        commands.iter().all(|(name, _)| name == "add-mask"),
        "unexpected runner command in {text:?} (parsed: {commands:?})"
    );
}

#[test]
fn stderr_file_redirect_stays_raw_under_ci() {
    // File redirect targets are not the job log: their bytes stay raw under
    // hardening, and forcing their capture must not leak them to stderr.
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("job.program");
    fs::write(
        &program,
        "run \"sh\" with arguments [\"-c\", \"printf '::error::in-file\\n' >&2\"] and stderr to \"err.txt\"\n",
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
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let filed = fs::read_to_string(directory.path().join("err.txt")).unwrap();
    assert!(
        filed.contains("::error::in-file\n"),
        "redirect file was rewritten: {filed:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("::error::in-file"),
        "file bytes leaked to the log: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
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

#[test]
fn masks_parse_in_a_merged_stdout_stderr_file_sink() {
    // The runner parses one merged log, not two isolated pipes: a stderr
    // partial sharing lines with stdout must not glue the next stdout mask
    // mid-line. Both descriptors point at one file here, so the test observes
    // the merged byte order the pipe-split assertions cannot see.
    let source = concat!(
        "run \"sh\" with arguments [\"-c\", \"printf 'lead'\"]\n",
        "run \"sh\" with arguments [\"-c\", \"printf 'err-lead' >&2\"]\n",
        "run \"sh\" with arguments [\"-c\", \"printf '::error::masked-forgery\\n'\"] as token and keep secret\n",
        "run \"sh\" with arguments [\"-c\", \"printf '::error::after-mask\\n'\"]\n",
    );
    let text = run_merged_source(source);
    assert_no_glued_masks(&text);
    let commands = runner_commands(&text);
    assert!(
        commands.iter().any(|(name, _)| name == "add-mask"),
        "no mask recognized in merged sink {text:?}"
    );
    assert!(
        commands.iter().all(|(name, _)| name == "add-mask"),
        "forgery parsed as a command in merged sink {text:?} (parsed: {commands:?})"
    );
    assert_neutralized(&non_mask_text(&text), "::error::after-mask");
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

#[test]
fn stdout_to_stderr_success_reaches_stderr_once_in_order_under_ci() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf 'out-line-499\\n'; printf 'err-line-499\\n' >&2\"] and stdout to stderr\n";
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    // A `stdout to stderr` redirect leaves nothing for the pipeline.
    assert!(stdout(&output).is_empty(), "stdout: {:?}", stdout(&output));
    let err = stderr(&output);
    let out_pos = err.find("out-line-499").expect("stdout must reach stderr");
    let err_pos = err
        .find("err-line-499")
        .expect("command stderr must reach stderr");
    assert!(out_pos < err_pos, "program order lost in {err:?}");
    assert_eq!(
        count_occurrences(&err, "out-line-499"),
        1,
        "duplicated stdout in {err:?}"
    );
    assert_eq!(
        count_occurrences(&err, "err-line-499"),
        1,
        "duplicated stderr in {err:?}"
    );
}

#[test]
fn stdout_to_stderr_failure_keeps_order_without_duplication_under_ci() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf 'fail-out-499\\n'; printf 'fail-err-499\\n' >&2; exit 1\"] and stdout to stderr\n";
    let output = run_source(source, &[], true);
    assert!(
        !output.status.success(),
        "failure must propagate: {output:?}"
    );
    assert!(stdout(&output).is_empty(), "stdout: {:?}", stdout(&output));
    let err = stderr(&output);
    let out_pos = err
        .find("fail-out-499")
        .expect("failed stdout must reach stderr");
    let err_pos = err
        .find("fail-err-499")
        .expect("failed stderr must reach stderr");
    assert!(out_pos < err_pos, "failure order lost in {err:?}");
    assert_eq!(
        count_occurrences(&err, "fail-out-499"),
        1,
        "duplicated stdout in {err:?}"
    );
    assert_eq!(
        count_occurrences(&err, "fail-err-499"),
        1,
        "duplicated stderr in {err:?}"
    );
}

#[test]
fn stdout_to_stderr_timeout_keeps_partial_bytes_sanitized_under_ci() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf 'early-out-499\\n'; printf 'early-err-499\\n' >&2; sleep 6; printf 'late-out-499\\n'\"] and stdout to stderr\n";
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("job.program");
    fs::write(&program, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run"])
        .arg(&program)
        .args(["--command-timeout-ms", "1500"])
        .current_dir(directory.path())
        .env_remove("GITHUB_ACTIONS")
        .env("GITHUB_ACTIONS", "true")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "the command must time out: {output:?}"
    );
    assert!(stdout(&output).is_empty(), "stdout: {:?}", stdout(&output));
    let err = stderr(&output);
    assert!(
        err.contains("early-out-499"),
        "partial stdout lost in {err:?}"
    );
    assert!(
        err.contains("early-err-499"),
        "partial stderr lost in {err:?}"
    );
    assert!(
        !err.contains("late-out-499"),
        "post-timeout bytes leaked in {err:?}"
    );
    assert_eq!(
        count_occurrences(&err, "early-out-499"),
        1,
        "duplicated partial stdout in {err:?}"
    );
    assert_eq!(
        count_occurrences(&err, "early-err-499"),
        1,
        "duplicated partial stderr in {err:?}"
    );
}

#[test]
fn stdout_to_stderr_forgery_is_neutralized_under_ci() {
    let source = "run \"sh\" with arguments [\"-c\", \"printf '::error::via-redirect-499\\n'\"] and stdout to stderr\n";
    let output = run_source(source, &[], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty(), "stdout: {:?}", stdout(&output));
    let err = stderr(&output);
    assert_neutralized(&err, "::error::via-redirect-499");
    assert!(
        runner_commands(&err).is_empty(),
        "forgery parsed in {err:?}"
    );
}

#[test]
fn stdout_to_stderr_sensitive_value_is_masked_before_it_is_echoed_under_ci() {
    let canary = "SECRET-STDERR-499-XYZ";
    let source = concat!(
        "accept token as text from secret prompt \"Token?\"\n",
        "run \"sh\" with arguments [\"-c\", \"printf '%s\\n' \\\"$TOKEN\\\"\"] and environment {\"TOKEN\": token} and stdout to stderr\n",
    );
    let output = run_source(source, &["--input", &format!("token={canary}")], true);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let out = stdout(&output);
    let err = stderr(&output);
    // Masks are emitted to stdout even when the value travels on stderr; the
    // merged runner log sees the mask first and redacts the value.
    assert!(
        out.contains(&format!("::add-mask::{canary}")),
        "mask missing in {out:?}"
    );
    assert_no_glued_masks(&out);
    // The value reaches stderr exactly once; the runner-side mask (above)
    // is what redacts it in the merged log, so Execute must not duplicate it.
    assert_eq!(
        count_occurrences(&err, canary),
        1,
        "sensitive value duplicated or lost in {err:?}"
    );
    assert!(
        runner_commands(&format!("{out}\n{err}"))
            .iter()
            .all(|(name, _)| name == "add-mask"),
        "unexpected runner command in {out:?} / {err:?}"
    );
}

#[test]
fn stdout_to_stderr_sensitive_failure_never_leaks_unmasked_bytes_under_ci() {
    // A failing sensitive step registers its captured stdout and stderr as
    // secrets before anything reaches the log. The fail-closed policy then
    // redacts the whole failure stderr to `[REDACTED]` — including nonsecret
    // bytes — so the test asserts the conservative marker, never the raw
    // `fail-err-499` text. The captured stdout canary still reaches stderr
    // exactly once (runner-masked by the stdout mask line).
    let canary = "SECRET-FAIL-499-XYZ";
    let source = concat!(
        "accept token as text from secret prompt \"Token?\"\n",
        "run \"sh\" with arguments [\"-c\", \"printf '%s\\n' \\\"$TOKEN\\\"; printf 'fail-err-499\\n' >&2; exit 1\"] and environment {\"TOKEN\": token} and stdout to stderr\n",
    );
    let output = run_source(source, &["--input", &format!("token={canary}")], true);
    assert!(
        !output.status.success(),
        "failure must propagate: {output:?}"
    );
    let out = stdout(&output);
    let err = stderr(&output);
    assert!(
        out.contains(&format!("::add-mask::{canary}")),
        "failure mask missing in {out:?}"
    );
    assert_no_glued_masks(&out);
    // Captured stdout reaches stderr once in program order (before the
    // diagnostic); the runner mask above is what redacts it in the merged log.
    assert_eq!(
        count_occurrences(&err, canary),
        1,
        "sensitive stdout duplicated or lost under fail-closed redaction"
    );
    // Fail-closed: the whole failure stderr (including nonsecret bytes) is
    // redacted, so the raw text must never appear and the marker must.
    assert!(
        !err.contains("fail-err-499"),
        "failure stderr must stay redacted under the sensitive fail-closed policy"
    );
    assert_eq!(
        count_occurrences(&err, "[REDACTED]"),
        1,
        "conservative redaction marker missing or duplicated"
    );
    let out_pos = err
        .find(canary)
        .expect("sensitive stdout must reach stderr");
    let redacted_pos = err
        .find("[REDACTED]")
        .expect("redaction marker must reach stderr");
    assert!(
        out_pos < redacted_pos,
        "program order lost: stdout must precede the redacted diagnostic"
    );
}
