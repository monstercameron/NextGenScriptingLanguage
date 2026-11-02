//! Mandatory OS-sandbox denial coverage for issue #457.
//!
//! The library tests in `platform::sandbox` self-skip when the OS facility is
//! absent, so these end-to-end tests prove the untrusted-mode guarantee
//! through the real CLI: representative `.program` files run under
//! `--execution-mode untrusted` must have an ungranted write and an ungranted
//! network connection denied, and Windows must refuse the native launch.
//!
//! A skipped availability probe is not acceptance. When
//! `EXECUTE_REQUIRE_OS_SANDBOX` is set (the sandbox-denial CI lane), an
//! unavailable facility fails these tests instead of skipping them. Local
//! runs without the facility keep the historical skip behavior.

#[path = "support/private_answers.rs"]
mod private_answers;

use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::PathBuf;
use std::process::Command;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Output;

/// Set by the sandbox-denial CI lane. A skip is a failure there.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sandbox_denial_is_required() -> bool {
    std::env::var_os("EXECUTE_REQUIRE_OS_SANDBOX").is_some()
}

/// Inner sandbox-facility errors. A run that fails with one of these never
/// established the sandbox, so it proves nothing about denials.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn facility_unavailable(stderr: &str) -> bool {
    stderr.contains("could not establish the required namespaces")
        || stderr.contains("trusted root-owned bubblewrap")
        || stderr.contains("macOS sandbox-exec is unavailable")
        || stderr.contains("macOS sandbox-exec is not an executable file")
        || stderr.contains("macOS sandbox-exec could not start")
        || stderr.contains("required applet sandbox is unsupported on this platform")
}

/// Returns true when the run failed only because the OS facility is absent.
/// Any other failure (including a sandbox profile bug in the test itself)
/// falls through to the denial assertions, which then fail loudly.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unavailable_or_fail_closed(label: &str, output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !facility_unavailable(&stderr) {
        return false;
    }
    assert!(
        !output.status.success(),
        "{label}: sandbox unavailable, the run must fail closed: {stderr}"
    );
    if sandbox_denial_is_required() {
        panic!(
            "{label}: OS sandbox facility is unavailable, but EXECUTE_REQUIRE_OS_SANDBOX is set: {stderr}"
        );
    }
    eprintln!("skipping {label}: OS sandbox facility unavailable");
    true
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn python_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct UnixLayout {
    work: tempfile::TempDir,
    manifests: PathBuf,
    policy: PathBuf,
    program: PathBuf,
    verdict_dir: PathBuf,
}

/// Working directory granted read, `verdict` subdirectory granted write, and
/// a shared capability policy for a `gated` applet. Each test writes its own
/// manifest pointing at its fixture binary.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unix_dirs() -> UnixLayout {
    let work = tempfile::tempdir().unwrap();
    let manifests = work.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    let verdict_dir = work.path().join("verdict");
    fs::create_dir(&verdict_dir).unwrap();
    let policy = work.path().join("policy.json");
    fs::write(
        &policy,
        serde_json::json!({
            "version": 1,
            "applets": {
                "gated": {
                    "capabilities": ["process.exec", "filesystem.read", "filesystem.write"],
                    "filesystem": {
                        "read": [work.path().to_string_lossy()],
                        "write": [verdict_dir.to_string_lossy()]
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    private_answers::make_private(&policy);
    let program = work.path().join("denial.program");
    fs::write(&program, "run gated\n").unwrap();
    UnixLayout {
        work,
        manifests,
        policy,
        program,
        verdict_dir,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_script_manifest(layout: &UnixLayout, binary: &Path) {
    fs::write(
        layout.manifests.join("gated.json"),
        serde_json::json!({
            "name": "gated",
            "binary": binary.to_string_lossy(),
            "arguments": [],
            "contract": {
                "version": 1,
                "capabilities": ["process.exec", "filesystem.read", "filesystem.write"],
                "effects": ["process.exec", "filesystem.read", "filesystem.write"]
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_untrusted(layout: &UnixLayout) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .env_remove("EXECUTE_DAEMON")
        .env_remove("EXECUTE_DAEMON_ACCEPT_CONTEXT")
        .current_dir(layout.work.path())
        .arg("run")
        .arg(&layout.program)
        .arg("--manifest-dir")
        .arg(&layout.manifests)
        .args(["--execution-mode", "untrusted", "--capability-policy"])
        .arg(&layout.policy)
        .output()
        .unwrap()
}

/// The verdict file proves the child actually ran inside the sandbox:
/// `DENIED` means the OS refused that specific access, `ALLOWED` means
/// confinement regressed, and a missing verdict without facility markers
/// means the test itself is broken.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_denied(label: &str, output: &Output, verdict: &Path) {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "{label}: ungranted access must fail the run: {stderr}"
    );
    let verdict = fs::read_to_string(verdict).unwrap_or_else(|error| {
        panic!("{label}: verdict file is missing: {error}; stderr: {stderr}")
    });
    assert_eq!(
        verdict.trim(),
        "DENIED",
        "{label}: sandbox allowed the ungranted access: {stderr}"
    );
}

/// The applet attempts an ungranted write while holding a granted verdict
/// directory. A confinement regression writes `ALLOWED` (and the run may
/// even succeed), so both the verdict and the run outcome are asserted.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn untrusted_mode_denies_an_ungranted_write() {
    let label = "untrusted write denial";
    let layout = unix_dirs();
    let secret_dir = layout.work.path().join("secret");
    fs::create_dir(&secret_dir).unwrap();
    let secret = secret_dir.join("ungranted-secret");
    fs::write(&secret, b"hidden").unwrap();
    let marker = secret_dir.join("forbidden");
    let verdict = layout.verdict_dir.join("write-verdict");
    let fixture = layout.work.path().join("deny-write.sh");
    write_executable(
        &fixture,
        &format!(
            "#!/bin/sh\nif touch {} 2>{}.err; then\n  echo ALLOWED > {}\n  exit 42\nelse\n  echo DENIED > {}\n  exit 43\nfi\n",
            shell_quote(&marker),
            shell_quote(&verdict),
            shell_quote(&verdict),
            shell_quote(&verdict),
        ),
    );
    write_script_manifest(&layout, &fixture);
    let output = run_untrusted(&layout);
    if unavailable_or_fail_closed(label, &output) {
        return;
    }
    assert_denied(label, &output, &verdict);
    assert!(!marker.exists(), "{label}: ungranted file was created");
    assert_eq!(
        fs::read(&secret).unwrap(),
        b"hidden",
        "{label}: ungranted secret was modified"
    );
    #[cfg(target_os = "macos")]
    {
        let child_stderr =
            fs::read_to_string(layout.verdict_dir.join("write-verdict.err")).unwrap_or_default();
        assert!(
            child_stderr.contains("Operation not permitted"),
            "{label}: expected an OS sandbox denial, got: {child_stderr}"
        );
    }
}

#[cfg(target_os = "linux")]
fn python3() -> Option<PathBuf> {
    // Fixed system locations, as in the library bubblewrap tests: the
    // interpreter must live under a path the sandbox bind-mounts read-only.
    [Path::new("/usr/bin/python3"), Path::new("/bin/python3")]
        .into_iter()
        .find(|path| path.is_file())
        .map(Path::to_path_buf)
}

/// The applet attempts a TCP connection to an owned loopback listener
/// without a network grant. Bubblewrap removes network access, so the
/// connect must fail and the listener must never see the peer.
#[cfg(target_os = "linux")]
#[test]
fn untrusted_mode_denies_an_ungranted_network_connection() {
    use std::net::TcpListener;

    let label = "untrusted network denial";
    let Some(python) = python3() else {
        if sandbox_denial_is_required() {
            panic!("{label}: Python 3 is required, but EXECUTE_REQUIRE_OS_SANDBOX is set");
        }
        eprintln!("skipping {label}: Python 3 is unavailable");
        return;
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let layout = unix_dirs();
    let verdict = layout.verdict_dir.join("network-verdict");
    let probe = layout.work.path().join("deny-connect.py");
    fs::write(
        &probe,
        format!(
            "import socket, sys\ns = socket.socket()\ns.settimeout(2)\ntry:\n    s.connect(('{}', {}))\nexcept Exception:\n    open({}, 'w').write('DENIED\\n')\n    sys.exit(43)\nopen({}, 'w').write('ALLOWED\\n')\nsys.exit(42)\n",
            address.ip(),
            address.port(),
            python_quote(&verdict),
            python_quote(&verdict),
        ),
    )
    .unwrap();
    fs::write(
        layout.manifests.join("gated.json"),
        serde_json::json!({
            "name": "gated",
            "binary": python.to_string_lossy(),
            "arguments": [{"literal": probe.to_string_lossy()}],
            "contract": {
                "version": 1,
                "capabilities": ["process.exec", "filesystem.read", "filesystem.write"],
                "effects": ["process.exec", "filesystem.read", "filesystem.write"]
            }
        })
        .to_string(),
    )
    .unwrap();
    let output = run_untrusted(&layout);
    if unavailable_or_fail_closed(label, &output) {
        return;
    }
    assert_denied(label, &output, &verdict);
    assert!(
        listener.accept().is_err(),
        "{label}: sandboxed process reached the owned loopback listener"
    );
}

/// The applet attempts a TCP connection with `nc` and no network grant. The
/// `sandbox-exec` profile denies the socket, and `nc` reports the refusal
/// itself, which proves the child ran under the sandbox.
#[cfg(target_os = "macos")]
#[test]
fn untrusted_mode_denies_an_ungranted_network_connection() {
    use std::net::{TcpListener, TcpStream};

    let label = "untrusted network denial";
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    // Baseline: the listener is reachable without the sandbox.
    TcpStream::connect(address).expect("owned loopback listener should accept a baseline");
    listener
        .accept()
        .expect("baseline TCP connection should arrive");
    let nc = PathBuf::from("/usr/bin/nc");
    assert!(nc.is_file(), "the macOS network fixture requires nc");
    let layout = unix_dirs();
    fs::write(
        layout.manifests.join("gated.json"),
        serde_json::json!({
            "name": "gated",
            "binary": nc.to_string_lossy(),
            "arguments": [
                {"literal": "-z"},
                {"literal": "-v"},
                {"literal": "-w"},
                {"literal": "2"},
                {"literal": address.ip().to_string()},
                {"literal": address.port().to_string()}
            ],
            "contract": {
                "version": 1,
                "capabilities": ["process.exec", "filesystem.read"],
                "effects": ["process.exec", "filesystem.read"]
            }
        })
        .to_string(),
    )
    .unwrap();
    let output = run_untrusted(&layout);
    if unavailable_or_fail_closed(label, &output) {
        return;
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "{label}: network was reachable without a grant: {stderr}"
    );
    assert!(
        stderr.contains("Operation not permitted"),
        "{label}: expected an OS sandbox network denial, got: {stderr}"
    );
    assert!(
        listener.accept().is_err(),
        "{label}: sandboxed applet reached the local listener"
    );
}

/// Windows has no OS sandbox backend: an untrusted native launch with a
/// policy must be refused before execution, never run unsandboxed.
#[cfg(windows)]
#[test]
fn untrusted_native_with_policy_is_refused_without_os_confinement() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("workflow.program");
    fs::write(&program, "run fixture.native\n").unwrap();
    let manifests = directory.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    fs::write(
        manifests.join("fixture.json"),
        serde_json::json!({
            "name": "fixture.native",
            "binary": env!("CARGO_BIN_EXE_execute"),
            "arguments": [],
            "contract": {"version": 1}
        })
        .to_string(),
    )
    .unwrap();
    let policy = directory.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"version":1,"applets":{"fixture.native":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();
    private_answers::make_private(&policy);
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "--execution-mode", "untrusted"])
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--capability-policy")
        .arg(&policy)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "untrusted native launch must be refused on Windows: {stderr}"
    );
    assert!(
        stderr.contains("required OS sandbox refused"),
        "expected a typed sandbox refusal, got: {stderr}"
    );
}
