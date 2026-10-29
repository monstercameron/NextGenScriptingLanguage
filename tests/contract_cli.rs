use serde_json::{json, Value};
use std::fs;
#[cfg(all(unix, feature = "live"))]
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
#[cfg(all(unix, feature = "live"))]
use std::process::Stdio;
use std::process::{Command, Output};

fn cli(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .current_dir(root)
        .output()
        .expect("start execute")
}

#[cfg(unix)]
fn write_manifest(directory: &Path, name: &str, binary: &str) {
    let value = json!({
        "name": name,
        "binary": binary,
        "parameters": {"path": {"type": "string", "required": true}},
        "arguments": [{"parameter": "path"}],
        "contract": {"version": 1}
    });
    fs::write(
        directory.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn decoded(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[cfg(windows)]
fn set_private_windows_test_acl(path: &Path) {
    use std::os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
            },
            GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION,
            OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY,
            TOKEN_USER,
        },
        Storage::FileSystem::{READ_CONTROL, WRITE_DAC, WRITE_OWNER},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC | WRITE_OWNER)
        .open(path)
        .unwrap();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token is a writable out-slot.
    let process = unsafe { GetCurrentProcess() };
    let mut raw_token = std::ptr::null_mut();
    // SAFETY: process is the valid pseudo-handle above; raw_token is a writable out-slot.
    let status = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw_token) };
    assert_ne!(status, 0);
    // SAFETY: OpenProcessToken succeeded and returned a new handle owned only by this scope.
    let token = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw_token) };
    let mut token_bytes = 0;
    // SAFETY: a null buffer and zero length are the documented TokenUser size query.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut token_bytes,
        );
    }
    assert_ne!(token_bytes, 0);
    let mut token_data =
        vec![0usize; (token_bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: token_data is aligned and has at least token_bytes writable bytes; token stays live.
    let status = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            token_data.as_mut_ptr().cast(),
            token_bytes,
            &mut token_bytes,
        )
    };
    assert_ne!(status, 0);
    // SAFETY: GetTokenInformation wrote a TOKEN_USER into the aligned buffer above.
    let user = unsafe { &*(token_data.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid_text = std::ptr::null_mut();
    // SAFETY: user SID points inside token_data and sid_text is a writable out-slot.
    let status = unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_text) };
    assert_ne!(status, 0);
    let mut sid_length = 0;
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated LocalAlloc string.
    while unsafe { *sid_text.add(sid_length) } != 0 {
        sid_length += 1;
    }
    let sid_units = {
        // SAFETY: the counted UTF-16 units are within the allocation before its terminator.
        unsafe { std::slice::from_raw_parts(sid_text, sid_length) }
    };
    let sid = String::from_utf16(sid_units).unwrap();
    // SAFETY: sid_text was allocated by ConvertSidToStringSidW with LocalAlloc.
    unsafe { LocalFree(sid_text.cast()) };

    let sddl_text = format!("O:{sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid})");
    let sddl = std::ffi::OsStr::new(&sddl_text)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: sddl is live and NUL-terminated, descriptor is writable, and the optional
    // descriptor-size output may be null for this API.
    let status = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(status, 0);
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    let status = {
        // SAFETY: descriptor is live; present, dacl, and defaulted are writable out-slots.
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
    };
    assert_ne!(status, 0);
    assert_ne!(present, 0);
    assert!(!dacl.is_null());
    // SAFETY: file is live, SID is inside token_data, dacl belongs to descriptor, and all
    // out/in parameters are valid. SetSecurityInfo copies these values before returning.
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            user.User.Sid,
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: descriptor came from ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(descriptor.cast()) };
    assert_eq!(status, 0);
}

#[cfg(any(unix, windows))]
fn set_private_policy_fixture_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(windows)]
    set_private_windows_test_acl(path);
}

#[cfg(all(unix, feature = "live"))]
fn live_request(socket: &Path, request: Value) -> Value {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket).unwrap();
    writeln!(stream, "{}", request).unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

#[cfg(all(unix, feature = "live"))]
struct ChildCleanup(std::process::Child);

#[cfg(all(unix, feature = "live"))]
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn applet_inspection_projects_typed_contract_and_keeps_legacy_fields() {
    let root = tempfile::tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    let manifest = json!({
        "name": "fixture.contract",
        "phrases": ["contract fixture"],
        "binary": "fixture-command-that-is-not-run",
        "parameters": {
            "path": {"type": "string", "required": true}
        },
        "arguments": [{"parameter": "path"}],
        "stdio": {"stdout": "capture", "stderr": "inherit"},
        "contract": {
            "version": 1,
            "backend": "command",
            "output": {"type": "text", "encoding": "raw", "trim_final_lf": true},
            "outcomes": [
                {"status": 0, "name": "ok", "kind": "success"},
                {"status": 1, "name": "missing", "kind": "absence"}
            ],
            "capabilities": ["filesystem.read"],
            "effects": ["filesystem.read"]
        }
    });
    fs::write(
        manifests.join("fixture.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let output = cli(
        root.path(),
        &[
            "applets",
            "inspect",
            "fixture.contract",
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = decoded(&output);
    let contract = &report["applet"];
    assert_eq!(report["protocol"], "execute.applet/1");
    assert_eq!(contract["contract_version"], 1);
    assert_eq!(contract["backend"], "native_process");
    assert_eq!(
        contract["parameters_schema"]["properties"]["path"]["type"],
        "string"
    );
    assert_eq!(contract["argv_template"][0]["parameter"], "path");
    assert_eq!(contract["io"]["output_schema"]["type"], "string");
    assert_eq!(contract["io"]["output_schema"]["encoding"], "raw");
    assert_eq!(contract["io"]["output_schema"]["trim_final_lf"], true);
    assert_eq!(contract["io"]["stdio"]["stdout"], "capture");
    assert_eq!(contract["execution"]["contract_backend"], "command");
    assert_eq!(
        contract["execution"]["capabilities"],
        json!(["filesystem.read"])
    );
    assert_eq!(contract["execution"]["effects"], json!(["filesystem.read"]));
    assert_eq!(contract["execution"]["outcomes"][1]["name"], "missing");
    assert_eq!(contract["execution"]["lifecycle"]["kind"], "oneshot");
    assert_eq!(contract["execution"]["result_envelope"], false);
    // Existing discovery fields are kept alongside the versioned projection.
    assert_eq!(contract["name"], "fixture.contract");
    assert_eq!(contract["execution"]["shell_expansion"], false);
    assert_eq!(contract["execution"]["availability"], "not_checked");
}

#[cfg(unix)]
#[test]
fn strict_policy_preflights_every_invocation_before_the_first_command_runs() {
    let root = tempfile::tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    write_manifest(&manifests, "before", "touch");
    write_manifest(&manifests, "denied", "touch");
    let marker = root.path().join("must-not-exist");
    let later = root.path().join("also-not-exist");
    let program = root.path().join("main.program");
    fs::write(
        &program,
        format!(
            "run before with path {:?}\nrun denied with path {:?}\n",
            marker.to_string_lossy(),
            later.to_string_lossy()
        ),
    )
    .unwrap();
    let policy = root.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"version":1,"applets":{"before":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();
    set_private_policy_fixture_permissions(&policy);

    let output = cli(
        root.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no allowlist entry"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "preflight must precede every native effect"
    );
    assert!(!later.exists());
}

#[cfg(all(unix, feature = "live"))]
#[test]
fn live_policy_rejects_a_source_patch_before_committing_it() {
    let root = tempfile::tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    write_manifest(&manifests, "before", "touch");
    write_manifest(&manifests, "denied", "touch");
    let policy = root.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"version":1,"applets":{"before":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();
    set_private_policy_fixture_permissions(&policy);
    let program = root.path().join("live.program");
    let source = "run before with path \"never-created\"\n";
    fs::write(&program, source).unwrap();
    let socket = root.path().join("control.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "serve",
            program.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
            "--paused",
        ])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert!(serde_json::from_str::<Value>(&ready).unwrap()["event"] == "ready");

    let replacement = format!(
        "run before with path \"never-created\"\nrun denied with path {:?}\n",
        root.path().join("denied-effect").to_string_lossy()
    );
    let rejected = live_request(
        &socket,
        json!({
            "op": "patch",
            "operations": [{"op": "replace", "path": "/source", "value": replacement}]
        }),
    );
    assert_eq!(rejected["ok"], false);
    assert!(rejected["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no allowlist entry"));
    let state = live_request(&socket, json!({"op":"read"}));
    assert_eq!(state["result"]["state"]["source"], source);

    let stopped = live_request(&socket, json!({"op":"control","action":"stop"}));
    assert_eq!(stopped["ok"], true);
    assert!(child.0.wait().unwrap().success());
}

#[cfg(all(unix, feature = "live"))]
fn private_token_text(path: &Path) -> String {
    fs::read_to_string(path)
        .expect("read private token file")
        .trim_end()
        .to_owned()
}

#[cfg(all(unix, feature = "live"))]
#[test]
fn live_agent_policy_requires_credentials_and_never_returns_them() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let program = root.path().join("live.program");
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    fs::write(
        manifests.join("sleep.json"),
        serde_json::to_vec(&json!({
            "name":"sleep",
            "binary":"/bin/sleep",
            "parameters":{"seconds":{"type":"string","required":true}},
            "arguments":[{"parameter":"seconds"}],
            "contract":{"version":1}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&program, "run sleep with seconds \"3\"\n").unwrap();
    let token_file = root.path().join("reader.token");
    let reader_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "reader",
            "--token-file",
            token_file.to_str().unwrap(),
        ],
    );
    assert!(reader_created.status.success());
    let reader_created = decoded(&reader_created);
    // The new token-create contract prints metadata only: no `token` member.
    assert!(reader_created.get("token").is_none());
    let digest = reader_created["token_sha256"].as_str().unwrap().to_owned();
    // The token value reaches the test through the new private channel: the
    // 0600 file, never stdout. Boolean assertions below echo no values, so a
    // failure cannot leak the token into test output.
    let token = private_token_text(&token_file);
    assert_eq!(token.len(), 64);
    assert_eq!(
        fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let operator_file = root.path().join("operator.token");
    let operator_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "operator",
            "--token-file",
            operator_file.to_str().unwrap(),
        ],
    );
    assert!(operator_created.status.success());
    let operator_created = decoded(&operator_created);
    assert!(operator_created.get("token").is_none());
    let operator_token = private_token_text(&operator_file);
    let operator_hash = operator_created["token_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let editor_file = root.path().join("editor.token");
    let editor_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "editor",
            "--token-file",
            editor_file.to_str().unwrap(),
        ],
    );
    assert!(editor_created.status.success());
    let editor_hash = decoded(&editor_created)["token_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let editor_token = private_token_text(&editor_file);
    let policy_file = root.path().join("agents.json");
    fs::write(
        &policy_file,
        serde_json::to_vec(&json!({
            "version": 1,
            "agents": {
                "reader": {"token_sha256": digest, "permissions": ["read"]},
                "editor": {"token_sha256": editor_hash, "permissions": ["edit"]},
                "operator": {"token_sha256": operator_hash, "permissions": ["stop", "edit", "input", "control"]}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    set_private_policy_fixture_permissions(&policy_file);
    let socket = root.path().join("control.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "serve",
            program.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--agent-policy",
            policy_file.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--paused",
        ])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"],
        "ready",
        "unexpected live serve response: {ready}"
    );

    let denied = live_request(&socket, json!({"op":"read"}));
    assert_eq!(denied["ok"], false);
    let allowed = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "read",
            "--socket",
            socket.to_str().unwrap(),
            "--agent",
            "reader",
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(!String::from_utf8_lossy(&allowed.stdout).contains(token.as_str()));

    // A paused session is idle at a safe boundary. An edit-only agent may
    // change ordinary metadata, but a mixed patch touching /inbox needs both
    // grants and is rejected before its ordinary edit can commit.
    let ordinary_edit = live_request(
        &socket,
        json!({"op":"create","path":"/user/note","value":"still editable","auth":{"agent":"editor","token":editor_token}}),
    );
    assert_eq!(ordinary_edit["ok"], true);
    let denied_input_patch = live_request(
        &socket,
        json!({
            "op":"patch",
            "operations":[
                {"op":"add","path":"/user/uncommitted","value":true},
                {"op":"add","path":"/inbox/-","value":{"text":"queued"}}
            ],
            "auth":{"agent":"editor","token":editor_token}
        }),
    );
    assert_eq!(denied_input_patch["ok"], false);
    let after_denial = live_request(
        &socket,
        json!({"op":"read","auth":{"agent":"reader","token":token}}),
    );
    assert_eq!(
        after_denial["result"]["state"]["user"].get("uncommitted"),
        None
    );
    assert_eq!(after_denial["result"]["state"]["inbox"], json!([]));
    let revision = after_denial["revision"].as_u64().unwrap();
    let stale_input_patch = live_request(
        &socket,
        json!({
            "op":"patch",
            "if_revision":revision.saturating_sub(1),
            "operations":[{"op":"add","path":"/inbox/-","value":{"text":"stale"}}],
            "auth":{"agent":"operator","token":operator_token}
        }),
    );
    assert_eq!(stale_input_patch["ok"], false);
    assert_eq!(stale_input_patch["error"]["code"], "conflict");
    let after_stale = live_request(
        &socket,
        json!({"op":"read","auth":{"agent":"reader","token":token}}),
    );
    assert_eq!(after_stale["result"]["state"]["inbox"], json!([]));
    let authorized_input_patch = live_request(
        &socket,
        json!({
            "op":"patch",
            "if_revision":revision,
            "operations":[
                {"op":"add","path":"/user/committed","value":true},
                {"op":"add","path":"/inbox/-","value":{"text":"queued"}}
            ],
            "auth":{"agent":"operator","token":operator_token}
        }),
    );
    assert_eq!(authorized_input_patch["ok"], true);
    let after_authorized = live_request(
        &socket,
        json!({"op":"read","auth":{"agent":"reader","token":token}}),
    );
    assert_eq!(
        after_authorized["result"]["state"]["user"]["committed"],
        true
    );
    assert_eq!(
        after_authorized["result"]["state"]["inbox"][0]["text"],
        "queued"
    );

    let resumed = live_request(
        &socket,
        json!({"op":"control","action":"resume","auth":{"agent":"operator","token":operator_token}}),
    );
    assert_eq!(resumed["ok"], true);
    let mut active = false;
    for _ in 0..100 {
        let snapshot = live_request(
            &socket,
            json!({"op":"read","auth":{"agent":"reader","token":token}}),
        );
        if snapshot["result"]["runtime"]["status"] == "running" {
            active = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(active, "sleep applet did not enter the active state");
    let active_denial = live_request(
        &socket,
        json!({
            "op":"patch",
            "operations":[{"op":"add","path":"/inbox/-","value":{"text":"active"}}],
            "auth":{"agent":"editor","token":editor_token}
        }),
    );
    assert_eq!(active_denial["ok"], false);

    let stopped = live_request(
        &socket,
        json!({"op":"control","action":"stop","auth":{"agent":"operator","token":operator_token}}),
    );
    assert_eq!(stopped["ok"], true);
    assert!(child.0.wait().unwrap().success());
}

#[cfg(all(unix, feature = "live"))]
#[test]
fn live_expired_agent_tokens_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("live.program");
    fs::write(&program, "run printf with message \"ready\"\n").unwrap();

    // `--expires-in` reports the policy `expires_at` as metadata without
    // ever printing the token itself.
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let fresh_file = root.path().join("fresh.token");
    let fresh_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "fresh",
            "--token-file",
            fresh_file.to_str().unwrap(),
            "--expires-in",
            "3600",
        ],
    );
    assert!(fresh_created.status.success());
    let fresh_created = decoded(&fresh_created);
    assert!(fresh_created.get("token").is_none());
    let fresh_expires = fresh_created["expires_at"].as_u64().unwrap();
    assert!(fresh_expires >= before + 3590 && fresh_expires <= before + 3700);

    let stale_file = root.path().join("stale.token");
    let stale_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "stale",
            "--token-file",
            stale_file.to_str().unwrap(),
        ],
    );
    assert!(stale_created.status.success());
    let stale_created = decoded(&stale_created);
    assert!(stale_created.get("token").is_none());
    let stale_digest = stale_created["token_sha256"].as_str().unwrap().to_owned();

    let policy_file = root.path().join("agents.json");
    fs::write(
        &policy_file,
        serde_json::to_vec(&json!({
            "version": 1,
            "agents": {
                "fresh": {
                    "token_sha256": fresh_created["token_sha256"],
                    "permissions": ["read"],
                    "expires_at": fresh_expires,
                },
                "stale": {
                    "token_sha256": stale_digest,
                    "permissions": ["read"],
                    "expires_at": 1,
                },
                "operator": {
                    "token_sha256": fresh_created["token_sha256"],
                    "permissions": ["stop"],
                    "expires_at": fresh_expires,
                },
            }
        }))
        .unwrap(),
    )
    .unwrap();
    set_private_policy_fixture_permissions(&policy_file);
    let socket = root.path().join("control.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "serve",
            program.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--agent-policy",
            policy_file.to_str().unwrap(),
            "--paused",
        ])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"],
        "ready"
    );

    // The unexpired grant authorizes; the long-expired grant denies even
    // though its token and permission are otherwise valid.
    let fresh = private_token_text(&fresh_file);
    let fresh_read = live_request(
        &socket,
        json!({"op":"read","auth":{"agent":"fresh","token":fresh}}),
    );
    assert_eq!(fresh_read["ok"], true);
    let stale = private_token_text(&stale_file);
    let stale_read = live_request(
        &socket,
        json!({"op":"read","auth":{"agent":"stale","token":stale}}),
    );
    assert_eq!(stale_read["ok"], false);

    let stopped = live_request(
        &socket,
        json!({"op":"control","action":"stop","auth":{"agent":"operator","token":fresh}}),
    );
    assert_eq!(stopped["ok"], true);
    assert!(child.0.wait().unwrap().success());
}

#[cfg(all(unix, feature = "live"))]
#[test]
fn live_reload_agents_revokes_tokens_without_restart() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("live.program");
    fs::write(&program, "run printf with message \"ready\"\n").unwrap();
    let reader_arg = root
        .path()
        .join("reader.token")
        .to_str()
        .unwrap()
        .to_owned();
    let admin_arg = root.path().join("admin.token").to_str().unwrap().to_owned();
    let reader_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "reader",
            "--token-file",
            reader_arg.as_str(),
        ],
    );
    assert!(reader_created.status.success());
    let reader_created = decoded(&reader_created);
    assert!(reader_created.get("token").is_none());
    let admin_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "admin",
            "--token-file",
            admin_arg.as_str(),
        ],
    );
    assert!(admin_created.status.success());
    let admin_created = decoded(&admin_created);
    assert!(admin_created.get("token").is_none());
    // The policy stores hashes only, so digests are safe to compare: they are
    // already printed by token-create itself.
    let reader_digest = reader_created["token_sha256"].as_str().unwrap();
    let admin_digest = admin_created["token_sha256"].as_str().unwrap();
    let policy_file = root.path().join("agents.json");
    let write_policy = |agents: &Value| {
        fs::write(&policy_file, serde_json::to_vec(agents).unwrap()).unwrap();
        set_private_policy_fixture_permissions(&policy_file);
    };
    write_policy(&json!({
        "version": 1,
        "agents": {
            "reader": {"token_sha256": reader_digest, "permissions": ["read"]},
            "admin": {
                "token_sha256": admin_digest,
                "permissions": ["read", "control", "stop"],
            },
        }
    }));
    let socket = root.path().join("control.sock");
    let socket_arg = socket.to_str().unwrap().to_owned();
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "serve",
            program.to_str().unwrap(),
            "--socket",
            socket_arg.as_str(),
            "--agent-policy",
            policy_file.to_str().unwrap(),
            "--paused",
        ])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"],
        "ready"
    );

    // Both grants work before the revocation.
    let read_as = |agent: &str, file: &str| {
        cli(
            root.path(),
            &[
                "live",
                "read",
                "--socket",
                socket_arg.as_str(),
                "--agent",
                agent,
                "--token-file",
                file,
            ],
        )
    };
    let before = read_as("reader", reader_arg.as_str());
    assert!(before.status.success());
    let revision = decoded(&before)["revision"].as_u64().unwrap();

    // Revoke the reader in the policy file, then reload the running server
    // through the existing control channel. The reload itself is authorized
    // by the surviving admin grant.
    write_policy(&json!({
        "version": 1,
        "agents": {
            "admin": {
                "token_sha256": admin_digest,
                "permissions": ["read", "control", "stop"],
            },
        }
    }));
    let reloaded = cli(
        root.path(),
        &[
            "live",
            "control",
            "reload-agents",
            "--socket",
            socket_arg.as_str(),
            "--agent",
            "admin",
            "--token-file",
            admin_arg.as_str(),
        ],
    );
    assert!(reloaded.status.success());
    let reloaded = decoded(&reloaded);
    assert_eq!(reloaded["ok"], true);
    // A policy reload is not a document change: the revision does not move.
    assert_eq!(reloaded["revision"], revision);

    // The revoked token now fails on the same running server; the admin grant
    // still works, proving the server never restarted.
    let denied = read_as("reader", reader_arg.as_str());
    assert!(!denied.status.success());
    assert_eq!(decoded(&denied)["ok"], false);
    let admin_read = read_as("admin", admin_arg.as_str());
    assert!(admin_read.status.success());
    assert!(child.0.try_wait().unwrap().is_none());

    // A broken policy file fails the reload closed: the running policy stays.
    fs::write(&policy_file, b"{\"version\":1,\"agents\":{}}").unwrap();
    let refused = cli(
        root.path(),
        &[
            "live",
            "control",
            "reload-agents",
            "--socket",
            socket_arg.as_str(),
            "--agent",
            "admin",
            "--token-file",
            admin_arg.as_str(),
        ],
    );
    assert!(!refused.status.success());
    let admin_read = read_as("admin", admin_arg.as_str());
    assert!(admin_read.status.success());

    let stopped = live_request(
        &socket,
        json!({"op":"control","action":"stop","auth":{"agent":"admin","token":private_token_text(&root.path().join("admin.token"))}}),
    );
    assert_eq!(stopped["ok"], true);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn policy_input_is_bounded_and_check_json_reports_parse_failures() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("main.program");
    fs::write(&program, "run \"true\"\n").unwrap();
    let policy = root.path().join("policy.json");
    fs::write(&policy, "{\"version\":1,\"unexpected\":true}").unwrap();
    set_private_policy_fixture_permissions(&policy);

    let invalid = cli(
        root.path(),
        &[
            "check",
            program.to_str().unwrap(),
            "--json",
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert!(!invalid.status.success());
    let report = decoded(&invalid);
    assert_eq!(report["protocol"], "execute.check/1");
    assert_eq!(report["ok"], false);
    assert!(report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| {
            diagnostic["code"] == "check_failed"
                && diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("capability policy"))
        }));

    // Keep this aligned with CapabilityPolicy's 64 KiB file bound. The constant is
    // intentionally private, so exercise acceptance at the boundary and rejection
    // immediately beyond it through the public loader and CLI.
    const POLICY_LIMIT_BYTES: usize = 64 * 1024;
    let mut at_limit = br#"{"version":1,"applets":{}}"#.to_vec();
    at_limit.resize(POLICY_LIMIT_BYTES, b' ');
    fs::write(&policy, at_limit).unwrap();
    flow::capabilities::CapabilityPolicy::load(&policy)
        .expect("a valid policy exactly at the byte limit must be accepted");

    fs::write(&policy, vec![b' '; POLICY_LIMIT_BYTES + 1]).unwrap();
    let oversized = cli(
        root.path(),
        &[
            "inspect",
            program.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert!(!oversized.status.success());
    let oversized_stderr = String::from_utf8_lossy(&oversized.stderr);
    assert!(
        oversized_stderr.contains("capability policy exceeds the size limit"),
        "expected the bounded-size diagnostic, got: {oversized_stderr}"
    );

    fs::write(&policy, r#"{"version":1,"applets":{}}"#).unwrap();
    let warning = cli(
        root.path(),
        &[
            "check",
            program.to_str().unwrap(),
            "--json",
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    let report = decoded(&warning);
    assert!(report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| {
            item["code"] == "data_flow_policy_not_configured"
                && item["message"]
                    .as_str()
                    .unwrap()
                    .contains("remote egress and persistence")
        }));
}

#[test]
fn help_lists_the_opt_in_policy_and_contract_inspection_details() {
    let root = tempfile::tempdir().unwrap();
    let output = cli(root.path(), &["--help"]);
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--capability-policy FILE"));
    // The data-flow policy is listed as its own option; the security-scope warning is
    // printed when a capability policy is used without it.
    assert!(help.contains("--data-flow-policy FILE"));
    assert!(help.contains("--total-timeout-ms N"));

    let applets = cli(root.path(), &["applets", "--help"]);
    let help = String::from_utf8_lossy(&applets.stdout);
    assert!(help.contains("typed I/O, outcomes, effects, backends, and lifecycle"));
}

#[cfg(unix)]
#[test]
fn total_timeout_is_enforced_for_finite_run() {
    let root = tempfile::tempdir().unwrap();
    let workflow = root.path().join("slow.program");
    fs::write(&workflow, "run \"sleep\" with arguments [\"1\"]\n").unwrap();
    let output = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--total-timeout-ms",
            "40",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "tooling")]
#[test]
fn applet_discovery_and_typed_matching_are_machine_readable_without_execution() {
    let root = tempfile::tempdir().unwrap();
    let discovered = cli(root.path(), &["applets", "discover"]);
    assert!(
        discovered.status.success(),
        "{}",
        String::from_utf8_lossy(&discovered.stderr)
    );
    assert_eq!(
        decoded(&discovered)["protocol"],
        "execute.applets-discovery/1"
    );

    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    for (name, ports) in [
        (
            "producer",
            json!({"outputs":{"value":{"schema":{"type":"text"},"delivery":"single"}}}),
        ),
        (
            "consumer",
            json!({"inputs":{"value":{"schema":{"type":"text"},"delivery":"single"}}}),
        ),
    ] {
        fs::write(
            manifests.join(format!("{name}.json")),
            serde_json::to_vec(&json!({
                "name": name,
                "binary": "must-not-be-executed",
                "parameters": if name == "consumer" {
                    json!({"value":{"type":"string","required":true}})
                } else {
                    json!({})
                },
                "arguments": if name == "consumer" {
                    json!([{"parameter":"value"}])
                } else {
                    json!([])
                },
                "contract": {
                    "version": 1,
                    "output": {"type":"text","encoding":"raw"},
                    "ports": ports
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let request = json!({
        "source": {"applet":"producer", "port":"value"},
        "target": {"applet":"consumer", "port":"value"}
    });
    let matched = cli(
        root.path(),
        &[
            "applets",
            "match",
            "--json",
            &request.to_string(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert!(
        matched.status.success(),
        "{}",
        String::from_utf8_lossy(&matched.stderr)
    );
    let report = decoded(&matched);
    assert_eq!(report["protocol"], "execute.applet-matches/1");
    assert_eq!(report["matches"].as_array().unwrap().len(), 1);
    assert_eq!(report["matches"][0]["compatible"], true);
}

#[test]
fn hidden_remote_resolver_emits_only_numeric_socket_addresses() {
    let root = tempfile::tempdir().unwrap();
    let resolved = cli(root.path(), &["__remote_resolve", "127.0.0.1", "443"]);
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let output = String::from_utf8(resolved.stdout).unwrap();
    let addresses = output.lines().collect::<Vec<_>>();
    assert_eq!(addresses, vec!["127.0.0.1:443"]);
    for address in addresses {
        assert_eq!(address.parse::<std::net::SocketAddr>().unwrap().port(), 443);
    }
}

#[cfg(any(feature = "module-tools", feature = "tooling"))]
#[test]
fn modules_verify_and_service_generate_are_explicit_read_or_render_operations() {
    let root = tempfile::tempdir().unwrap();
    #[cfg(feature = "module-tools")]
    {
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/modules");
        let package = examples.join("package");
        let manifest = package.join("program.modules.json");
        let lock = package.join("program.modules.lock.json");
        let verify = cli(
            root.path(),
            &[
                "modules",
                "verify",
                manifest.to_str().unwrap(),
                "--root",
                examples.to_str().unwrap(),
                "--lock",
                lock.to_str().unwrap(),
            ],
        );
        assert!(
            verify.status.success(),
            "{}",
            String::from_utf8_lossy(&verify.stderr)
        );
        assert_eq!(decoded(&verify)["validation"], "package_and_lock");
    }

    #[cfg(feature = "tooling")]
    {
        let workflow = root.path().join("scheduled.program");
        fs::write(
            &workflow,
            "every 1 hour\n  run \"printf\" with arguments [\"tick\"]\n",
        )
        .unwrap();
        let executable = env!("CARGO_BIN_EXE_execute");
        let rendered = cli(
            root.path(),
            &[
                "service",
                "generate",
                "systemd",
                workflow.to_str().unwrap(),
                "--working-dir",
                root.path().to_str().unwrap(),
            ],
        );
        assert!(
            rendered.status.success(),
            "{}",
            String::from_utf8_lossy(&rendered.stderr)
        );
        let rendered = String::from_utf8(rendered.stdout).unwrap();
        assert!(rendered.contains("[Service]"));
        if cfg!(unix) {
            // The generated unit escapes the Windows path, so the literal path check is Unix-only.
            assert!(rendered.contains(executable));
        }
        assert!(rendered.contains("scheduled.program"));
        assert!(!root.path().join("scheduled.service").exists());
    }
}

#[test]
fn schedule_flags_are_bounded_and_rejected_for_finite_workflows() {
    let root = tempfile::tempdir().unwrap();
    let workflow = root.path().join("finite.program");
    fs::write(&workflow, "run printf with message \"unused\"\n").unwrap();
    let finite = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--missed-ticks",
            "coalesce",
        ],
    );
    assert!(!finite.status.success());
    assert!(String::from_utf8_lossy(&finite.stderr).contains("only to triggered"));
    let unbounded = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--missed-ticks",
            "catch-up",
        ],
    );
    assert!(!unbounded.status.success());
    assert!(String::from_utf8_lossy(&unbounded.stderr).contains("requires --max-catch-up"));
    let too_many = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--missed-ticks",
            "catch-up",
            "--max-catch-up",
            "65",
        ],
    );
    assert!(!too_many.status.success());
    assert!(String::from_utf8_lossy(&too_many.stderr).contains("between 1 and 64"));
}
