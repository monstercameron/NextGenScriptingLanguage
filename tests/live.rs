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

    let directory = saturation_dir();
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

    let directory = saturation_dir();
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

    let directory = saturation_dir();
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

    let directory = saturation_dir();
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

    let directory = saturation_dir();
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

    let directory = saturation_dir();
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
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    let directory = saturation_dir();
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
            assert!(
                !status.success(),
                "serve must refuse a shared socket parent"
            );
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
    let directory = saturation_dir();
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
    assert_eq!(
        stop["ok"], true,
        "stop is served past full wait slots: {stop}"
    );
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

/// Start a paused, headless live server and wait for its ready line.
fn saturation_server(program: &std::path::Path, socket: &std::path::Path) -> std::process::Child {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("live")
        .arg("serve")
        .arg(program)
        .arg("--socket")
        .arg(socket)
        .arg("--paused")
        .arg("--headless")
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
        serde_json::from_str::<serde_json::Value>(&ready).unwrap()["event"],
        "ready",
        "{ready}"
    );
    child
}

/// One-shot request over a fresh connection; the server answers one JSON line.
fn saturation_request(
    socket: &std::path::Path,
    message: serde_json::Value,
    timeout: std::time::Duration,
) -> serde_json::Value {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    writeln!(stream, "{message}").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

/// The current revision, which every saturation test anchors its waits on.
fn saturation_revision(socket: &std::path::Path) -> u64 {
    saturation_request(
        socket,
        serde_json::json!({"op":"read"}),
        std::time::Duration::from_secs(10),
    )["revision"]
        .as_u64()
        .expect("read reports a revision")
}

/// Hold one general slot with a long poll; the thread exits when the server
/// stops. A seating waiter that loses the admission race to a gate probe is
/// shed with `busy` instead of holding a slot; it retries so the gate can only
/// observe genuine saturation, never a lost race. Probes are transient while
/// waiters persist, so seating converges instead of racing. Retries are bounded
/// so a failed test cannot leak a polling thread.
fn saturation_waiter(
    socket: std::path::PathBuf,
    revision: u64,
) -> std::thread::JoinHandle<serde_json::Value> {
    std::thread::spawn(move || {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let reply = saturation_request(
                &socket,
                serde_json::json!({"op":"wait","after":revision,"timeout_ms":10000}),
                Duration::from_secs(30),
            );
            if reply["error"]["code"] != "busy" || Instant::now() >= deadline {
                return reply;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}

/// Wait until the server sheds general-lane traffic: a zero-timeout wait is
/// answered `busy` exactly when every general slot is held by a long poll.
/// Bounded by a deadline instead of a fixed sleep, so the test neither races
/// admission nor idles past it. Probe traffic stays far below the flood rule.
fn saturation_gate(socket: &std::path::Path, label: &str) {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let revision = saturation_revision(socket);
        let probe = saturation_request(
            socket,
            serde_json::json!({"op":"wait","after":revision,"timeout_ms":0}),
            Duration::from_secs(10),
        );
        if probe["error"]["code"] == "busy" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: server never saturated its general slots: {probe}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stop a saturation server, fail loudly when it does not exit, and collect
/// the long-poll waiters it wakes.
fn saturation_stop(
    socket: &std::path::Path,
    child: &mut std::process::Child,
    waiters: Vec<std::thread::JoinHandle<serde_json::Value>>,
    label: &str,
) {
    use std::time::{Duration, Instant};
    let stop = saturation_request(
        socket,
        serde_json::json!({"op":"control","action":"stop"}),
        Duration::from_secs(10),
    );
    assert_eq!(stop["ok"], true, "{label}: stop stays served: {stop}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("{label}: live server did not stop");
    }
    for waiter in waiters {
        let reply = waiter.join().expect("wait thread finishes after stop");
        assert_eq!(reply["ok"], true, "a woken wait answers: {reply}");
    }
}

/// A socket parent the live server accepts: `tempfile` dirs inherit the
/// umask, so claim the directory private explicitly instead of relying on it.
fn saturation_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn saturation_program(directory: &std::path::Path) -> std::path::PathBuf {
    let program = directory.join("tool.program");
    std::fs::write(
        &program,
        "accept name as text with default \"x\"\nrun \"printf\" with arguments [\"finished\"]\n",
    )
    .unwrap();
    program
}

/// Slow and partial same-UID senders must never hold the reserve lane: 16
/// waits fill every general slot while one connection stalls mid-line and
/// another drips a byte at a time, yet read and stop are still served. No
/// sleeps: partials cannot occupy reserve slots by construction, and the gate
/// proves the general slots are genuinely full before probing control.
#[test]
fn live_stop_survives_partial_senders_holding_no_slot() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const WAITERS: usize = 16;
    let directory = saturation_dir();
    let program = saturation_program(directory.path());
    let socket = directory.path().join("control.sock");
    let mut child = saturation_server(&program, &socket);
    let revision = saturation_revision(&socket);
    let waiters: Vec<_> = (0..WAITERS)
        .map(|_| saturation_waiter(socket.clone(), revision))
        .collect();
    saturation_gate(&socket, "partial senders");
    // Open the slow senders after the gate so they are provably still pending
    // at probe time: one stalls mid-line, the other drips a byte at a time.
    // Neither may ever claim a slot, let alone a reserve one.
    let mut stalled = UnixStream::connect(&socket).unwrap();
    stalled
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stalled, "{{\"op\":\"wait\",\"after\":").unwrap();
    let dripper = std::thread::spawn({
        let socket = socket.clone();
        move || {
            use std::io::{BufRead, BufReader};
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            for byte in br#"{"op":"wa"# {
                // The server may stop (closing the socket) before the drip
                // finishes; either way this sender never holds a slot.
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // The server stops before the drip completes; the read then ends.
            let mut response = String::new();
            let _ = BufReader::new(stream).read_line(&mut response);
        }
    });
    let read = saturation_request(
        &socket,
        serde_json::json!({"op":"read"}),
        Duration::from_secs(10),
    );
    assert_eq!(
        read["ok"], true,
        "read is served past partial senders: {read}"
    );
    drop(stalled);
    saturation_stop(&socket, &mut child, waiters, "partial senders");
    dripper.join().unwrap();
}

/// A silent-connection flood far past the bounded pending queue must not deny
/// control: incomplete holders are evicted oldest-first while a fully-read
/// stop is admitted to the reserve and ends the run.
#[test]
fn live_stop_survives_a_pending_queue_flood() {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const WAITERS: usize = 16;
    const PARTIALS: usize = 96;
    let directory = saturation_dir();
    let program = saturation_program(directory.path());
    let socket = directory.path().join("control.sock");
    let mut child = saturation_server(&program, &socket);
    let revision = saturation_revision(&socket);
    let waiters: Vec<_> = (0..WAITERS)
        .map(|_| saturation_waiter(socket.clone(), revision))
        .collect();
    saturation_gate(&socket, "pending flood");
    // Open the flood after the gate: far more silent connections than the
    // bounded pending queue holds, all provably still pending at probe time.
    // Each costs the server only a bounded pending entry until it is evicted,
    // never a thread or a slot.
    let partials: Vec<UnixStream> = (0..PARTIALS)
        .map(|_| {
            let stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
        })
        .collect();
    let read = saturation_request(
        &socket,
        serde_json::json!({"op":"read"}),
        Duration::from_secs(10),
    );
    assert_eq!(
        read["ok"], true,
        "read is served past a pending flood: {read}"
    );
    drop(partials);
    saturation_stop(&socket, &mut child, waiters, "pending flood");
}

/// While every general slot holds a long poll, non-priority traffic is shed
/// with a `busy` code before any state or policy work, reads stay available,
/// and the schema reports the real admission bounds.
#[test]
fn live_saturated_general_lane_sheds_with_busy_and_reports_real_limits() {
    use std::time::Duration;

    const WAITERS: usize = 16;
    let directory = saturation_dir();
    let program = saturation_program(directory.path());
    let socket = directory.path().join("control.sock");
    let mut child = saturation_server(&program, &socket);
    let revision = saturation_revision(&socket);
    let waiters: Vec<_> = (0..WAITERS)
        .map(|_| saturation_waiter(socket.clone(), revision))
        .collect();
    saturation_gate(&socket, "shed semantics");
    // A shed non-priority wait carries the busy code instead of hanging or
    // silently dropping, and a shed mutation is refused before touching state.
    let shed = saturation_request(
        &socket,
        serde_json::json!({"op":"wait","after":revision,"timeout_ms":0}),
        Duration::from_secs(10),
    );
    assert_eq!(shed["ok"], false, "saturated non-priority sheds: {shed}");
    assert_eq!(
        shed["error"]["code"], "busy",
        "shed carries the busy code: {shed}"
    );
    let refused = saturation_request(
        &socket,
        serde_json::json!({"op":"control","action":"pause"}),
        Duration::from_secs(10),
    );
    assert_eq!(
        refused["error"]["code"], "busy",
        "shed happens before control work: {refused}"
    );
    let read = saturation_request(
        &socket,
        serde_json::json!({"op":"read"}),
        Duration::from_secs(10),
    );
    assert_eq!(read["ok"], true, "read stays served while shedding: {read}");
    saturation_stop(&socket, &mut child, waiters, "shed semantics");
}

/// The schema reports the actual admission bounds: general slots plus the
/// priority spare, the pending queue, and which requests the reserve serves.
#[test]
fn live_schema_reports_real_admission_limits() {
    use std::process::{Command, Stdio};
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("live")
        .arg("schema")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let limits = &schema["limits"];
    assert_eq!(limits["general_clients"], 16, "general slots: {limits}");
    assert_eq!(limits["priority_spare"], 2, "reserve spare: {limits}");
    assert!(limits["pending_connections"].as_u64().unwrap_or(0) > 0);
    assert!(limits["priority_classify_bytes"].as_u64().unwrap_or(0) > 0);
    assert!(limits["pending_timeout_ms"].as_u64().unwrap_or(0) > 0);
    // Range checks only: the exact burst/refill numbers live in the
    // deterministic `FloodBudget` unit tests in src/live/state.rs, so schema
    // retunes do not break this integration test. This gate only pins the
    // burst-tolerant shape (a sizable burst, a much smaller sustained rate).
    let burst = limits["burst_accepts"]
        .as_u64()
        .expect("schema limits must report burst_accepts");
    let sustained = limits["sustained_accepts_per_second"]
        .as_u64()
        .expect("schema limits must report sustained_accepts_per_second");
    assert!(
        (512..=8192).contains(&burst),
        "burst budget admits legitimate bursts: {limits}"
    );
    assert!(
        (1..=512).contains(&sustained),
        "sustained rate bounds floods: {limits}"
    );
    assert!(
        burst > sustained * 10,
        "burst must dwarf the sustained rate: {limits}"
    );
    // Backwards compatibility: `concurrent_clients` stays as a deprecated
    // alias for `general_clients` so strict pre-reserve schema readers keep
    // validating. Both must report the same truthful general-slot count.
    assert_eq!(
        limits["concurrent_clients"], limits["general_clients"],
        "deprecated concurrent_clients alias must track general_clients: {limits}"
    );
    assert_eq!(
        limits["concurrent_clients"], 16,
        "concurrent_clients alias keeps the general-slot count: {limits}"
    );
    assert!(
        limits.get("accepts_per_second").is_none(),
        "stale accepts_per_second must go: {limits}"
    );
    let priority = limits["priority_requests"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(priority.contains(&serde_json::json!("read")));
    assert!(priority.contains(&serde_json::json!("control stop")));
}

/// Benign bursts must never shed with `busy` while general slots are free.
/// Rapid sequential read/wait polls plus a concurrent CRUD burst stay served:
/// 400 polls plus 800 CRUD operations fit well within the 2048-request burst
/// budget, and priority reads never spend it. Only slot exhaustion or a
/// sustained past-burst flood sheds. This test holds no long waiter, so every
/// reply must be `ok`. No sleeps: the burst itself is the load, and timeouts
/// bound every request.
#[test]
fn live_benign_burst_is_served_while_slots_are_free() {
    use std::time::{Duration, Instant};

    let directory = saturation_dir();
    let program = saturation_program(directory.path());
    let socket = directory.path().join("control.sock");
    let mut child = saturation_server(&program, &socket);
    // Phase 1: tight sequential read/wait polls, the backpressure shape. A
    // zero-timeout wait returns immediately, so this loop connects far faster
    // than any per-second rate bound; with no waiter holding a slot, none of
    // it may shed.
    for _ in 0..200 {
        let read = saturation_request(
            &socket,
            serde_json::json!({"op":"read"}),
            Duration::from_secs(10),
        );
        assert_eq!(read["ok"], true, "sequential read stays served: {read}");
        let revision = read["revision"].as_u64().expect("read reports a revision");
        let wait = saturation_request(
            &socket,
            serde_json::json!({"op":"wait","after":revision,"timeout_ms":0}),
            Duration::from_secs(10),
        );
        assert_eq!(wait["ok"], true, "idle poll stays served: {wait}");
    }
    // Phase 2: concurrent CRUD burst on isolated paths, the thousand-request
    // shape: 8 workers x 25 iterations x 4 operations, each a fresh
    // connection. Peak concurrency stays far below the 16 general slots.
    let workers: Vec<_> = (0..8)
        .map(|worker| {
            let socket = socket.clone();
            std::thread::spawn(move || {
                for iteration in 0..25 {
                    let path = format!("/user/burst-{worker}-{iteration}");
                    let create = saturation_request(
                        &socket,
                        serde_json::json!({"op":"create","path":path.clone(),"value":{"value":0}}),
                        Duration::from_secs(10),
                    );
                    assert_eq!(create["ok"], true, "burst create stays served: {create}");
                    let update = saturation_request(
                        &socket,
                        serde_json::json!({"op":"update","path":format!("{path}/value"),"value":1}),
                        Duration::from_secs(10),
                    );
                    assert_eq!(update["ok"], true, "burst update stays served: {update}");
                    let read = saturation_request(
                        &socket,
                        serde_json::json!({"op":"read","path":format!("{path}/value")}),
                        Duration::from_secs(10),
                    );
                    assert_eq!(read["result"], 1, "burst read stays served: {read}");
                    let delete = saturation_request(
                        &socket,
                        serde_json::json!({"op":"delete","path":path.clone()}),
                        Duration::from_secs(10),
                    );
                    assert_eq!(delete["ok"], true, "burst delete stays served: {delete}");
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("burst worker finishes");
    }
    let read = saturation_request(
        &socket,
        serde_json::json!({"op":"read"}),
        Duration::from_secs(10),
    );
    assert_eq!(
        read["ok"], true,
        "server stays served after the burst: {read}"
    );
    let stop = saturation_request(
        &socket,
        serde_json::json!({"op":"control","action":"stop"}),
        Duration::from_secs(10),
    );
    assert_eq!(
        stop["ok"], true,
        "stop stays served after the burst: {stop}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("burst test: live server did not stop");
    }
}

/// A sustained same-user flood past the burst budget sheds with `busy` while
/// priority traffic stays served. Thirty-two hundred back-to-back
/// zero-timeout waits exceed the burst plus the small refill a fast run
/// accrues (a few seconds at 64/s), so at least some shed; interleaved reads
/// prove the fast path never spends the budget, and stop ends the run
/// afterwards. No sleeps and no slot holders: the request rate itself is the
/// flood, and timeouts bound every request. Exact burst/refill counts are
/// pinned by the deterministic `FloodBudget` unit tests, not here, so slow
/// CI only risks a longer run, never a wrong count. The loop exits early
/// once both sides are observed to keep slow machines fast.
/// (Timing-sensitive by construction: a pathologically slow machine that
/// refills faster than it connects could admit everything; the unit tests
/// are the deterministic shed gate.)
#[test]
fn live_sustained_flood_sheds_while_read_and_stop_stay_served() {
    use std::time::{Duration, Instant};

    const FLOOD: usize = 3200;
    let directory = saturation_dir();
    let program = saturation_program(directory.path());
    let socket = directory.path().join("control.sock");
    let mut child = saturation_server(&program, &socket);
    let revision = saturation_revision(&socket);
    let mut admitted = 0usize;
    let mut shed = 0usize;
    let deadline = Instant::now() + Duration::from_secs(60);
    for index in 0..FLOOD {
        assert!(
            Instant::now() < deadline,
            "flood did not finish promptly (admitted {admitted}, shed {shed})"
        );
        if index % 400 == 0 {
            let read = saturation_request(
                &socket,
                serde_json::json!({"op":"read"}),
                Duration::from_secs(10),
            );
            assert_eq!(read["ok"], true, "read stays served mid-flood: {read}");
        }
        let reply = saturation_request(
            &socket,
            serde_json::json!({"op":"wait","after":revision,"timeout_ms":0}),
            Duration::from_secs(10),
        );
        if reply["error"]["code"] == "busy" {
            shed += 1;
        } else {
            assert_eq!(
                reply["ok"], true,
                "admitted flood request is served: {reply}"
            );
            admitted += 1;
        }
        // Fast-path exit: once the burst has been visibly admitted and sheds
        // have started, further requests prove nothing new.
        if admitted >= 500 && shed >= 10 {
            break;
        }
    }
    assert!(
        shed > 0,
        "a sustained flood past the burst must shed (admitted {admitted})"
    );
    assert!(
        admitted > 0,
        "the burst budget must admit before shedding (shed {shed})"
    );
    let read = saturation_request(
        &socket,
        serde_json::json!({"op":"read"}),
        Duration::from_secs(10),
    );
    assert_eq!(
        read["ok"], true,
        "read stays served after the flood: {read}"
    );
    let stop = saturation_request(
        &socket,
        serde_json::json!({"op":"control","action":"stop"}),
        Duration::from_secs(10),
    );
    assert_eq!(
        stop["ok"], true,
        "stop stays served after the flood: {stop}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("flood test: live server did not stop");
    }
}
