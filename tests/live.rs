#![cfg(unix)]
//! Real CLI/socket/native-process regressions for opt-in live execution.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn live_control_end_to_end() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live_control.py");
    let mut child = Command::new("python3")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_execute"))
        .stdin(Stdio::null())
        .spawn()
        .expect("Python 3 is required for live CLI integration tests");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "live CLI integration failed");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("live CLI tests exceeded their outer deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn paused_live_bindings_withhold_secret_prompt_values() {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::time::Duration;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("secret.program");
    let socket = directory.path().join("control.sock");
    std::fs::write(
        &program,
        "accept token as text from secret prompt \"Token?\"\nrun printf with message token\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("live")
        .arg("serve")
        .arg(&program)
        .arg("--socket")
        .arg(&socket)
        .arg("--paused")
        .arg("--headless")
        .arg("--breakpoints")
        .arg("2")
        .arg("--bindings")
        .arg("{\"token\":\"CANARY-LIVE-TOKEN\"}")
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut ready = String::new();
    reader.read_line(&mut ready).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"],
        "ready"
    );
    let request = |op: &str| -> Value {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        writeln!(stream, "{}", json!({"op": op})).unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    };
    let mut resume = UnixStream::connect(&socket).unwrap();
    writeln!(resume, "{}", json!({"op":"control","action":"resume"})).unwrap();
    let mut response = String::new();
    BufReader::new(resume).read_line(&mut response).unwrap();
    for _ in 0..100 {
        let snapshot = request("read");
        if snapshot
            .pointer("/result/runtime/status")
            .and_then(Value::as_str)
            == Some("breakpoint")
        {
            assert_eq!(
                snapshot.pointer("/result/runtime/debug/paused/bindings/token"),
                Some(&json!({"withheld":"secret"}))
            );
            assert!(!snapshot.to_string().contains("CANARY-LIVE-TOKEN"));
            let mut stop = UnixStream::connect(&socket).unwrap();
            writeln!(stop, "{}", json!({"op":"control","action":"stop"})).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if child.try_wait().unwrap().is_none() {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("live server did not stop after control request");
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("live run did not pause at the secret binding breakpoint");
}
