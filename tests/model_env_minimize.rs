#![cfg(feature = "model")]
use std::fs;
use std::process::Command;

// This test executable is also copied next to Execute as its model helper. The
// helper invocation selects only this test so it can report the actual child env.
#[test]
fn model_helper_probe() {
    if std::env::var_os("EXECUTE_TEST_MODEL_HELPER").is_some() {
        println!(
            "MODEL_ENV_SAFE={}",
            std::env::var("LANE_SAFE").unwrap_or_default()
        );
        println!(
            "MODEL_ENV_PRIVATE={}",
            std::env::var("LANE_PRIVATE").unwrap_or_default()
        );
    }
}

#[test]
fn model_helper_obeys_child_environment_allowlist() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join(if cfg!(windows) {
        "execute.exe"
    } else {
        "execute"
    });
    let helper = directory.path().join(if cfg!(windows) {
        "execute-model.exe"
    } else {
        "execute-model"
    });
    fs::copy(env!("CARGO_BIN_EXE_execute"), &executable).unwrap();
    fs::copy(std::env::current_exe().unwrap(), &helper).unwrap();
    let run = |allow: Option<&str>| {
        let mut command = Command::new(&executable);
        if let Some(allow) = allow {
            command.args(["--env-allow", allow]);
        }
        command
            .args(["model", "--exact", "model_helper_probe", "--nocapture"])
            .env("EXECUTE_TEST_MODEL_HELPER", "1")
            .env("LANE_SAFE", "yes")
            .env("LANE_PRIVATE", "private-helper-canary");
        command.env_remove("EXECUTE_CHILD_ENV_ALLOW");
        command.output().unwrap()
    };
    let limited = run(Some("EXECUTE_TEST_MODEL_HELPER,LANE_SAFE"));
    assert!(
        limited.status.success(),
        "{}",
        String::from_utf8_lossy(&limited.stderr)
    );
    let text = String::from_utf8_lossy(&limited.stdout);
    assert!(text.contains("MODEL_ENV_SAFE=yes"), "{text}");
    assert!(text.contains("MODEL_ENV_PRIVATE=\n"), "{text}");
    assert!(!text.contains("private-helper-canary"));
    let full = run(None);
    assert!(
        full.status.success(),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert!(
        String::from_utf8_lossy(&full.stdout).contains("MODEL_ENV_PRIVATE=private-helper-canary")
    );
    let invalid = run(Some("BAD=NAME"));
    assert!(!invalid.status.success());
    assert!(!String::from_utf8_lossy(&invalid.stdout).contains("private-helper-canary"));
}
