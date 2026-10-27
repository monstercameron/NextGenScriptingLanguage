//! Owned executable fixtures for native Windows/Unix process contract tests.
use std::io::{BufRead, Read, Write};
// This fixture intentionally lets the helper process outlive its short-lived parent
// so the tests can exercise inherited-stderr draining after the parent exits.
#[cfg(unix)]
#[allow(clippy::zombie_processes)]
fn spawn_detached_child(command: &mut std::process::Command) -> std::io::Result<()> {
    let _child = command.spawn()?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("") {
        "argv" => serde_json::to_writer(std::io::stdout(), &args[1..]).unwrap(),
        "json-lines" => json_lines_worker(&args[1..]),
        "write" => {
            std::io::stdout().write_all(args[1].as_bytes()).unwrap();
        }
        "repeat-byte" => {
            let mut remaining: usize = args[1].parse().unwrap();
            let bytes = [b'x'; 4096];
            let mut stdout = std::io::stdout().lock();
            while remaining > 0 {
                let next = remaining.min(bytes.len());
                stdout.write_all(&bytes[..next]).unwrap();
                remaining -= next;
            }
        }
        "flood-lines" => {
            // Fill more than an anonymous pipe can hold. After a downstream
            // reader exits, choose a process-defined status as many Windows
            // runtimes do when stdout reports ERROR_NO_DATA / ERROR_BROKEN_PIPE.
            let status: i32 = args[1].parse().unwrap();
            let mut stdout = std::io::stdout().lock();
            let chunk = [b'x'; 8192];
            let result = (|| -> std::io::Result<()> {
                stdout.write_all(b"first\n")?;
                for _ in 0..128 {
                    stdout.write_all(&chunk)?;
                }
                stdout.flush()
            })();
            if let Err(error) = result {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    std::process::exit(status);
                }
                std::process::exit(90);
            }
        }
        "flood-independent-failure" => {
            // Even if the reader closes, this process exits with its own
            // unrelated failure status after attempting a large write.
            let mut stdout = std::io::stdout().lock();
            let chunk = [b'x'; 8192];
            let _ = stdout.write_all(b"first\n");
            for _ in 0..128 {
                match stdout.write_all(&chunk) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => break,
                    Err(_) => std::process::exit(90),
                }
            }
            std::process::exit(42);
        }
        "first-line" => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            std::io::stdout().write_all(line.as_bytes()).unwrap();
            std::io::stdout().flush().unwrap();
        }
        "delayed-copy" => {
            std::thread::sleep(std::time::Duration::from_millis(250));
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout()).unwrap();
        }
        "copy" => {
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout()).unwrap();
        }
        "upper" => {
            let mut data = Vec::new();
            std::io::stdin()
                .take(16 * 1024 * 1024)
                .read_to_end(&mut data)
                .unwrap();
            data.make_ascii_uppercase();
            std::io::stdout().write_all(&data).unwrap();
        }
        "exit" => {
            if let Some(data) = args.get(2) {
                print!("{data}");
                std::io::stdout().flush().unwrap();
            }
            eprintln!("fixture failed");
            std::process::exit(args[1].parse().unwrap());
        }
        "produce" => {
            for text in ["one\n", "two\n", "three\n"] {
                print!("{text}");
                std::io::stdout().flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(35));
            }
        }
        "produce-exit" => {
            println!("event");
            std::io::stdout().flush().unwrap();
            std::process::exit(args[1].parse().unwrap());
        }
        "sleep" => std::thread::sleep(std::time::Duration::from_millis(
            args[1].parse::<u64>().unwrap().min(10000),
        )),
        "late-write" => {
            std::thread::sleep(std::time::Duration::from_millis(700));
            std::fs::write(&args[1], b"escaped").unwrap();
        }
        "spawn" => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["late-write", &args[1]])
                .spawn()
                .unwrap();
            println!("started");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            child.wait().unwrap();
        }
        "increment" => {
            let p = std::path::Path::new(&args[1]);
            let n = std::fs::read_to_string(p)
                .unwrap_or_else(|_| "0".into())
                .parse::<u32>()
                .unwrap()
                + 1;
            std::fs::write(p, n.to_string()).unwrap();
            print!("{n}");
        }
        "count-wait" => {
            // args: counter directory, element name, gate directory. Counts this
            // run in `<dir>/<name>`, then waits while `<gate>/<name>.hold` exists.
            let path = std::path::Path::new(&args[1]).join(&args[2]);
            let n = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| "0".into())
                .parse::<u32>()
                .unwrap()
                + 1;
            std::fs::write(&path, n.to_string()).unwrap();
            let hold = std::path::Path::new(&args[3]).join(format!("{}.hold", args[2]));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while hold.exists() {
                if std::time::Instant::now() >= deadline {
                    std::process::exit(3);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            print!("{n}");
        }
        "flaky" => {
            // flaky COUNTER FAILURES STATUS: count this run, and exit STATUS while
            // the count is at most FAILURES. Otherwise print the count and succeed.
            let p = std::path::Path::new(&args[1]);
            let n = std::fs::read_to_string(p)
                .unwrap_or_else(|_| "0".into())
                .parse::<u32>()
                .unwrap()
                + 1;
            std::fs::write(p, n.to_string()).unwrap();
            let failures: u32 = args[2].parse().unwrap();
            if n <= failures {
                eprintln!("fixture busy");
                std::process::exit(args[3].parse().unwrap());
            }
            print!("{n}");
        }
        "stderr-bytes" => {
            let count: usize = args[1].parse().unwrap();
            let bytes = [b'e'; 4096];
            let mut remaining = count;
            let mut sink = std::io::stderr().lock();
            while remaining > 0 {
                let next = remaining.min(bytes.len());
                sink.write_all(&bytes[..next]).unwrap();
                remaining -= next;
            }
            if let Some(status) = args.get(2) {
                std::process::exit(status.parse().unwrap());
            }
        }
        #[cfg(unix)]
        "hold-stderr-after-parent-exit" => {
            use std::os::unix::process::CommandExt;
            let marker = &args[1];
            let release = &args[2];
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["hold-stderr-child", marker, release])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit());
            // SAFETY: setsid is async-signal-safe and the closure uses no shared
            // state in the forked child before exec.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            spawn_detached_child(&mut command).unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !std::path::Path::new(marker).exists() {
                assert!(
                    std::time::Instant::now() < until,
                    "descendant did not start"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        #[cfg(unix)]
        "hold-stderr-child" => {
            std::fs::write(&args[1], b"started").unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !std::path::Path::new(&args[2]).exists() && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::fs::write(format!("{}.done", args[1]), b"done").unwrap();
        }
        "stderr-flood" => {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let bytes = [b'e'; 8192];
            let mut sink = std::io::stderr().lock();
            while std::time::Instant::now() < until {
                if sink.write_all(&bytes).is_err() {
                    break;
                }
            }
        }
        "flood" => {
            let bytes = [b'x'; 32768];
            for _ in 0..128 {
                if std::io::stdout().write_all(&bytes).is_err() {
                    break;
                }
            }
        }
        "stream-probe" => {
            // args: per-stream byte count, then a status-file path. Stdout
            // frames carry `~526OUT~` headers, stderr frames `~526ERR~`, so
            // the parent counts uniquely framed records instead of raw bytes
            // that also occur in trace JSON. Exit 0 only when every byte
            // lands; 42 when O_NONBLOCK is ever observed on the inherited
            // stderr; 43 when a write fails (EAGAIN included). The status
            // file repeats the outcome so the parent has exit/status proof
            // independent of execute's status propagation.
            let total: usize = args[1].parse().unwrap();
            let status_file = args[2].clone();
            stream_probe(total, &status_file);
        }
        _ => std::process::exit(64),
    }
}

/// Magic record headers for the issue #526 stream probe. Trace JSON and the
/// `output-line` message never contain these markers, so the parent counts
/// framed records instead of raw bytes that also occur in trace output.
#[cfg(unix)]
const STREAM_PROBE_OUT_MAGIC: &[u8] = b"~526OUT~";
#[cfg(unix)]
const STREAM_PROBE_ERR_MAGIC: &[u8] = b"~526ERR~";
#[cfg(unix)]
const STREAM_PROBE_FRAME_LEN: usize = 8192;

#[cfg(unix)]
#[derive(PartialEq, Eq)]
enum StreamProbeOutcome {
    Ok,
    Nonblocking,
    ShortWrite,
}

#[cfg(unix)]
fn stream_probe(total: usize, status_file: &str) {
    let outcome = stream_probe_inner(total);
    // The status file is a regular file inside the test's tempdir, so this
    // write is unaffected by the shared pipe under test. A missing or
    // unexpected status line fails the parent even if exit-code propagation
    // through execute ever changes.
    let line = match outcome {
        StreamProbeOutcome::Ok => "probe-526-ok\n",
        StreamProbeOutcome::Nonblocking => "probe-526-nonblock\n",
        StreamProbeOutcome::ShortWrite => "probe-526-short\n",
    };
    if std::fs::write(status_file, line).is_err() {
        std::process::exit(43);
    }
    match outcome {
        StreamProbeOutcome::Ok => {}
        StreamProbeOutcome::Nonblocking => std::process::exit(42),
        StreamProbeOutcome::ShortWrite => std::process::exit(43),
    }
}

#[cfg(unix)]
fn stream_probe_inner(total: usize) -> StreamProbeOutcome {
    use std::sync::atomic::{AtomicBool, Ordering};

    let nonblocking_seen = std::sync::Arc::new(AtomicBool::new(false));
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let monitor_seen = std::sync::Arc::clone(&nonblocking_seen);
    let monitor_stop = std::sync::Arc::clone(&stop);
    let monitor = std::thread::spawn(move || {
        while !monitor_stop.load(Ordering::Acquire) {
            // SAFETY: fd 2 is this process's inherited stderr, open for the
            // whole probe. F_GETFL only reads its status flags.
            let flags = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL) };
            if flags == -1 || flags & libc::O_NONBLOCK != 0 {
                monitor_seen.store(true, Ordering::Release);
            }
            std::hint::spin_loop();
        }
    });
    let writer_seen = std::sync::Arc::clone(&nonblocking_seen);
    let stdout_writer = std::thread::spawn(move || {
        write_framed_raw(
            libc::STDOUT_FILENO,
            STREAM_PROBE_OUT_MAGIC,
            b'O',
            total,
            &writer_seen,
        )
    });
    let stderr_ok = write_framed_raw(
        libc::STDERR_FILENO,
        STREAM_PROBE_ERR_MAGIC,
        b'E',
        total,
        &nonblocking_seen,
    );
    let stdout_ok = stdout_writer.join().unwrap();
    stop.store(true, Ordering::Release);
    monitor.join().unwrap();
    if nonblocking_seen.load(Ordering::Acquire) {
        return StreamProbeOutcome::Nonblocking;
    }
    if !stderr_ok || !stdout_ok {
        return StreamProbeOutcome::ShortWrite;
    }
    StreamProbeOutcome::Ok
}

/// Write exactly `total` bytes to `fd` as newline-terminated frames that each
/// start with `magic`, without changing any descriptor flags. Newline
/// termination keeps line-oriented forwarding flowing while the probe runs.
/// Only EINTR retries; a WouldBlock (EAGAIN) return means a blocking shared
/// descriptor was corrupted and fails the probe.
#[cfg(unix)]
fn write_framed_raw(
    fd: libc::c_int,
    magic: &[u8],
    fill: u8,
    total: usize,
    nonblocking_seen: &std::sync::atomic::AtomicBool,
) -> bool {
    let mut frame = [fill; STREAM_PROBE_FRAME_LEN];
    frame[..magic.len()].copy_from_slice(magic);
    frame[STREAM_PROBE_FRAME_LEN - 1] = b'\n';
    let mut remaining = total;
    while remaining > 0 {
        if fd == libc::STDERR_FILENO {
            // SAFETY: stderr is open for the whole probe; F_GETFL only reads
            // its status flags. The monitor thread can miss a brief window,
            // so the writer re-checks before every frame.
            let flags = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL) };
            if flags == -1 || flags & libc::O_NONBLOCK != 0 {
                nonblocking_seen.store(true, std::sync::atomic::Ordering::Release);
                return false;
            }
        }
        let next = remaining.min(frame.len());
        let mut offset = 0;
        while offset < next {
            // SAFETY: `offset < next <= frame.len()` and `fd` is an open
            // writer owned by this process (stdout or the inherited stderr).
            // The call changes no status flags.
            let count =
                unsafe { libc::write(fd, frame[offset..next].as_ptr().cast(), next - offset) };
            if count > 0 {
                offset += count as usize;
                continue;
            }
            if count == 0 {
                return false;
            }
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        remaining -= next;
    }
    true
}

#[cfg(not(unix))]
fn stream_probe(total: usize, _status_file: &str) {
    // The regression test driving this probe is Unix-only; keep other
    // platforms compiling with plain exact-count writes.
    std::io::stdout().write_all(&vec![b'o'; total]).unwrap();
    std::io::stderr().write_all(&vec![b'e'; total]).unwrap();
}

fn is_handshake_request(request: &serde_json::Value) -> bool {
    request.get("type").and_then(serde_json::Value::as_str) == Some("handshake")
        && request.get("protocol").and_then(serde_json::Value::as_str)
            == Some("ngsl.json-lines-worker")
        && request
            .get("versions")
            .and_then(serde_json::Value::as_array)
            .is_some()
        && !request
            .as_object()
            .is_some_and(|object| object.contains_key("id") || object.contains_key("arguments"))
}

fn json_lines_worker(arguments: &[String]) {
    let mode = arguments.first().map(String::as_str).unwrap_or("echo");
    let marker = arguments
        .get(1)
        .expect("json-lines fixture needs a launch marker");
    let effects = arguments.get(2);
    use std::fs::OpenOptions;
    let mut marker_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
        .expect("opening worker launch marker");
    writeln!(marker_file, "{}", std::process::id()).expect("recording worker pid");

    if mode == "ignore" {
        std::thread::sleep(std::time::Duration::from_secs(30));
        return;
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut sequence = 0u64;
    let mut v2_negotiated = false;
    for line in stdin.lock().lines() {
        let line = line.expect("reading worker request");
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("decoding worker request");
        if is_handshake_request(&request) {
            if v2_negotiated {
                let response = serde_json::json!({
                    "type":"handshake",
                    "protocol":"ngsl.json-lines-worker",
                    "error":"protocol version cannot be renegotiated"
                });
                serde_json::to_writer(&mut stdout, &response)
                    .expect("encoding renegotiation error");
                stdout
                    .write_all(b"\n")
                    .expect("terminating renegotiation error");
                stdout.flush().expect("flushing renegotiation error");
                continue;
            }
            let valid = request.get("protocol").and_then(serde_json::Value::as_str)
                == Some("ngsl.json-lines-worker")
                && request
                    .get("versions")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|versions| {
                        versions.iter().any(|version| version.as_u64() == Some(2))
                    });
            v2_negotiated |= valid;
            let response = if valid {
                serde_json::json!({
                    "type":"handshake",
                    "protocol":"ngsl.json-lines-worker",
                    "version":2
                })
            } else {
                serde_json::json!({"type":"handshake","version":1})
            };
            serde_json::to_writer(&mut stdout, &response).expect("encoding handshake response");
            stdout
                .write_all(b"\n")
                .expect("terminating handshake response");
            stdout.flush().expect("flushing handshake response");
            continue;
        }
        let request_version = request.get("version").and_then(serde_json::Value::as_u64);
        if !v2_negotiated
            && request_version == Some(2)
            && request.get("input_base64").is_some()
            && request.get("input").is_none()
        {
            let response = serde_json::json!({
                "id": request.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0),
                "status": 2,
                "output": [],
                "stderr": "fixture: v2 frame received before handshake"
            });
            serde_json::to_writer(&mut stdout, &response)
                .expect("encoding unnegotiated v2 frame error");
            stdout
                .write_all(b"\n")
                .expect("terminating unnegotiated v2 frame error");
            stdout
                .flush()
                .expect("flushing unnegotiated v2 frame error");
            continue;
        }
        if v2_negotiated && request_version != Some(2) {
            let mut response = serde_json::json!({
                "version": 2,
                "id": request.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0),
                "status": 2,
            });
            if request["json_output"].as_bool().unwrap_or(false) {
                response["value"] = serde_json::Value::Null;
            } else {
                response["output_base64"] = serde_json::Value::String(String::new());
            }
            response["stderr"] = serde_json::Value::String(
                "fixture: frame version is missing or unsupported after v2 handshake".into(),
            );
            serde_json::to_writer(&mut stdout, &response).expect("encoding v2 version error");
            stdout
                .write_all(b"\n")
                .expect("terminating v2 version error");
            stdout.flush().expect("flushing v2 version error");
            continue;
        }
        let protocol_v2 = v2_negotiated && request_version == Some(2);
        let request_id = request["id"].as_u64().expect("request id");
        let input = if protocol_v2 {
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                request["input_base64"]
                    .as_str()
                    .expect("v2 input Base64 string"),
            )
            .expect("decoding v2 input Base64")
        } else {
            request["input"]
                .as_array()
                .expect("request input bytes")
                .iter()
                .map(|byte| {
                    u8::try_from(byte.as_u64().expect("byte value")).expect("byte in range")
                })
                .collect::<Vec<_>>()
        };
        let mut response_id = request_id;
        let mut status = 0i32;
        let mut output = input;
        let mut typed_value = None;
        let mut stderr = None;

        match mode {
            "sequence" => {
                sequence += 1;
                output = sequence.to_string().into_bytes();
            }
            "typed" => {
                output = br#"{"n":42}"#.to_vec();
                stderr = Some("é: diagnostic");
            }
            "echo-value" => {
                typed_value = request
                    .get("value")
                    .and_then(|value| value.get("payload"))
                    .cloned();
                if typed_value.is_none() {
                    status = 2;
                }
            }
            "status" => {
                status = -17;
                output = vec![0, 255, 1];
                stderr = Some("é: diagnostic");
            }
            "wrong-id" => response_id = response_id.wrapping_add(1),
            "invalid-json" => {
                stdout.write_all(b"{invalid json}\n").unwrap();
                stdout.flush().unwrap();
                return;
            }
            "partial" => {
                stdout.write_all(b"{\"id\":").unwrap();
                stdout.flush().unwrap();
                return;
            }
            "eof" => return,
            "oversized" => output = vec![b'x'; 256],
            "slow" => {
                if let Some(effects) = effects {
                    let mut file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(effects)
                        .expect("opening effect marker");
                    writeln!(file, "received").expect("recording request effect");
                }
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
            _ => {}
        }

        let response = if protocol_v2 {
            let mut response = serde_json::json!({
                "version": 2,
                "id": response_id,
                "status": status,
            });
            if request["json_output"].as_bool().unwrap_or(false) {
                let value = typed_value.unwrap_or_else(|| {
                    serde_json::from_slice(&output).expect("typed JSON worker output")
                });
                response["value"] = value;
            } else {
                response["output_base64"] = serde_json::Value::String(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &output,
                ));
            }
            if let Some(stderr) = stderr {
                response["stderr"] = serde_json::Value::String(stderr.to_owned());
            }
            response
        } else {
            serde_json::json!({
                "id": response_id,
                "status": status,
                "output": output,
                "stderr": stderr,
            })
        };
        serde_json::to_writer(&mut stdout, &response).expect("encoding worker response");
        stdout.write_all(b"\n").expect("terminating worker frame");
        stdout.flush().expect("flushing worker response");
        if mode == "trailing-and-exit" {
            let effects = effects.expect("trailing-output fixture needs a release marker");
            while !std::path::Path::new(effects).exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            stdout
                .write_all(b"unsolicited\n")
                .expect("writing unsolicited worker bytes");
            stdout.flush().expect("flushing unsolicited worker bytes");
            return;
        }
        if mode == "exit-after-one" {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_handshake_request;

    #[test]
    fn handshake_discriminator_preserves_legacy_extension_requests() {
        let handshake = serde_json::json!({
            "type": "handshake",
            "protocol": "ngsl.json-lines-worker",
            "versions": [2]
        });
        assert!(is_handshake_request(&handshake));

        let legacy_request = serde_json::json!({
            "type": "handshake",
            "protocol": "ngsl.json-lines-worker",
            "versions": [2],
            "id": 8,
            "arguments": ["legacy.operation"]
        });
        assert!(!is_handshake_request(&legacy_request));
    }
}
