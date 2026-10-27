//! Privileged steps: `explain` marks elevators and wrapper launchers, and a capability
//! policy with `deny_privileged: true` refuses them before any process starts. The check
//! also runs at every command launch, so it holds for resumed runs and for programs that
//! the static preflight does not walk.

use flow::capabilities::CapabilityPolicy;
use flow::manifest::Registry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn manifest(name: &str, binary: &str, literals: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "binary": binary,
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {},
        "arguments": literals.iter().map(|value| serde_json::json!({ "literal": value })).collect::<Vec<_>>()
    })
}

/// Three applets: `elevated.run` runs sudo, `wrapped.run` runs env (which can name
/// another program), and `plain.run` runs printf. The program has a plain step, then
/// an elevated one.
fn fixture(work: &Path) -> (PathBuf, PathBuf) {
    let manifests = work.join("manifests");
    fs::create_dir(&manifests).unwrap();
    let definitions = [
        ("elevated.run", "sudo", vec!["true"]),
        ("wrapped.run", "env", vec!["sudo", "true"]),
        ("plain.run", "printf", vec!["plain"]),
    ];
    for (name, binary, literals) in definitions {
        fs::write(
            manifests.join(format!("{name}.json")),
            manifest(name, binary, &literals).to_string(),
        )
        .unwrap();
    }
    let program = work.join("elevated.program");
    fs::write(&program, "run plain.run\nrun elevated.run\n").unwrap();
    (manifests, program)
}

fn policy_file(work: &Path, body: &str) -> PathBuf {
    let policy = work.join("policy.json");
    fs::write(&policy, body).unwrap();
    policy
}

fn line_of(plan: &str, number: usize) -> &str {
    let prefix = format!("line {number}:");
    plan.lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no line {number} in {plan}"))
}

#[test]
fn explain_marks_an_elevator_and_leaves_a_plain_step_alone() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, program) = fixture(work.path());
    let output = execute(
        work.path(),
        &[
            "explain",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let plan = text(&output.stdout);
    assert!(
        line_of(&plan, 2).contains("[privileged: elevator]"),
        "{plan}"
    );
    assert!(!line_of(&plan, 1).contains("privileged"), "{plan}");
    assert!(!line_of(&plan, 1).contains("wrapper"), "{plan}");
}

#[test]
fn explain_marks_a_wrapper_launcher_and_a_direct_uppercase_exe() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, _) = fixture(work.path());
    let program = work.path().join("wrapper.program");
    fs::write(&program, "run wrapped.run\nrun \"SUDO.EXE\"\n").unwrap();
    let output = execute(
        work.path(),
        &[
            "explain",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let plan = text(&output.stdout);
    assert!(
        line_of(&plan, 1).contains("[wrapper: may run another program]"),
        "{plan}"
    );
    assert!(
        line_of(&plan, 2).contains("[privileged: elevator]"),
        "{plan}"
    );
}

#[test]
fn explain_recognises_the_extended_lists() {
    let work = tempfile::tempdir().unwrap();
    let names = [
        "runas",
        "sg",
        "newgrp",
        "sudo-rs",
        "su",
        "runuser",
        "systemd-run",
        "machinectl",
        "gsudo",
        "please",
        "pkexec",
        "doas",
        "run0",
        "env",
        "nice",
        "ionice",
        "chrt",
        "taskset",
        "time",
        "watch",
        "script",
        "start-stop-daemon",
        "setpriv",
        "xargs",
        "busybox",
        "cmd",
        "powershell",
        "pwsh",
        "ksh",
        "mksh",
        "ash",
        "csh",
        "tcsh",
        "yash",
        "posh",
        "sh",
        "bash",
        "dash",
        "zsh",
        "fish",
    ];
    let source: String = names
        .iter()
        .map(|name| format!("run \"{name}\"\n"))
        .collect();
    let program = work.path().join("names.program");
    fs::write(&program, source).unwrap();
    let output = execute(work.path(), &["explain", program.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let plan = text(&output.stdout);
    for (index, name) in names.iter().enumerate() {
        let line = line_of(&plan, index + 1);
        assert!(
            line.contains("[privileged") || line.contains("[wrapper"),
            "{name} is not marked: {line}"
        );
    }
}

#[test]
fn a_policy_that_denies_privileged_steps_refuses_them_before_any_effect() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, program) = fixture(work.path());
    let policy = policy_file(
        work.path(),
        r#"{"version":1,"applets":{},"deny_privileged":true}"#,
    );
    let output = execute(
        work.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("denies privileged steps"), "{stderr}");
    // The plain step comes first, but preflight refuses the program before any step runs.
    assert!(text(&output.stdout).is_empty(), "{}", text(&output.stdout));
}

#[test]
fn an_env_wrapper_is_denied_whatever_its_arguments() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, _) = fixture(work.path());
    let program = work.path().join("wrapped.program");
    fs::write(&program, "run wrapped.run\n").unwrap();
    let policy = policy_file(
        work.path(),
        r#"{"version":1,"applets":{},"deny_privileged":true}"#,
    );
    let output = execute(
        work.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("command launcher"), "{stderr}");
    assert!(stderr.contains("allow_wrappers"), "{stderr}");
}

#[test]
fn an_allowed_wrapper_passes_the_privilege_check() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, _) = fixture(work.path());
    let program = work.path().join("wrapped.program");
    fs::write(&program, "run wrapped.run\n").unwrap();
    let policy = policy_file(
        work.path(),
        r#"{"version":1,"applets":{},"deny_privileged":true,"allow_wrappers":["env"]}"#,
    );
    let output = execute(
        work.path(),
        &[
            "check",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    // The applet has no grant, so the strict policy still refuses it, but not as a
    // privileged step.
    let stderr = text(&output.stderr);
    assert!(!stderr.contains("denies privileged steps"), "{stderr}");
}

#[test]
fn an_elevator_cannot_be_allowed() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, program) = fixture(work.path());
    let policy = policy_file(
        work.path(),
        r#"{"version":1,"applets":{},"deny_privileged":true,"allow_wrappers":["sudo"]}"#,
    );
    let output = execute(
        work.path(),
        &[
            "check",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains("elevators cannot be allowed"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn the_policy_flag_must_be_a_boolean() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, program) = fixture(work.path());
    let policy = policy_file(
        work.path(),
        r#"{"version":1,"applets":{},"deny_privileged":"yes"}"#,
    );
    let output = execute(
        work.path(),
        &[
            "check",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn a_resumed_run_is_refused_when_its_suffix_is_privileged() {
    let work = tempfile::tempdir().unwrap();
    let (manifests, _) = fixture(work.path());
    let executable = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    let program = work.path().join("resume.program");
    // Line 1 completes, line 2 fails, line 3 is privileged and never reached.
    fs::write(
        &program,
        format!(
            "run plain.run\nrun \"{executable}\" with arguments [\"--no-such-option\"]\nrun elevated.run\n"
        ),
    )
    .unwrap();
    let checkpoint = work.path().join("ckpt");
    let failed = execute(
        work.path(),
        &[
            "run",
            "resume.program",
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert_eq!(failed.status.code(), Some(1), "{}", text(&failed.stderr));

    let policy = policy_file(
        work.path(),
        r#"{"version":1,"applets":{},"deny_privileged":true}"#,
    );
    let resumed = execute(
        work.path(),
        &[
            "resume",
            "resume.program",
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("denies privileged steps"),
        "{}",
        text(&resumed.stderr)
    );
}

fn deny_all_privileged_registry(manifests: &[serde_json::Value]) -> Registry {
    let sources: Vec<String> = manifests.iter().map(|value| value.to_string()).collect();
    let registry = Registry::from_sources(&sources).unwrap();
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        applets: BTreeMap::new(),
        deny_privileged: true,
        allow_wrappers: BTreeSet::new(),
        host_environment: BTreeSet::new(),
    }));
    registry
}

#[test]
fn the_launch_check_classifies_the_manifest_name_and_the_resolved_path() {
    let registry = deny_all_privileged_registry(&[manifest("elevated.run", "sudo", &["true"])]);
    let directory = tempfile::tempdir().unwrap();
    for (names, resolved) in [
        (vec!["sudo"], PathBuf::from("sudo")),
        (vec!["SUDO.EXE"], PathBuf::from("SUDO.EXE")),
        (vec!["tool"], PathBuf::from("/usr/bin/sudo")),
        (vec!["env"], PathBuf::from("env")),
        (vec!["sh"], PathBuf::from("sh")),
    ] {
        let error =
            flow::capabilities::check_privileged_launch(&registry, &names, &resolved).unwrap_err();
        assert!(
            format!("{error:#}").contains("denies privileged steps"),
            "{names:?} {resolved:?}: {error:#}"
        );
    }
    // A file whose own name is not listed is still refused when its resolved path is an
    // elevator: a renamed copy of an elevator is an elevator.
    let renamed = directory.path().join("pkexec");
    fs::write(&renamed, b"").unwrap();
    let error =
        flow::capabilities::check_privileged_launch(&registry, &["tool"], &renamed).unwrap_err();
    assert!(
        format!("{error:#}").contains("denies privileged steps"),
        "{error:#}"
    );
    flow::capabilities::check_privileged_launch(&registry, &["printf"], Path::new("printf"))
        .expect("a standard executable is not refused");
}

#[test]
fn command_for_refuses_a_privileged_launch_before_any_command_is_built() {
    let registry = deny_all_privileged_registry(&[manifest("elevated.run", "sudo", &["true"])]);
    let directory = tempfile::tempdir().unwrap();
    // A registered applet whose manifest binary is sudo.
    let registered = flow::capabilities::command_for(
        Path::new("sudo"),
        ["true"],
        &registry,
        "elevated.run",
        &flow::working_dir::WorkingDirectory::plain(directory.path()),
    );
    let error = format!("{:#}", registered.unwrap_err());
    assert!(error.contains("denies privileged steps"), "{error}");
    // A direct executable that is not registered: refused by its resolved name.
    let direct = flow::capabilities::command_for(
        Path::new("/usr/bin/sudo"),
        ["true"],
        &registry,
        "sudo",
        &flow::working_dir::WorkingDirectory::plain(directory.path()),
    );
    let error = format!("{:#}", direct.unwrap_err());
    assert!(error.contains("denies privileged steps"), "{error}");
    // An unregistered applet name over a renamed elevator: refused by the resolved path.
    let renamed = directory.path().join("pkexec");
    fs::write(&renamed, b"").unwrap();
    let renamed_refused = flow::capabilities::command_for(
        &renamed,
        std::iter::empty::<&str>(),
        &registry,
        "tool",
        &flow::working_dir::WorkingDirectory::plain(directory.path()),
    );
    let error = format!("{:#}", renamed_refused.unwrap_err());
    assert!(error.contains("denies privileged steps"), "{error}");
}
