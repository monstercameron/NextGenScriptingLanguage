#![cfg(all(unix, feature = "live"))]
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
        "accept token as text from secret prompt \"Token?\"\nuse value token\n",
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
        "ready",
        "{ready}"
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

#[test]
fn paused_live_bindings_show_an_explicitly_public_environment_value() {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::time::Duration;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("public.program");
    let socket = directory.path().join("control.sock");
    let public_home = "/LIVE-PUBLIC-HOME";
    std::fs::write(
        &program,
        "accept home_dir as text from public environment \"HOME\"\nuse value home_dir\n",
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
        .arg(json!({"home_dir": public_home}).to_string())
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
        "ready",
        "{ready}"
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
                snapshot.pointer("/result/runtime/debug/paused/bindings/home_dir"),
                Some(&json!(public_home))
            );
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
    panic!("live run did not pause at the public binding breakpoint");
}

#[test]
fn paused_live_bindings_withhold_secret_pipeline_derived_values() {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::time::Duration;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("secret.program");
    let socket = directory.path().join("control.sock");
    let canary = "CANARY-PIPELINE-LIVE-315";
    std::fs::write(
        &program,
        "accept token as text from secret prompt \"Token?\"\nuse value token\nthen \"cat\" as echoed\nrun printf with message \"continue\"\n",
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
        .arg("4")
        .arg("--bindings")
        .arg(json!({"token":canary}).to_string())
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
    let request = || -> Value {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        writeln!(stream, "{}", json!({"op":"read"})).unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    };
    let mut resume = UnixStream::connect(&socket).unwrap();
    writeln!(resume, "{}", json!({"op":"control","action":"resume"})).unwrap();
    let mut response = String::new();
    BufReader::new(resume).read_line(&mut response).unwrap();
    for _ in 0..100 {
        let snapshot = request();
        if snapshot
            .pointer("/result/runtime/status")
            .and_then(Value::as_str)
            == Some("breakpoint")
        {
            assert_eq!(
                snapshot.pointer("/result/runtime/debug/paused/bindings/echoed"),
                Some(&json!({"withheld":"secret"}))
            );
            assert!(!snapshot.to_string().contains(canary));
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
    panic!("live run did not pause at the derived secret binding breakpoint");
}

#[test]
fn paused_live_bindings_withhold_secret_reusable_program_results() {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::time::Duration;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("secret.program");
    let socket = directory.path().join("control.sock");
    let canary = "CANARY-NESTED-LIVE-315";
    std::fs::write(directory.path().join("credential.txt"), canary).unwrap();
    std::fs::write(
        &program,
        "define a program named reveal\n  run printf with message \"CANARY-NESTED-LIVE-SOURCE-315\"\n  run \"cat\" with arguments [\"credential.txt\"]\nrun program reveal as token and keep secret\nuse value token\n",
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
        .arg("5")
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
        "ready",
        "{ready}"
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
            fn secret_paths(value: &Value, path: &str, canary: &str, found: &mut Vec<String>) {
                match value {
                    Value::String(text) if text.contains(canary) => found.push(path.to_owned()),
                    Value::Array(items) => {
                        for (index, item) in items.iter().enumerate() {
                            secret_paths(item, &format!("{path}/{index}"), canary, found);
                        }
                    }
                    Value::Object(items) => {
                        for (key, item) in items {
                            secret_paths(item, &format!("{path}/{key}"), canary, found);
                        }
                    }
                    _ => {}
                }
            }
            let mut paths = Vec::new();
            secret_paths(&snapshot, "", canary, &mut paths);
            secret_paths(&snapshot, "", "CANARY-NESTED-LIVE-SOURCE-315", &mut paths);
            assert!(paths.is_empty(), "secret visible at {paths:?}");
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
    panic!("live run did not pause at the secret reusable-program result");
}

#[test]
fn paused_live_task_group_withholds_sensitive_aggregate_and_source() {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("secret.program");
    let socket = directory.path().join("control.sock");
    let canary = "CANARY-TASK-LIVE-315";
    std::fs::write(
        &program,
        format!("parallel at most 1 as results\n  run printf with message \"{canary}\" as token and keep secret\nrun printf with message \"done\"\n"),
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
        .arg("3")
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"],
        "ready"
    );
    let request = |op: &str| -> Value {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        writeln!(stream, "{}", json!({"op":op})).unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    };
    let mut resume = UnixStream::connect(&socket).unwrap();
    writeln!(resume, "{}", json!({"op":"control","action":"resume"})).unwrap();
    let mut response = String::new();
    BufReader::new(resume).read_line(&mut response).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = request("read");
        if snapshot
            .pointer("/result/runtime/status")
            .and_then(Value::as_str)
            == Some("breakpoint")
        {
            assert_eq!(
                snapshot.pointer("/result/state/bindings/results"),
                Some(&json!({"withheld":"secret"}))
            );
            assert!(!snapshot.to_string().contains(canary));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live run did not pause at task-group result"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stop = UnixStream::connect(&socket).unwrap();
    writeln!(stop, "{}", json!({"op":"control","action":"stop"})).unwrap();
    let mut response = String::new();
    BufReader::new(stop).read_line(&mut response).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("live task group failed to stop");
    }
}

#[test]
fn live_error_diagnostics_redact_secret_input_values() {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("secret.program");
    let socket = directory.path().join("control.sock");
    let canary = "CANARY-LIVE-ERROR-315";
    std::fs::write(
        &program,
        "accept token as text from secret prompt \"Token?\"\nuse value token\nrun \"sh\" with arguments [\"-c\", \"read value; printf '%s' \\\"$value\\\" >&2; exit 1\"]\n",
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
        .arg("3")
        .arg("--bindings")
        .arg(format!("{{\"token\":\"{canary}\"}}"))
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let ready_json = serde_json::from_str::<Value>(&ready).unwrap();
    assert_eq!(ready_json["event"], "ready", "{ready_json}");
    let request = |message: Value| -> Value {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        writeln!(stream, "{message}").unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    };
    request(json!({"op":"control","action":"resume"}));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut resumed_breakpoint = false;
    loop {
        let snapshot = request(json!({"op":"read"}));
        if !resumed_breakpoint
            && snapshot
                .pointer("/result/runtime/status")
                .and_then(Value::as_str)
                == Some("breakpoint")
        {
            request(json!({"op":"control","action":"resume"}));
            resumed_breakpoint = true;
            continue;
        }
        if snapshot
            .pointer("/result/runtime/status")
            .and_then(Value::as_str)
            == Some("failed")
        {
            assert!(!snapshot.to_string().contains(canary));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live run did not report failed secret input: {snapshot}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    request(json!({"op":"control","action":"stop"}));
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("live server failed to stop");
    }
}

#[test]
fn live_serve_refuses_a_socket_in_a_shared_parent() {
    use serde_json::Value;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let shared = directory.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    let program = directory.path().join("tool.program");
    let socket = shared.join("control.sock");
    std::fs::write(
        &program,
        "accept name as text with default \"x\"\nrun \"printf\" with arguments [\"finished\"]\n",
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
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success(), "serve must refuse a shared socket parent");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "serve did not refuse the shared socket parent"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let _ = child.wait();
    let reply: Value = serde_json::from_str(&stdout).expect("serve prints a JSON error");
    assert_eq!(reply["ok"], false);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("private"),
        "refusal names the private-directory requirement: {reply}"
    );
    assert!(
        !socket.exists(),
        "a refused endpoint must not be created in the shared directory"
    );
}

#[test]
fn live_stop_is_served_while_wait_slots_are_full() {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const WAITERS: usize = 16;
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("tool.program");
    let socket = directory.path().join("control.sock");
    std::fs::write(
        &program,
        "accept name as text with default \"x\"\nrun \"printf\" with arguments [\"finished\"]\n",
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
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"],
        "ready",
        "{ready}"
    );
    let request = |message: Value| -> Value {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(stream, "{message}").unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    };
    let revision = request(json!({"op":"read"}))["revision"]
        .as_u64()
        .expect("read reports a revision");
    // Hold every general client slot with a long poll.
    let sent = Arc::new(AtomicUsize::new(0));
    let waiters: Vec<_> = (0..WAITERS)
        .map(|_| {
            let socket = socket.clone();
            let sent = sent.clone();
            std::thread::spawn(move || -> Value {
                let mut stream = UnixStream::connect(&socket).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                writeln!(
                    stream,
                    "{}",
                    json!({"op":"wait","after":revision,"timeout_ms":10000})
                )
                .unwrap();
                sent.fetch_add(1, Ordering::SeqCst);
                let mut response = String::new();
                BufReader::new(stream).read_line(&mut response).unwrap();
                serde_json::from_str(&response).unwrap()
            })
        })
        .collect();
    while sent.load(Ordering::SeqCst) != WAITERS {
        std::thread::sleep(Duration::from_millis(20));
    }
    // The accept loop polls every 20 ms; let it admit every wait before probing
    // the reserved lane with stop.
    std::thread::sleep(Duration::from_secs(2));
    let stop = request(json!({"op":"control","action":"stop"}));
    assert_eq!(stop["ok"], true, "stop is served past full wait slots: {stop}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("live server did not stop while {WAITERS} waits were connected");
    }
    for waiter in waiters {
        let reply = waiter.join().expect("wait thread finishes after stop");
        assert_eq!(reply["ok"], true, "a woken wait answers: {reply}");
    }
}
