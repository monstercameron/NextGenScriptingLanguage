#![cfg(feature = "model")]
use std::process::{Command, Output};
use std::sync::Mutex;

static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

// Spawn this test executable as a lightweight child-environment probe. The
// production helper's identity check is covered by the model CLI tests; this
// probe only needs to report the variables that survive the host allowlist.
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

struct RestoreEnvironment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Drop for RestoreEnvironment {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[test]
fn model_helper_child_environment_allowlist_filters_variables() {
    let _lock = ENVIRONMENT_LOCK.lock().unwrap();
    let names = ["EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE", "LANE_PRIVATE"];
    let _restore = RestoreEnvironment(
        names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect(),
    );

    std::env::set_var("LANE_SAFE", "yes");
    std::env::set_var("LANE_PRIVATE", "private-helper-canary");

    let probe = |allow: Option<&str>| -> Output {
        match allow {
            Some(allow) => std::env::set_var("EXECUTE_CHILD_ENV_ALLOW", allow),
            None => std::env::remove_var("EXECUTE_CHILD_ENV_ALLOW"),
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "model_helper_probe", "--nocapture"]);
        flow::capabilities::apply_host_child_environment(&mut command).unwrap();
        command
            .env("EXECUTE_TEST_MODEL_HELPER", "1")
            .output()
            .unwrap()
    };

    let limited = probe(Some("LANE_SAFE"));
    assert!(
        limited.status.success(),
        "{}",
        String::from_utf8_lossy(&limited.stderr)
    );
    let text = String::from_utf8_lossy(&limited.stdout);
    assert!(text.contains("MODEL_ENV_SAFE=yes"), "{text}");
    assert!(text.contains("MODEL_ENV_PRIVATE=\n"), "{text}");
    assert!(!text.contains("private-helper-canary"));

    let full = probe(None);
    assert!(
        full.status.success(),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert!(
        String::from_utf8_lossy(&full.stdout).contains("MODEL_ENV_PRIVATE=private-helper-canary")
    );

    let mut invalid = Command::new(std::env::current_exe().unwrap());
    invalid.args(["--exact", "model_helper_probe", "--nocapture"]);
    std::env::set_var("EXECUTE_CHILD_ENV_ALLOW", "BAD=NAME");
    assert!(flow::capabilities::apply_host_child_environment(&mut invalid).is_err());
}
