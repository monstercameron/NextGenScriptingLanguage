#![cfg(unix)]
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SCRIPT: &str = "run \"/usr/bin/env\" with arguments []\n";
const SECRET: &str = "not-for-child-6bfea8";

fn base(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(dir)
        .env("LANE_SAFE", "yes")
        .env("LANE_PRIVATE", SECRET)
        .env_remove("EXECUTE_DAEMON")
        .env_remove("EXECUTE_DAEMON_ACCEPT_CONTEXT")
        .env_remove("EXECUTE_CHILD_ENV_ALLOW");
    command
}
fn assert_minimized(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"LANE_SAFE=yes\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}
#[test]
fn finite_runs_filter_ambient_environment_and_records_but_default_inherits() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.program");
    let record = dir.path().join("run.record");
    fs::write(&file, SCRIPT).unwrap();
    let output = base(dir.path())
        .arg("run")
        .arg(&file)
        .arg("--record")
        .arg(&record)
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE")
        .output()
        .unwrap();
    assert_minimized(&output);
    fn check_record(path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                check_record(&path);
            } else {
                assert!(
                    !String::from_utf8_lossy(&fs::read(&path).unwrap()).contains(SECRET),
                    "{}",
                    path.display()
                );
            }
        }
    }
    check_record(&record);
    let full = base(dir.path()).arg("run").arg(&file).output().unwrap();
    assert!(full.status.success());
    assert!(String::from_utf8_lossy(&full.stdout).contains(SECRET));
    let empty = base(dir.path())
        .arg("run")
        .arg(&file)
        .env("EXECUTE_CHILD_ENV_ALLOW", "")
        .output()
        .unwrap();
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
}

#[test]
fn command_line_environment_flags_work_without_a_capability_policy() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.program");
    fs::write(&file, SCRIPT).unwrap();
    let allowed = base(dir.path())
        .arg("run")
        .arg(&file)
        .args(["--env-allow", "LANE_SAFE"])
        .output()
        .unwrap();
    assert_minimized(&allowed);

    let cleared = base(dir.path())
        .args(["--clear-env", "run"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(cleared.status.success());
    assert!(cleared.stdout.is_empty());
}
#[test]
fn malformed_allowlists_and_unlisted_command_overrides_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.program");
    fs::write(&file, SCRIPT).unwrap();
    for allow in ["LANE_SAFE,,PATH", "LD_PRELOAD", "BAD=NAME"] {
        let output = base(dir.path())
            .arg("run")
            .arg(&file)
            .env("EXECUTE_CHILD_ENV_ALLOW", allow)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    }
    fs::write(
        &file,
        "run \"/usr/bin/env\" with arguments [] and environment {\"LANE_PRIVATE\": \"explicit\"}\n",
    )
    .unwrap();
    let output = base(dir.path())
        .arg("run")
        .arg(&file)
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not in EXECUTE_CHILD_ENV_ALLOW"));
    let allowed = base(dir.path())
        .arg("run")
        .arg(&file)
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_PRIVATE")
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(allowed.stdout, b"LANE_PRIVATE=explicit\n");
}
#[test]
fn run_parts_and_listener_handlers_apply_the_same_environment_limit() {
    let dir = tempfile::tempdir().unwrap();
    let parts = dir.path().join("parts");
    fs::create_dir(&parts).unwrap();
    fs::write(parts.join("run.program"), SCRIPT).unwrap();
    let output = base(dir.path())
        .arg("run-parts")
        .arg(&parts)
        .args(["--env-allow", "LANE_SAFE"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("LANE_SAFE=yes"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    let file = dir.path().join("listen.program");
    fs::write(&file, "listen to stdin\nwhenever a line arrives as record\n  run \"/usr/bin/env\" with arguments []\n").unwrap();
    let mut child = base(dir.path())
        .arg("run")
        .arg(&file)
        .arg("--max-runs")
        .arg("1")
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"event\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("LANE_SAFE=yes"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
}
#[test]
fn opted_in_daemon_uses_the_callers_environment_limit() {
    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let mut daemon = Daemon(
        base(work.path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_PRIVATE")
            .args(["daemon", "serve"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = base(work.path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .args(["daemon", "status"])
            .output()
            .unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        if reply["running"] == true {
            break;
        }
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    let file = work.path().join("main.program");
    fs::write(&file, SCRIPT).unwrap();
    let output = base(work.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("EXECUTE_DAEMON_ACCEPT_CONTEXT", "1")
        .env("EXECUTE_DAEMON_TRACE", "1")
        .arg("run")
        .arg(&file)
        .arg("--daemon")
        .args(["--env-allow", "LANE_SAFE"])
        .output()
        .unwrap();
    assert_minimized(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("client-started"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn named_applets_intersect_the_host_allowlist_with_policy() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_dir = dir.path().join("manifests");
    fs::create_dir(&manifest_dir).unwrap();
    fs::write(manifest_dir.join("env.json"), r#"{"name":"fixture.env","binary":"/usr/bin/env","arguments":[],"contract":{"version":1,"capabilities":["process.exec"],"effects":[]}}"#).unwrap();
    let file = dir.path().join("main.program");
    fs::write(&file, "run fixture.env\n").unwrap();
    let policy = dir.path().join("policy.json");
    fs::write(&policy, r#"{"version":1,"applets":{"fixture.env":{"capabilities":["process.exec"],"environment":["LANE_SAFE","LANE_PRIVATE"]}}}"#).unwrap();
    let output = base(dir.path())
        .arg("run")
        .arg(&file)
        .arg("--manifest-dir")
        .arg(&manifest_dir)
        .arg("--capability-policy")
        .arg(&policy)
        .args(["--execution-mode", "trusted"])
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE")
        .output()
        .unwrap();
    assert_minimized(&output);
    fs::write(&policy, r#"{"version":1,"applets":{"fixture.env":{"capabilities":["process.exec"],"environment":["LANE_PRIVATE"]}}}"#).unwrap();
    let output = base(dir.path())
        .arg("run")
        .arg(&file)
        .arg("--manifest-dir")
        .arg(&manifest_dir)
        .arg("--capability-policy")
        .arg(&policy)
        .args(["--execution-mode", "trusted"])
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn manifest_environment_overrides_must_be_allowlisted() {
    let dir = tempfile::tempdir().unwrap();
    let manifests = dir.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    fs::write(manifests.join("env.json"), r#"{"name":"fixture.env","binary":"/usr/bin/env","arguments":[],"contract":{"version":1,"environment":{"set":{"LANE_PRIVATE":"manifest-value"}}}}"#).unwrap();
    let file = dir.path().join("main.program");
    fs::write(&file, "run fixture.env\n").unwrap();
    let output = base(dir.path())
        .arg("run")
        .arg(&file)
        .arg("--manifest-dir")
        .arg(&manifests)
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not in EXECUTE_CHILD_ENV_ALLOW"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("manifest-value"));
}

#[test]
fn persistent_worker_receives_only_allowed_ambient_variables() {
    let Some(python) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("python3"))
        .find(|path| path.is_file())
    else {
        panic!("python3 required by worker fixture")
    };
    let dir = tempfile::tempdir().unwrap();
    let manifests = dir.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    let code = "import json,os,sys\nfor line in sys.stdin:\n r=json.loads(line); out=os.getenv('LANE_SAFE','missing')+'|'+os.getenv('LANE_PRIVATE','absent'); print(json.dumps({'id':r['id'],'status':0,'output':list(out.encode())}),flush=True)\n";
    let manifest = serde_json::json!({"name":"fixture.worker","binary":python,"arguments":[],"contract":{"version":1,"backend":"json_lines_worker","lifecycle":{"kind":"json_lines","startup_arguments":["-c",code]}}});
    fs::write(manifests.join("worker.json"), manifest.to_string()).unwrap();
    let file = dir.path().join("main.program");
    fs::write(&file, "run fixture.worker\n").unwrap();
    let output = base(dir.path())
        .arg("run")
        .arg(&file)
        .arg("--manifest-dir")
        .arg(&manifests)
        .env("EXECUTE_CHILD_ENV_ALLOW", "LANE_SAFE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("yes|absent"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
}
