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
        _ => std::process::exit(64),
    }
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
    for line in stdin.lock().lines() {
        let line = line.expect("reading worker request");
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("decoding worker request");
        let request_id = request["id"].as_u64().expect("request id");
        let input = request["input"]
            .as_array()
            .expect("request input bytes")
            .iter()
            .map(|byte| u8::try_from(byte.as_u64().expect("byte value")).expect("byte in range"))
            .collect::<Vec<_>>();
        let mut response_id = request_id;
        let mut status = 0i32;
        let mut output = input;
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

        let response = serde_json::json!({
            "id": response_id,
            "status": status,
            "output": output,
            "stderr": stderr,
        });
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
