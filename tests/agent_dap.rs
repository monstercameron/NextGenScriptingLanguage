#![cfg(feature = "dap")]
//! `execute dap`: DAP framing, and a scripted session that launches a program with a
//! breakpoint, pauses, inspects, steps, continues and terminates.

use flow::agent::dap::{read_message, write_message};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufReader, Cursor};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn framing_round_trips_multibyte_bodies_and_rejects_bad_headers() {
    let first = json!({"type": "request", "command": "threads", "text": "é 😀"});
    let second = json!({"type": "event", "event": "terminated"});
    let mut bytes = Vec::new();
    write_message(&mut bytes, &first).unwrap();
    write_message(&mut bytes, &second).unwrap();

    // Content-Length counts bytes, not characters.
    let header = String::from_utf8_lossy(&bytes[..bytes.iter().position(|b| *b == b'{').unwrap()])
        .into_owned();
    let body_length = serde_json::to_vec(&first).unwrap().len();
    assert!(header.starts_with(&format!("Content-Length: {body_length}\r\n\r\n")));

    let mut reader = BufReader::new(Cursor::new(bytes));
    assert_eq!(read_message(&mut reader).unwrap(), Some(first));
    assert_eq!(read_message(&mut reader).unwrap(), Some(second));
    assert_eq!(read_message(&mut reader).unwrap(), None, "end of input");

    // Header names are case-insensitive, and unknown headers are ignored.
    let lowercase = b"content-length: 2\r\nX-Other: 1\r\n\r\n{}".to_vec();
    let mut reader = BufReader::new(Cursor::new(lowercase));
    assert_eq!(read_message(&mut reader).unwrap(), Some(json!({})));

    let missing = b"Content-Type: x\r\n\r\n{}".to_vec();
    let mut reader = BufReader::new(Cursor::new(missing));
    assert!(read_message(&mut reader).is_err());

    let truncated = b"Content-Length: 50\r\n\r\n{}".to_vec();
    let mut reader = BufReader::new(Cursor::new(truncated));
    assert!(read_message(&mut reader).is_err());
}

/// A DAP client over a spawned `execute dap`. Messages that do not match the wait
/// are kept, so events and responses can arrive in any order.
struct Client {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    pending: VecDeque<Value>,
    seq: i64,
}

impl Client {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("dap")
            .env_remove("EXECUTE_MODELS_OFFLINE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start execute dap");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, incoming) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(message)) = read_message(&mut reader) {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            incoming,
            pending: VecDeque::new(),
            seq: 0,
        }
    }

    fn send(&mut self, message: Value) {
        write_message(&mut self.stdin, &message).expect("write DAP request");
    }

    /// Send a request and return its response.
    fn request(&mut self, command: &str, arguments: Value) -> Value {
        self.seq += 1;
        let seq = self.seq;
        self.send(
            json!({"seq": seq, "type": "request", "command": command, "arguments": arguments}),
        );
        self.wait(|message| message["type"] == "response" && message["request_seq"] == json!(seq))
            .unwrap_or_else(|| panic!("no response to {command}"))
    }

    /// Wait for the next event named `name`.
    fn event(&mut self, name: &str) -> Value {
        self.wait(|message| message["type"] == "event" && message["event"] == name)
            .unwrap_or_else(|| panic!("no {name} event"))
    }

    fn wait(&mut self, matches: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(index) = self.pending.iter().position(&matches) {
            return self.pending.remove(index);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let message = self.incoming.recv_timeout(remaining).ok()?;
            if matches(&message) {
                return Some(message);
            }
            self.pending.push_back(message);
        }
    }

    /// Every output event seen so far, concatenated.
    fn output(&mut self) -> String {
        let mut text = String::new();
        let mut kept = VecDeque::new();
        for message in self.pending.drain(..) {
            if message["type"] == "event" && message["event"] == "output" {
                text.push_str(message["body"]["output"].as_str().unwrap_or_default());
            } else {
                kept.push_back(message);
            }
        }
        self.pending = kept;
        text
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Disconnect first, so the adapter stops any hosted run it started. Killing the
        // adapter alone would leave that run behind.
        self.seq += 1;
        let seq = self.seq;
        let _ = write_message(
            &mut self.stdin,
            &json!({"seq": seq, "type": "request", "command": "disconnect", "arguments": {}}),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn launch_refuses_a_missing_program_and_unknown_requests() {
    let directory = tempfile::tempdir().unwrap();
    let mut client = Client::start();
    let initialized = client.request("initialize", json!({"adapterID": "execute"}));
    assert_eq!(initialized["success"], true, "{initialized}");
    assert_eq!(
        initialized["body"]["supportsConfigurationDoneRequest"],
        true
    );
    client.event("initialized");

    let missing = directory.path().join("absent.program");
    let launch = client.request(
        "launch",
        json!({"program": missing, "cwd": directory.path()}),
    );
    assert_eq!(launch["success"], false, "{launch}");
    assert!(launch["message"]
        .as_str()
        .unwrap()
        .contains("does not exist"));

    let unknown = client.request("evaluate", json!({}));
    assert_eq!(unknown["success"], false, "{unknown}");
}

#[test]
fn scripted_session_pauses_inspects_steps_continues_and_terminates() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("tool.program");
    std::fs::write(
        &program,
        "for each item in [\"a\", \"b\"]\n  run printf with message item\n",
    )
    .unwrap();
    let mut client = Client::start();

    let initialized = client.request("initialize", json!({"adapterID": "execute"}));
    assert_eq!(initialized["success"], true, "{initialized}");
    client.event("initialized");

    let launched = client.request(
        "launch",
        json!({"program": program, "cwd": directory.path()}),
    );
    assert_eq!(launched["success"], true, "{launched}");

    let breakpoints = client.request(
        "setBreakpoints",
        json!({"source": {"path": program}, "breakpoints": [{"line": 2}]}),
    );
    assert_eq!(
        breakpoints["body"]["breakpoints"][0]["verified"], true,
        "{breakpoints}"
    );

    let configured = client.request("configurationDone", json!({}));
    assert_eq!(configured["success"], true, "{configured}");

    // First iteration: the breakpoint on the body line.
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "breakpoint", "{stopped}");
    assert_eq!(stopped["body"]["threadId"], 1);

    let threads = client.request("threads", json!({}));
    assert_eq!(threads["body"]["threads"][0]["id"], 1, "{threads}");

    let stack = client.request("stackTrace", json!({"threadId": 1}));
    assert_eq!(stack["body"]["stackFrames"][0]["line"], 2, "{stack}");

    let scopes = client.request("scopes", json!({"frameId": 1}));
    let reference = scopes["body"]["scopes"][0]["variablesReference"].clone();
    let variables = client.request("variables", json!({"variablesReference": reference}));
    assert_eq!(
        variables["body"]["variables"][0]["name"], "item",
        "{variables}"
    );
    assert_eq!(
        variables["body"]["variables"][0]["value"], "a",
        "{variables}"
    );

    // Continue to the breakpoint again, on the second iteration.
    let continued = client.request("continue", json!({"threadId": 1}));
    assert_eq!(continued["success"], true, "{continued}");
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "breakpoint", "{stopped}");
    let variables = client.request("variables", json!({"variablesReference": reference}));
    assert_eq!(
        variables["body"]["variables"][0]["value"], "b",
        "{variables}"
    );

    // Step once: the body statement runs and the loop header is next.
    let stepped = client.request("next", json!({"threadId": 1}));
    assert_eq!(stepped["success"], true, "{stepped}");
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "step", "{stopped}");

    let continued = client.request("continue", json!({"threadId": 1}));
    assert_eq!(continued["success"], true, "{continued}");
    let exited = client.event("exited");
    assert_eq!(exited["body"]["exitCode"], 0, "{exited}");
    client.event("terminated");

    // Plain run prints the loop's final value, so the output is `b`.
    assert_eq!(client.output(), "b\n");

    let disconnected = client.request("disconnect", json!({}));
    assert_eq!(disconnected["success"], true, "{disconnected}");
    let status = wait_for_exit(&mut client.child, Duration::from_secs(10));
    assert!(status.success());
}

fn wait_for_exit(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "adapter did not exit");
        thread::sleep(Duration::from_millis(20));
    }
}
