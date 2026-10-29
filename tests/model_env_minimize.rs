#![cfg(feature = "model")]
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

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

fn read_request_headers(stream: &mut TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&bytes[..end + 4]).into_owned());
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 || bytes.len() + count > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incomplete or oversized HTTP request headers",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn serve_chat_completion() -> (String, thread::JoinHandle<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind owned loopback listener");
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback listener failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        let headers = read_request_headers(&mut stream)
            .unwrap_or_else(|error| panic!("reading loopback request: {error}"));
        let body = r#"{"model":"local-fixture","choices":[{"index":0,"message":{"role":"assistant","content":"accepted"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        Some(headers.to_ascii_lowercase())
    });
    (endpoint, server)
}

fn run_execute_model(executable: &Path, allow: &str, endpoint: &str) -> Output {
    let mut command = Command::new(executable);
    command
        .args([
            "--env-allow",
            allow,
            "model",
            "generate",
            "--provider",
            "openai",
            "--api",
            "chat_completions",
            "--model",
            "gpt-6-luna",
            "--instruction",
            "Draft one line",
            "--endpoint",
            endpoint,
            "--api-key-env",
            "EXECUTE_MODEL_KEY_PROBE",
        ])
        .env("EXECUTE_MODELS_OFFLINE", "")
        .env("EXECUTE_MODEL_KEY_PROBE", "helper-env-canary")
        .env("EXECUTE_MODEL_ALLOW_LOOPBACK", "1")
        .env("OPENAI_API_KEY", "unselected-default-key")
        .env_remove("EXECUTE_CHILD_ENV_ALLOW")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap()
}

#[test]
fn model_cli_parses_env_allow_and_launches_the_trusted_sibling_helper() {
    let execute_source = PathBuf::from(env!("CARGO_BIN_EXE_execute"));
    let helper_source = PathBuf::from(env!("CARGO_BIN_EXE_execute-model"));
    let source_directory = execute_source
        .parent()
        .expect("Cargo execute binary has a parent directory");
    assert_eq!(
        helper_source.parent(),
        Some(source_directory),
        "Cargo binaries must be siblings before staging"
    );

    // Stage byte-identical Cargo-built production binaries beside one another.
    // The copy gives each helper a single link while preserving its identity;
    // placing the private directory under Cargo's trusted output directory
    // keeps all parent ACLs subject to the normal production trust gate.
    let directory = tempfile::Builder::new()
        .prefix("model-env-e2e-")
        .tempdir_in(source_directory)
        .expect("create staging directory beside Cargo binaries");
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
    std::fs::copy(&execute_source, &executable).unwrap();
    std::fs::copy(&helper_source, &helper).unwrap();

    let offline = run_execute_model(
        &executable,
        "EXECUTE_MODELS_OFFLINE",
        "http://127.0.0.1:1/v1",
    );
    assert!(!offline.status.success());
    assert!(offline.stdout.is_empty());
    assert_eq!(offline.stderr, b"execute-model: offline\n");

    let (endpoint, server) = serve_chat_completion();
    let allow = if cfg!(windows) {
        "PATH,SystemRoot,EXECUTE_MODEL_KEY_PROBE,EXECUTE_MODEL_ALLOW_LOOPBACK"
    } else {
        "PATH,EXECUTE_MODEL_KEY_PROBE,EXECUTE_MODEL_ALLOW_LOOPBACK"
    };
    let filtered = run_execute_model(&executable, allow, &endpoint);
    assert!(
        filtered.status.success(),
        "{}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    assert!(filtered.stderr.is_empty());
    let response: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(response["text"], "accepted");

    let headers = server
        .join()
        .unwrap()
        .expect("helper reached loopback server");
    assert!(
        headers.starts_with("post /v1/chat/completions "),
        "{headers}"
    );
    assert!(
        headers
            .lines()
            .any(|line| line == "authorization: bearer helper-env-canary"),
        "allowlisted helper credential missing from request: {headers}"
    );
    assert!(!headers.contains("unselected-default-key"));
}
