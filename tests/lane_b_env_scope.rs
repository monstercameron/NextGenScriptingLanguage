//! Per-command environment: `and environment {...}` sets variables for one native
//! command, and `and without environment [...]` removes them for that command only.
//! Values are secrets: explain shows names only.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const LOADER_OVERRIDES: [&str; 8] = [
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_IMAGE_SUFFIX",
];

fn clear_loader_overrides(command: &mut Command) {
    for name in LOADER_OVERRIDES {
        command.env_remove(name);
    }
}

fn execute_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    clear_loader_overrides(&mut command);
    command
}

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, command: &str, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    let mut child = execute_command();
    child
        .current_dir(directory)
        .env("LANE_B_UNSET", "present")
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra);
    clear_loader_overrides(&mut child);
    child.output().unwrap()
}

fn run(script: &str, extra: &[&str]) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script, extra);
    (directory, output)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn environment_set_applies_to_that_command_only() {
    let (_directory, output) = run(
        "run \"env\" with arguments [] and environment {\"LANE_B_SCOPED\": \"yes\"}\nrun \"env\" with arguments []\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let text = stdout(&output);
    // Both commands print their environment. Only the first has the variable,
    // so the marker appears exactly once.
    assert_eq!(text.matches("LANE_B_SCOPED=yes").count(), 1, "{text}");
}

#[test]
fn environment_without_removes_for_that_command_only() {
    let (_directory, output) = run(
        "run \"env\" with arguments [] and without environment [\"LANE_B_UNSET\"]\nrun \"env\" with arguments []\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    // The second command still inherits LANE_B_UNSET from the Execute process.
    assert_eq!(stdout(&output).matches("LANE_B_UNSET=present").count(), 1);
}

#[test]
fn environment_value_can_come_from_a_binding() {
    let (_directory, output) = run(
        "accept mode as text with default \"zz\"\nrun \"env\" with arguments [] and environment {\"LANE_B_MODE\": mode}\n",
        &["--input", "mode=from-input"],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(
        stdout(&output).contains("LANE_B_MODE=from-input"),
        "{}",
        stdout(&output)
    );
}

#[cfg(unix)]
#[test]
fn path_overlay_is_used_for_lookup_launch_and_explain() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("fakebin");
    fs::create_dir(&bin).unwrap();
    let tool = bin.join("lane-b-path-tool");
    fs::write(&tool, "#!/bin/sh\nprintf 'overlay-path-ran\\n'\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let path = serde_json::to_string(bin.to_str().unwrap()).unwrap();
    let source = format!(
        "run \"lane-b-path-tool\" with arguments [] and environment {{\"PATH\": {path}}}\n"
    );

    let output = run_script(directory.path(), "run", &source, &[]);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(stdout(&output), "overlay-path-ran\n");

    let explain = run_script(directory.path(), "explain", &source, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["direct"] == true)
        .unwrap();
    assert_eq!(invocation["resolved"], true, "{plan}");
    assert_eq!(
        invocation["resolved_path"],
        fs::canonicalize(&tool).unwrap().to_string_lossy().as_ref()
    );
}

#[cfg(unix)]
#[test]
fn static_check_uses_the_command_path_overlay() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let inherited = directory.path().join("inherited");
    let overlay = directory.path().join("overlay");
    fs::create_dir_all(&inherited).unwrap();
    fs::create_dir_all(&overlay).unwrap();
    let tool = inherited.join("lane-b-only-in-inherited-path");
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let path = serde_json::to_string(overlay.to_str().unwrap()).unwrap();
    let source = format!(
        "run \"lane-b-only-in-inherited-path\" with arguments [] and environment {{\"PATH\": {path}}}\n"
    );
    let program = directory.path().join("workflow.program");
    fs::write(&program, source).unwrap();
    let output = execute_command()
        .current_dir(directory.path())
        .env("PATH", &inherited)
        .arg("check")
        .arg(&program)
        .arg("--resolve")
        .arg("--json")
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["diagnostics"][0]["code"], "exec.not_found");
    assert!(report["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains(overlay.to_str().unwrap()));
}

#[cfg(unix)]
#[test]
fn static_check_reports_a_missing_executable_from_workflow_path_overlay() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let inherited = directory.path().join("inherited");
    let overlay = directory.path().join("overlay");
    fs::create_dir_all(&inherited).unwrap();
    fs::create_dir_all(&overlay).unwrap();
    let tool = inherited.join("lane-b-only-in-inherited-path");
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let path = serde_json::to_string(overlay.to_str().unwrap()).unwrap();
    let source = format!(
        "use environment {{\"PATH\": {path}}}\nrun \"lane-b-only-in-inherited-path\" with arguments []\n"
    );
    let program = directory.path().join("workflow.program");
    fs::write(&program, source).unwrap();
    let output = execute_command()
        .current_dir(directory.path())
        .env("PATH", &inherited)
        .arg("check")
        .arg(&program)
        .arg("--resolve")
        .arg("--json")
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["diagnostics"][0]["code"], "exec.not_found");
    assert!(report["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains(overlay.to_str().unwrap()));
}

#[cfg(unix)]
#[test]
fn workflow_environment_persists_and_reusable_program_restores_caller_path() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let caller_bin = directory.path().join("caller-bin");
    let callee_bin = directory.path().join("callee-bin");
    fs::create_dir_all(&caller_bin).unwrap();
    fs::create_dir_all(&callee_bin).unwrap();
    let caller_tool = caller_bin.join("lane-b-scoped-tool");
    let callee_tool = callee_bin.join("lane-b-scoped-tool");
    fs::write(&caller_tool, "#!/bin/sh\nprintf 'caller-path\\n'\n").unwrap();
    fs::write(&callee_tool, "#!/bin/sh\nprintf 'callee-path\\n'\n").unwrap();
    fs::set_permissions(&caller_tool, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&callee_tool, fs::Permissions::from_mode(0o755)).unwrap();
    let caller_path = serde_json::to_string(caller_bin.to_str().unwrap()).unwrap();
    let callee_path = serde_json::to_string(callee_bin.to_str().unwrap()).unwrap();
    let source = format!(
        "define a program named inner\n  use environment {{\"PATH\": {callee_path}}}\n  run \"lane-b-scoped-tool\" with arguments []\nuse environment {{\"PATH\": {caller_path}}}\nrun program inner\nrun \"lane-b-scoped-tool\" with arguments []\n"
    );

    let output = run_script(directory.path(), "run", &source, &[]);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(stdout(&output), "callee-path\ncaller-path\n");

    let explain = run_script(directory.path(), "explain", &source, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let paths = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["direct"] == true)
        .map(|step| step["resolved_path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            fs::canonicalize(&callee_tool)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            fs::canonicalize(&caller_tool)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ]
    );
}

#[cfg(unix)]
#[test]
fn runtime_path_binding_is_used_after_it_is_computed() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("fakebin");
    fs::create_dir(&bin).unwrap();
    let tool = bin.join("lane-b-dynamic-path-tool");
    fs::write(&tool, "#!/bin/sh\nprintf 'dynamic-path-ran\\n'\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let source = "accept here as text from program directory\naccept oldpath as text from public environment \"PATH\"\nrun \"printf\" with arguments [\"%s/fakebin:%s\", here, oldpath] as newpath\nrun \"lane-b-dynamic-path-tool\" with arguments [] and environment {\"PATH\": newpath}\n";
    fs::write(directory.path().join("workflow.program"), source).unwrap();

    let output = execute_command()
        .arg("run")
        .arg(directory.path().join("workflow.program"))
        .arg("--manifest-dir")
        .arg(manifests())
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "dynamic-path-ran\n"
    );

    let explain = run_script(directory.path(), "explain", source, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "lane-b-dynamic-path-tool")
        .unwrap();
    assert_eq!(
        invocation["resolved"], true,
        "{plan}; stderr={:?}",
        explain.stderr
    );
    assert_eq!(
        invocation["resolved_path"],
        fs::canonicalize(&tool).unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(invocation["resolved_path_inferred"], true, "{plan}");
}

#[cfg(unix)]
#[test]
fn explain_does_not_publish_a_path_found_only_in_sensitive_path_tail() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let sensitive_bin = directory.path().join("sensitive-bin");
    fs::create_dir(&sensitive_bin).unwrap();
    let tool = sensitive_bin.join("lane-b-secret-path-tool");
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let inherited_path = format!("/usr/bin:/bin:{}", sensitive_bin.display());
    let source = format!(
        "accept oldpath as text from environment \"PATH\"\nuse environment {{\"PATH\": oldpath}}\nrun \"lane-b-secret-path-tool\" with arguments []\n"
    );
    fs::write(directory.path().join("workflow.program"), &source).unwrap();
    let explain = execute_command()
        .arg("explain")
        .arg(directory.path().join("workflow.program"))
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--json")
        .current_dir(directory.path())
        .env("PATH", inherited_path)
        .output()
        .unwrap();
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "lane-b-secret-path-tool")
        .unwrap();
    assert!(invocation["resolved_path"].is_null(), "{plan}");
    assert!(invocation["resolved"].is_null(), "{plan}");
    assert!(invocation["resolved_path_inferred"].is_null(), "{plan}");
    assert!(!plan
        .to_string()
        .contains(&sensitive_bin.to_string_lossy().to_string()));
}

#[cfg(unix)]
#[test]
fn explain_rejects_secret_environment_value_in_printf_arguments() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let sensitive_bin = directory.path().join("sensitive-format-bin");
    fs::create_dir(&sensitive_bin).unwrap();
    let tool = sensitive_bin.join("lane-b-secret-format-tool");
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let inherited_path = format!("/usr/bin:/bin:{}", sensitive_bin.display());
    let source = "accept oldpath as text from environment \"PATH\"\nrun \"printf\" with arguments [oldpath] as newpath\nuse environment {\"PATH\": newpath}\nrun \"lane-b-secret-format-tool\" with arguments []\n";
    fs::write(directory.path().join("workflow.program"), source).unwrap();
    let explain = execute_command()
        .arg("explain")
        .arg(directory.path().join("workflow.program"))
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--json")
        .current_dir(directory.path())
        .env("PATH", inherited_path)
        .output()
        .unwrap();
    assert!(!explain.status.success(), "{:?}", explain.stdout);
    let stderr = String::from_utf8_lossy(&explain.stderr);
    assert!(
        stderr.contains("cannot be passed as a command argument"),
        "{stderr}"
    );
    assert!(!stderr.contains(&sensitive_bin.to_string_lossy().to_string()));
    assert!(!String::from_utf8_lossy(&explain.stdout)
        .contains(&sensitive_bin.to_string_lossy().to_string()));
}

#[cfg(unix)]
#[test]
fn explain_does_not_choose_a_printf_path_from_one_conditional_arm() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let first_bin = directory.path().join("first-bin");
    let second_bin = directory.path().join("second-bin");
    fs::create_dir(&first_bin).unwrap();
    fs::create_dir(&second_bin).unwrap();
    for bin in [&first_bin, &second_bin] {
        let tool = bin.join("lane-b-branch-path-tool");
        fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let source = format!(
        "accept mode as text with default \"first\"\nif mode is \"first\"\n  run \"printf\" with arguments [\"%s\", \"{}\"] as newpath\notherwise\n  run \"printf\" with arguments [\"%s\", \"{}\"] as newpath\nuse environment {{\"PATH\": newpath}}\nrun \"lane-b-branch-path-tool\" with arguments []\n",
        first_bin.display(),
        second_bin.display(),
    );
    let explain = run_script(directory.path(), "explain", &source, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "lane-b-branch-path-tool")
        .unwrap();
    assert!(invocation["resolved_path"].is_null(), "{plan}");
    assert!(invocation["resolved_path_inferred"].is_null(), "{plan}");
}

#[cfg(unix)]
#[test]
fn explain_invalidates_workflow_path_changed_in_conditional_arms() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let custom_bin = directory.path().join("custom-bin");
    let target_bin = directory.path().join("target-bin");
    fs::create_dir(&custom_bin).unwrap();
    fs::create_dir(&target_bin).unwrap();
    let custom_printf = custom_bin.join("printf");
    fs::write(&custom_printf, "#!/bin/sh\nprintf custom\n").unwrap();
    fs::set_permissions(&custom_printf, fs::Permissions::from_mode(0o755)).unwrap();
    let tool = target_bin.join("lane-b-conditional-path-tool");
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let custom_path = serde_json::to_string(&custom_bin.to_string_lossy().to_string()).unwrap();
    let system_path = serde_json::to_string("/usr/bin:/bin").unwrap();
    let target_path = serde_json::to_string(&target_bin.to_string_lossy().to_string()).unwrap();
    let source = format!(
        "accept mode as text with default \"first\"\nif mode is \"first\"\n  use environment {{\"PATH\": {custom_path}}}\notherwise\n  use environment {{\"PATH\": {system_path}}}\nrun \"printf\" with arguments [\"%s\", {target_path}] as newpath\nuse environment {{\"PATH\": newpath}}\nrun \"lane-b-conditional-path-tool\" with arguments []\n"
    );
    let explain = run_script(directory.path(), "explain", &source, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "lane-b-conditional-path-tool")
        .unwrap();
    assert!(invocation["resolved_path"].is_null(), "{plan}");
    assert!(invocation["resolved_path_inferred"].is_null(), "{plan}");
}

#[cfg(unix)]
#[test]
fn explain_does_not_infer_printf_after_loader_environment_taint() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let target_bin = directory.path().join("loader-taint-bin");
    fs::create_dir(&target_bin).unwrap();
    let tool = target_bin.join("lane-b-loader-taint-tool");
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let target_path = serde_json::to_string(&target_bin.to_string_lossy().to_string()).unwrap();
    let source = format!(
        "use environment {{\"PATH\": \"/usr/bin:/bin\", \"LD_PRELOAD\": \"/tmp/interposed.so\"}}\nrun \"printf\" with arguments [\"%s\", {target_path}] as newpath\nuse environment {{\"PATH\": newpath}}\nrun \"lane-b-loader-taint-tool\" with arguments []\n"
    );
    let explain = run_script(directory.path(), "explain", &source, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "lane-b-loader-taint-tool")
        .unwrap();
    assert!(invocation["resolved_path"].is_null(), "{plan}");
    assert!(invocation["resolved_path_inferred"].is_null(), "{plan}");

    let per_command_source = format!(
        "use environment {{\"PATH\": \"/usr/bin:/bin\"}}\nrun \"printf\" with arguments [\"%s\", {target_path}] as newpath and environment {{\"LD_PRELOAD\": \"/tmp/interposed.so\"}}\nuse environment {{\"PATH\": newpath}}\nrun \"lane-b-loader-taint-tool\" with arguments []\n"
    );
    let explain = run_script(
        directory.path(),
        "explain",
        &per_command_source,
        &["--json"],
    );
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "lane-b-loader-taint-tool")
        .unwrap();
    assert!(invocation["resolved_path"].is_null(), "{plan}");
    assert!(invocation["resolved_path_inferred"].is_null(), "{plan}");

    let inherited_source = format!(
        "use environment {{\"PATH\": \"/usr/bin:/bin\"}}\nrun \"printf\" with arguments [\"%s\", {target_path}] as newpath\nuse environment {{\"PATH\": newpath}}\nrun \"lane-b-loader-taint-tool\" with arguments []\n"
    );
    let workflow = directory.path().join("workflow.program");
    fs::write(&workflow, inherited_source).unwrap();
    let explain = execute_command()
        .arg("explain")
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--json")
        .current_dir(directory.path())
        .env("DYLD_FALLBACK_LIBRARY_PATH", "/tmp/alternate-libraries")
        .output()
        .unwrap();
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let invocation = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "lane-b-loader-taint-tool")
        .unwrap();
    assert!(invocation["resolved_path"].is_null(), "{plan}");
    assert!(invocation["resolved_path_inferred"].is_null(), "{plan}");
}

#[cfg(unix)]
#[test]
fn explain_does_not_infer_a_printf_binding_with_stdout_redirected() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept here as text from program directory\naccept oldpath as text from public environment \"PATH\"\nrun \"printf\" with arguments [\"%s/fakebin:%s\", here, oldpath] as newpath and stdout to stderr\nrun \"lane-b-dynamic-path-tool\" with arguments [] and environment {\"PATH\": newpath}\n";
    let explain = run_script(directory.path(), "explain", source, &["--json"]);
    if explain.status.success() {
        let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
        let invocation = plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["binary"] == "lane-b-dynamic-path-tool")
            .unwrap();
        assert!(invocation["resolved_path"].is_null(), "{plan}");
        assert!(invocation["resolved_path_inferred"].is_null(), "{plan}");
    } else {
        assert!(
            String::from_utf8_lossy(&explain.stderr).contains("redirected stdout"),
            "{:?}",
            explain.stderr
        );
    }
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_workflow_path_changes_inside_conditionals_and_loops() {
    let cases = [
        "run \"mkdir\" with arguments [\"before-conditional\"]\nif true is true\n  use environment {\"PATH\": \"/first-path\"}\notherwise\n  use environment {\"PATH\": \"/second-path\"}\nrun \"mkdir\" with arguments [\"after-conditional\"]\n",
        "run \"mkdir\" with arguments [\"before-loop\"]\nrepeat 2 times\n  use environment {\"PATH\": \"/loop-path\"}\nrun \"mkdir\" with arguments [\"after-loop\"]\n",
    ];
    for (case_index, source) in cases.into_iter().enumerate() {
        let directory = tempfile::tempdir().unwrap();
        let checkpoint_path = directory.path().join("journal");
        let checkpoint = checkpoint_path.to_str().unwrap();
        let output = run_script(
            directory.path(),
            "run",
            source,
            &["--checkpoint", checkpoint],
        );
        assert_ne!(output.status.code(), Some(0));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("checkpointed workflow environment changes must be at top level"),
            "{:?}",
            output.stderr
        );
        let marker = if case_index == 0 {
            "before-conditional"
        } else {
            "before-loop"
        };
        assert!(!directory.path().join(marker).exists());
        assert!(!checkpoint_path.exists());
    }
}

#[cfg(unix)]
#[test]
fn checkpoint_resume_restores_runtime_computed_workflow_path_and_identity() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("checkpoint-dynamic-bin");
    fs::create_dir(&bin).unwrap();
    let marker = directory.path().join("dynamic-first-attempt");
    let identity_tool = bin.join("lane-b-checkpoint-identity");
    let identity_source = "#!/bin/sh\nprintf 'dynamic-identity-ok\\n'\n";
    fs::write(&identity_tool, identity_source).unwrap();
    fs::set_permissions(&identity_tool, fs::Permissions::from_mode(0o755)).unwrap();
    let tool = bin.join("lane-b-checkpoint-dynamic-path");
    fs::write(
        &tool,
        format!(
            "#!/bin/sh\nif [ ! -f '{}' ]; then\n  : > '{}'\n  /bin/sleep 30\nfi\nprintf 'dynamic-checkpoint-path-ok\\n'\n",
            marker.display(),
            marker.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let source = "accept here as text from program directory\nrun \"printf\" with arguments [\"%s/checkpoint-dynamic-bin\", here] as newpath\nuse environment {\"PATH\": newpath}\nrun \"lane-b-checkpoint-identity\" with arguments []\nrun \"lane-b-checkpoint-dynamic-path\" with arguments []\n";
    let workflow = directory.path().join("workflow.program");
    let path_workflow = source.to_owned();
    fs::write(&workflow, &path_workflow).unwrap();
    let checkpoint = directory.path().join("checkpoint");
    let mut child = execute_command()
        .current_dir(directory.path())
        .arg("run")
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !marker.exists() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("dynamic-PATH command never entered its interrupted attempt");
    }
    // SAFETY: the process id belongs to the still-running child started above.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let interrupted = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= wait_deadline {
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(!interrupted.success(), "the attempt should be interrupted");

    fs::write(
        &identity_tool,
        "#!/bin/sh\nprintf 'changed-binary-ran\\n'\n",
    )
    .unwrap();
    let changed = run_script(
        directory.path(),
        "resume",
        &path_workflow,
        &[
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--retry-uncertain",
        ],
    );
    assert_ne!(changed.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("completed checkpoint node changed"),
        "changed executable identity should be refused: {:?}",
        changed.stderr
    );

    fs::write(&identity_tool, identity_source).unwrap();

    let resumed = run_script(
        directory.path(),
        "resume",
        &path_workflow,
        &[
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--retry-uncertain",
        ],
    );
    assert_eq!(resumed.status.code(), Some(0), "{:?}", resumed.stderr);
    assert_eq!(stdout(&resumed), "dynamic-checkpoint-path-ok\n");
}

#[test]
fn workflow_environment_can_remove_inherited_values() {
    let (_directory, output) = run(
        "use without environment [\"LANE_B_UNSET\"]\nrun \"env\" with arguments []\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!stdout(&output).contains("LANE_B_UNSET="));
}

#[cfg(unix)]
#[test]
fn checkpoint_resume_reconstructs_static_workflow_environment_without_persisting_values() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    fn interrupt_at(child: &mut std::process::Child, marker: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !marker.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("workflow command never entered its interrupted attempt");
        }
        // SAFETY: the process id belongs to the still-running child started above.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let interrupted = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= wait_deadline {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(!interrupted.success(), "the attempt should be interrupted");
    }

    fn assert_secret_not_persisted(checkpoint: &Path) {
        for entry in fs::read_dir(checkpoint).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                assert_secret_not_persisted(&entry.path());
            } else if kind.is_file() {
                let bytes = fs::read(entry.path()).unwrap();
                assert!(!bytes
                    .windows(b"checkpoint-secret-value".len())
                    .any(|window| window == b"checkpoint-secret-value"));
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("checkpoint-bin");
    fs::create_dir(&bin).unwrap();
    let first_marker = directory.path().join("first-attempt");
    let second_marker = directory.path().join("second-attempt");
    let tool = bin.join("lane-b-checkpoint-env");
    fs::write(
        &tool,
        format!(
            "#!/bin/sh\nif [ ! -f '{}' ]; then\n  : > '{}'\n  /bin/sleep 30\nfi\n[ \"$LANE_B_RESUME_SECRET\" = \"checkpoint-secret-value\" ] || exit 41\n[ \"${{LANE_B_UNSET-unset}}\" = present ] || exit 42\nprintf 'set-state-ok\\n'\n",
            first_marker.display(),
            first_marker.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let verify_unset = directory.path().join("verify-unset.sh");
    fs::write(
        &verify_unset,
        format!(
            "#!/bin/sh\nif [ ! -f '{}' ]; then\n  : > '{}'\n  /bin/sleep 30\nfi\nif [ \"${{LANE_B_RESUME_SECRET+x}}\" = x ]; then secret_state=set; else secret_state=unset; fi\nif [ \"${{LANE_B_UNSET+x}}\" = x ]; then inherited_state=set; else inherited_state=unset; fi\nprintf '%s|%s|%s\\n' \"$PATH\" \"$secret_state\" \"$inherited_state\"\n",
            second_marker.display(),
            second_marker.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&verify_unset, fs::Permissions::from_mode(0o755)).unwrap();
    let path = serde_json::to_string(bin.to_str().unwrap()).unwrap();
    let verify_unset_literal = serde_json::to_string(verify_unset.to_str().unwrap()).unwrap();
    let source = format!(
        "use environment {{\"PATH\": {path}, \"LANE_B_RESUME_SECRET\": \"checkpoint-secret-value\"}}\nrun \"lane-b-checkpoint-env\" with arguments []\nuse without environment [\"PATH\", \"LANE_B_RESUME_SECRET\", \"LANE_B_UNSET\"]\nrun {} with arguments []\n",
        verify_unset_literal
    );
    let workflow = directory.path().join("workflow.program");
    fs::write(&workflow, &source).unwrap();
    let checkpoint = directory.path().join("checkpoint");
    let mut child = execute_command()
        .current_dir(directory.path())
        .arg("run")
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .env("LANE_B_UNSET", "present")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    interrupt_at(&mut child, &first_marker);
    assert_secret_not_persisted(&checkpoint);

    let mut resume = execute_command()
        .current_dir(directory.path())
        .arg("resume")
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .arg("--retry-uncertain")
        .env("LANE_B_UNSET", "present")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    interrupt_at(&mut resume, &second_marker);
    assert_secret_not_persisted(&checkpoint);

    let resumed = run_script(
        directory.path(),
        "resume",
        &source,
        &[
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--retry-uncertain",
        ],
    );
    assert_eq!(resumed.status.code(), Some(0), "{:?}", resumed.stderr);
    assert!(!stdout(&resumed).contains(bin.to_str().unwrap()));
    assert!(stdout(&resumed).contains("|unset|unset\n"));
}

#[cfg(windows)]
#[test]
fn case_variant_path_keys_resolve_and_reach_the_child_consistently() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first-bin");
    let last = directory.path().join("last-bin");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&last).unwrap();
    let command = std::env::var_os("SystemRoot")
        .expect("Windows sets SystemRoot")
        .into_string()
        .unwrap();
    let command = PathBuf::from(command).join("System32").join("cmd.exe");
    let first_command = first.join("cmd.exe");
    let last_command = last.join("cmd.exe");
    fs::copy(&command, &first_command).unwrap();
    fs::copy(&command, &last_command).unwrap();
    let first_path = serde_json::to_string(first.to_str().unwrap()).unwrap();
    let last_path = serde_json::to_string(last.to_str().unwrap()).unwrap();
    let source = format!(
        "use environment {{\"PATH\": {first_path}, \"Path\": {last_path}}}\nrun \"cmd.exe\" with arguments [\"/d\", \"/c\", \"echo\", \"%PATH%\"]\n"
    );

    let output = run_script(directory.path(), "run", &source, &[]);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let child_path = stdout(&output).trim().to_owned();

    let explain = run_script(directory.path(), "explain", &source, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    let resolved = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "cmd.exe")
        .unwrap()["resolved_path"]
        .as_str()
        .unwrap();
    assert_eq!(PathBuf::from(child_path), last);
    assert_eq!(
        PathBuf::from(resolved),
        fs::canonicalize(last_command).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn background_job_inherits_workflow_environment_set_and_unset_values() {
    let (_directory, set_output) = run(
        r#"use environment {"LANE_B_JOB_VALUE": "present"}
spawn as job
  run "sh" with arguments ["-c", "printf '%s' \"$LANE_B_JOB_VALUE\""]
await job
"#,
        &[],
    );
    assert_eq!(set_output.status.code(), Some(0), "{:?}", set_output.stderr);
    assert_eq!(stdout(&set_output), "present");

    let (_directory, unset_output) = run(
        r#"use without environment ["LANE_B_UNSET"]
spawn as job
  run "sh" with arguments ["-c", "if [ -z \"${LANE_B_UNSET+x}\" ]; then printf unset; else printf set; fi"]
await job
"#,
        &[],
    );
    assert_eq!(
        unset_output.status.code(),
        Some(0),
        "{:?}",
        unset_output.stderr
    );
    assert_eq!(stdout(&unset_output), "unset");
}

#[cfg(unix)]
#[test]
fn background_job_inherits_workflow_path_for_lookup_and_child_environment() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("job-bin");
    fs::create_dir(&bin).unwrap();
    let tool = bin.join("lane-b-job-path-tool");
    fs::write(&tool, "#!/bin/sh\nprintf '%s' \"$PATH\"\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let path = serde_json::to_string(bin.to_str().unwrap()).unwrap();
    let source = format!(
        "use environment {{\"PATH\": {path}}}\nspawn as job\n  run \"lane-b-job-path-tool\" with arguments []\nawait job\n"
    );
    let output = run_script(directory.path(), "run", &source, &[]);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(stdout(&output), bin.to_str().unwrap());
}

#[test]
fn literal_nul_environment_is_rejected_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"first\"\nthen save to \"marker.txt\"\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": \"x\\u0000y\"}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert_ne!(
        check.status.code(),
        Some(0),
        "check stdout: {:?}",
        check.stdout
    );
    let check_stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check_stderr.contains("environment value"), "{check_stderr}");

    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("environment value"),
        "{:?}",
        output.stderr
    );
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the earlier save ran before environment validation"
    );
}

#[test]
fn nul_in_a_text_binding_is_rejected_before_spawning_the_command() {
    let (_directory, output) = run(
        "run \"printf\" with arguments [\"\\\\000\"] as value\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": value}\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("environment value"), "{stderr}");
    assert!(stderr.contains("NUL byte"), "{stderr}");
}

#[test]
fn known_nul_text_binding_is_rejected_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"x\\u0000y\" as bad\nuse value bad as alias\nthen save to \"marker.txt\"\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": alias}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert_ne!(
        check.status.code(),
        Some(0),
        "check stdout: {:?}",
        check.stdout
    );
    let check_stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check_stderr.contains("environment value"), "{check_stderr}");
    assert!(check_stderr.contains("NUL byte"), "{check_stderr}");

    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("environment value"), "{stderr}");
    assert!(stderr.contains("NUL byte"), "{stderr}");
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the save ran before validating the known NUL binding"
    );
}

#[test]
fn known_nul_environment_binding_survives_branch_merge() {
    let directory = tempfile::tempdir().unwrap();
    let script = "accept enabled as boolean with default false\nif enabled is true\n  use value \"x\\u0000y\" as bad\notherwise\n  use value \"a\\u0000b\" as bad\nthen save to \"marker.txt\"\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": bad}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("environment value"), "{stderr}");
    assert!(stderr.contains("NUL byte"), "{stderr}");
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the save ran before validating the merged NUL binding"
    );
}

#[test]
fn known_nul_taken_through_a_static_pointer_is_rejected_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/x\" as pointer\nuse value {\"x\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\ntake text at pointer from document as bad\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": bad}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert_ne!(
        check.status.code(),
        Some(0),
        "check stdout: {:?}",
        check.stdout
    );
    let check_stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check_stderr.contains("NUL byte"), "{check_stderr}");

    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NUL byte"), "{stderr}");
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the save ran before validating the statically known take result"
    );
}

#[test]
fn unknown_pointer_does_not_treat_an_unselected_nul_as_the_take_result() {
    let directory = tempfile::tempdir().unwrap();
    let script = "accept pointer as text with default \"/safe\"\nuse value {\"safe\": \"ok\", \"other\": \"x\\u0000y\"} as document\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert!(check.status.success(), "check stderr: {:?}", check.stderr);
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "run stderr: {:?}", output.stderr);
}

#[test]
fn exact_loop_rebinding_updates_known_pointer_values() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value \"/nul\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn finally_rebinding_updates_known_pointer_values() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  try\n    use value \"/safe\" as pointer\n  finally\n    use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value \"/nul\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  try\n    use value \"/nul\" as pointer\n  finally\n    use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn unreachable_loop_assignments_do_not_replace_known_pointer_values() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  break\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(directory.path().join("marker.txt").exists());
}

#[test]
fn finally_created_pointer_facts_are_merged_for_preflight() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\ntry\n  use value \"body\"\nfinally\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\ntry\n  use value \"body\"\nfinally\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn nested_loop_break_does_not_hide_outer_loop_rebinding() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  repeat 1 times\n    break\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("NUL byte"),
        "{:?}",
        output.stderr
    );
    assert!(!directory.path().join("marker.txt").exists());
}

#[test]
fn repeated_constant_loop_assignments_keep_the_final_known_value() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 2 times\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value \"/nul\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 2 times\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn two_item_for_each_does_not_require_a_repeat_summary() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nuse value [\"one\", \"two\"] as items\nfor each item in items\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(directory.path().join("marker.txt").exists());
}

#[test]
fn repeat_with_early_transfer_does_not_require_a_repeat_summary() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 2 times\n  break\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(directory.path().join("marker.txt").exists());
}

#[test]
fn nested_loop_exit_still_blocks_outer_loop_fact_propagation() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nrepeat 1 times\n  repeat 1 times\n    exit with status 0\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert!(check.status.success(), "{:?}", check.stderr);
}

#[test]
fn explain_shows_names_but_not_values() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "explain",
        "run \"env\" with arguments [] and environment {\"LANE_B_SECRET\": \"hunter2-value\"} and without environment [\"LANE_B_OLD\"]\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let text = stdout(&output);
    assert!(text.contains("LANE_B_SECRET"), "{text}");
    assert!(text.contains("LANE_B_OLD"), "{text}");
    assert!(!text.contains("hunter2-value"), "{text}");
}

#[test]
fn invalid_environment_forms_are_rejected_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            "run \"env\" with arguments [] and environment {\"BAD=NAME\": \"x\"}\n",
            "invalid environment name",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_NUM\": 1}\n",
            "must be a text literal",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_BOTH\": \"x\"} and without environment [\"LANE_B_BOTH\"]\n",
            "both set and removed",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_X\": \"x\"}\npipe to \"tr\" with arguments [\"a\", \"b\"]\n",
            "single command",
        ),
        (
            "run cd with path \".\" and environment {\"LANE_B_X\": \"x\"}\n",
            "builtin",
        ),
    ];
    for (script, expected) in cases {
        let output = run_script(directory.path(), "check", script, &[]);
        assert_ne!(output.status.code(), Some(0), "{script}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{script}: {stderr}");
    }
}
