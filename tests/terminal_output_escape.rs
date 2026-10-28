#![cfg(unix)]
//! Terminal safety for pipeline output (#522 part 1): a hostile model/fixture
//! response carrying OSC/CSI/C1 escapes and bidi overrides must not reach a
//! TTY raw, while pipes and files keep the exact bytes (H17 pipe guarantee).

use std::fs;
use std::io::Read;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// OSC title-set, CSI erase, C1 CSI, BEL and a bidi override, as a program
/// string literal: the parser decodes `\u{...}` into the real control bytes.
const HOSTILE_LITERAL: &str = "a\\u001b]0;pwned\\u0007now\\u001b[2Jthen\\u009b31mswap\\u202eend";

/// The exact stdout bytes a pipe must carry for the hostile program.
const HOSTILE_RAW: &[u8] = b"a\x1b]0;pwned\x07now\x1b[2Jthen\xc2\x9b31mswap\xe2\x80\xaeend\n";

/// The exact TTY rendering after `escape_controls` (plus the pty's own
/// `\n` -> `\r\n` translation, which tests strip before comparing).
const HOSTILE_ESCAPED: &str = "a\\u{1b}]0;pwned\\u{7}now\\u{1b}[2Jthen\\u{9b}31mswap\\u{202e}end\n";

fn hostile_program() -> String {
    format!("run printf with message \"{HOSTILE_LITERAL}\"\n")
}

fn run_piped(directory: &Path, manifests: &Path, script: &str) -> Vec<u8> {
    let script_path = directory.join("workflow.program");
    fs::write(&script_path, script).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(&script_path)
        .arg("--manifest-dir")
        .arg(manifests)
        .stdin(Stdio::null())
        .output()
        .expect("run execute with piped stdout");
    assert!(
        output.status.success(),
        "execute failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Run `execute` with stdout attached to a real pty slave (so
/// `stdout.is_terminal()` is true) and return the bytes read from the master.
fn run_on_pty(directory: &Path, manifests: &Path, script: &str) -> Vec<u8> {
    let script_path = directory.join("workflow.program");
    fs::write(&script_path, script).unwrap();
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: on success openpty stores two fresh, owned file descriptors in
    // `master`/`slave`; the null name/termp/winp arguments take no storage.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "openpty failed");
    // SAFETY: `slave` is a fresh owned fd from the successful openpty above.
    let child_slave = unsafe { libc::dup(slave) };
    assert!(child_slave >= 0, "dup of pty slave failed");
    // SAFETY: `slave` is still the owned fd from openpty; closing it here
    // leaves the child with the dup above, so master reads EOF at child exit.
    let closed = unsafe { libc::close(slave) };
    assert_eq!(closed, 0, "close of pty slave failed");
    // SAFETY: `master` is a fresh owned fd from openpty; ownership moves to
    // the reader thread below, which closes it when the file drops.
    let master_file = unsafe { std::fs::File::from_raw_fd(master) };
    // SAFETY: `child_slave` is a fresh owned fd from the successful dup;
    // ownership moves to the spawned child stdio.
    let child_stdio = unsafe { Stdio::from_raw_fd(child_slave) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(&script_path)
        .arg("--manifest-dir")
        .arg(manifests)
        .stdin(Stdio::null())
        .stdout(child_stdio)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn execute with pty stdout");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut master_file = master_file;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match master_file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                // Linux reports EIO on the master once the slave side closes.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("reading pty master failed: {error}"),
            }
        }
        let _ = sender.send(bytes);
    });
    let bytes = match receiver.recv_timeout(Duration::from_secs(60)) {
        Ok(bytes) => bytes,
        Err(_) => {
            let _ = child.kill();
            panic!("timed out reading execute output from pty");
        }
    };
    let output = child.wait_with_output().expect("wait for execute");
    assert!(
        output.status.success(),
        "execute on pty failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Strip the pty line discipline's own `\n` -> `\r\n` translation before
    // comparing against the expected rendering.
    bytes.into_iter().filter(|byte| *byte != b'\r').collect()
}

fn assert_terminal_neutral(rendered: &[u8], context: &str) {
    assert!(
        !rendered.contains(&0x1b),
        "raw ESC reached the terminal ({context}): {rendered:?}"
    );
    assert!(
        !rendered.contains(&0x9b),
        "raw C1 CSI reached the terminal ({context}): {rendered:?}"
    );
    let text = String::from_utf8(rendered.to_vec()).expect("TTY output is UTF-8");
    assert!(
        !text.contains('\u{202e}'),
        "raw bidi override reached the terminal ({context}): {text:?}"
    );
}

#[test]
fn pipe_keeps_hostile_pipeline_bytes_exact() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let stdout = run_piped(directory.path(), &manifests, &hostile_program());
    assert_eq!(stdout, HOSTILE_RAW, "H17 pipe guarantee broke");
}

#[test]
fn tty_neutralizes_hostile_pipeline_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let rendered = run_on_pty(directory.path(), &manifests, &hostile_program());
    assert_terminal_neutral(&rendered, "streamed printf");
    assert_eq!(
        rendered,
        HOSTILE_ESCAPED.as_bytes(),
        "TTY rendering must escape, never drop, content"
    );
}

#[test]
fn tty_final_write_is_escaped_while_files_stay_raw() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    // `save` consumes stdout, so the tail value reaches the final stdout
    // write instead of the streamed one; the file itself must stay raw.
    let script =
        format!("run printf with message \"{HOSTILE_LITERAL}\"\nthen save to \"kept.txt\"\n");
    let rendered = run_on_pty(directory.path(), &manifests, &script);
    assert_terminal_neutral(&rendered, "final write");
    assert_eq!(rendered, HOSTILE_ESCAPED.as_bytes());
    assert_eq!(
        fs::read(directory.path().join("kept.txt")).unwrap(),
        HOSTILE_RAW,
        "file sinks keep exact bytes even when stdout is a TTY"
    );
}

/// The acceptance object itself: an `llm.ask` fixture response carrying
/// OSC/CSI/bidi is neutralized on a TTY and byte-exact on a pipe.
#[test]
#[cfg(feature = "model")]
fn model_fixture_text_is_neutralized_on_tty_and_raw_on_pipe() {
    let hostile = "go\u{1b}]0;pwned\u{7}now\u{1b}[2Jthen\u{9b}31mswap\u{202e}end";
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    let model_manifest: serde_json::Value =
        serde_json::from_str(include_str!("../manifests/llm.ask.json")).unwrap();
    fs::write(
        manifests.join("llm.ask.json"),
        serde_json::to_vec(&model_manifest).unwrap(),
    )
    .unwrap();
    let printf_path = ["/usr/bin/printf", "/bin/printf"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .expect("Unix printf executable");
    let mut printf_manifest: serde_json::Value =
        serde_json::from_str(include_str!("../manifests/printf.json")).unwrap();
    printf_manifest["binary"] = serde_json::json!(printf_path);
    fs::write(
        manifests.join("printf.json"),
        serde_json::to_vec(&printf_manifest).unwrap(),
    )
    .unwrap();
    let fixture = directory.path().join("evil.json");
    fs::write(
        &fixture,
        serde_json::json!({
            "expected_request": {
                "provider": "openai",
                "kind": "generate",
                "model": "model-under-test",
                "state": "",
                "instruction": "draft text"
            },
            "response": {
                "model": "model-under-test",
                "status": "completed",
                "error": null,
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": hostile}]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
        .to_string(),
    )
    .unwrap();
    let script = format!(
        "llm.ask with model \"model-under-test\" and instruction \"draft text\" and fixture {} as raw\nthen parse json as result\ntake text at \"/text\" from result as answer\nrun printf with message answer\n",
        serde_json::to_string(&fixture.to_string_lossy()).unwrap()
    );
    let expected_pipe = format!("{hostile}\n").into_bytes();
    let piped = run_piped(directory.path(), &manifests, &script);
    assert_eq!(
        piped, expected_pipe,
        "pipes keep the fixture response byte-exact"
    );
    let rendered = run_on_pty(directory.path(), &manifests, &script);
    assert_terminal_neutral(&rendered, "model fixture response");
    assert_eq!(
        rendered,
        "go\\u{1b}]0;pwned\\u{7}now\\u{1b}[2Jthen\\u{9b}31mswap\\u{202e}end\n".as_bytes(),
        "TTY rendering must escape, never drop, the fixture text"
    );
}
