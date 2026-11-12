use flow::service_deployment::{render_for_workflow, ServiceFormat, WorkflowServiceRequest};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[path = "support/private_answers.rs"]
#[cfg(windows)]
mod private_answers;

const SECRET: &str = "CANARY-ISSUE-767-TOKEN";

fn write_private_answers(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(windows)]
    private_answers::make_private(path);
}

fn request<'a>(
    format: ServiceFormat,
    workflow: &'a Path,
    working_directory: &'a Path,
    executable: &'a Path,
    inputs: &'a BTreeMap<String, String>,
    answers_file: Option<&'a Path>,
    persistent: bool,
) -> WorkflowServiceRequest<'a> {
    WorkflowServiceRequest {
        format,
        workflow_path: workflow,
        executable,
        working_directory,
        manifest_directory: None,
        execution_mode: None,
        capability_policy: None,
        data_flow_policy: None,
        child_environment_allowlist: None,
        answers_file,
        label: Some("com.test.issue767"),
        inputs,
        program_arguments: &[],
        persistent,
    }
}

// Documented in docs/guide.md, "Generate a reviewable service definition":
// an environment-sourced input is supplied with --answers, never --input.
// The CLI itself cannot run this on the Windows machine because the repo's
// manifests folder is refused by the trust gate, so this drives the same
// renderer the CLI calls, with the answers map that `main.rs` loads from the file.
#[test]
fn environment_input_is_supplied_through_a_private_answers_file() {
    let work = tempfile::tempdir().unwrap();
    let workflow = work.path().join("nightly.program");
    fs::write(
        &workflow,
        "accept token as text from environment \"EXECUTE_ISSUE_767_TOKEN\"\nevery 5 minutes\n  then encode token as json\n",
    )
    .unwrap();
    let executable = std::env::current_exe().unwrap();
    let answers = work.path().join("answers.json");
    write_private_answers(&answers, &format!(r#"{{"token":"{SECRET}"}}"#));

    let supplied = BTreeMap::from([("token".to_owned(), SECRET.to_owned())]);
    let refused = render_for_workflow(&request(
        ServiceFormat::Systemd,
        &workflow,
        work.path(),
        &executable,
        &supplied,
        None,
        false,
    ))
    .unwrap_err();
    let refused = format!("{refused:#}");
    assert!(refused.contains("refuses to embed"), "{refused}");
    assert!(!refused.contains(SECRET), "{refused}");

    let from_answers = flow::prompt::answers_from_file(&answers).unwrap();
    let rendered = render_for_workflow(&request(
        ServiceFormat::Systemd,
        &workflow,
        work.path(),
        &executable,
        &from_answers,
        Some(&answers),
        false,
    ))
    .unwrap();
    assert!(rendered.contains("--answers"), "{rendered}");
    assert!(!rendered.contains(SECRET), "{rendered}");
}

// Documented in docs/guide.md, "Generate a systemd timer for a cron workflow".
#[test]
fn systemd_timer_renders_timer_and_oneshot_service_for_cron_workflows() {
    let work = tempfile::tempdir().unwrap();
    let workflow = work.path().join("nightly.program");
    fs::write(
        &workflow,
        "on schedule \"30 9 * * 1-5\"\n  then encode \"body\" as json\n",
    )
    .unwrap();
    let executable = std::env::current_exe().unwrap();
    let inputs = BTreeMap::new();

    let rendered = render_for_workflow(&request(
        ServiceFormat::SystemdTimer,
        &workflow,
        work.path(),
        &executable,
        &inputs,
        None,
        false,
    ))
    .unwrap();
    assert!(
        rendered.contains("OnCalendar=Mon,Tue,Wed,Thu,Fri *-*-* 09:30:00\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Persistent=false\nUnit=com.test.issue767.service\n"),
        "{rendered}"
    );
    assert!(rendered.contains("WantedBy=timers.target"), "{rendered}");
    assert!(rendered.contains("Type=oneshot"), "{rendered}");
    assert!(rendered.contains("\"run\" \"--once\""), "{rendered}");

    let persistent = render_for_workflow(&request(
        ServiceFormat::SystemdTimer,
        &workflow,
        work.path(),
        &executable,
        &inputs,
        None,
        true,
    ))
    .unwrap();
    assert!(persistent.contains("Persistent=true\n"), "{persistent}");

    let interval = work.path().join("interval.program");
    fs::write(&interval, "every 5 minutes\n  then encode \"body\" as json\n").unwrap();
    let refused = render_for_workflow(&request(
        ServiceFormat::SystemdTimer,
        &interval,
        work.path(),
        &executable,
        &inputs,
        None,
        false,
    ))
    .unwrap_err();
    assert!(
        format!("{refused:#}").contains("systemd-timer requires"),
        "{refused:#}"
    );
}
