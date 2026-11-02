//! Untrusted-mode per-child resource bounds (#456).
//!
//! These tests run the real native spawn path (`runtime::run` through
//! `capabilities::command_for`) under a capability policy, so every assertion
//! exercises the installed `setrlimit` hook and the exit classification, not
//! a mock. They run on macOS and Linux; a test that needs the OS sandbox
//! facility skips cleanly when preflight reports it unavailable, and panics
//! on any other refusal so a broken profile cannot pass silently. No test
//! fork-bombs: the process-count bound is verified by reading back the
//! installed value, never by tripping it.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use flow::{
    capabilities::{CapabilityPolicy, ExecutionMode},
    manifest::Registry,
    parser, runtime,
};
use std::path::Path;

/// generous wall-clock budget: the resource kills under test fire in about a
/// second of child CPU time, well inside this budget.
fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        command_timeout: Some(std::time::Duration::from_secs(60)),
        ..runtime::RunOptions::new(cwd.to_path_buf())
    }
}

fn sh_manifest(capabilities: &[&str], effects: &[&str]) -> String {
    serde_json::json!({
        "name": "sh",
        "binary": "/bin/sh",
        "parameters": {"script": {"type": "string", "required": true}},
        "arguments": [{"literal": "-c"}, {"parameter": "script"}],
        "contract": {"version": 1, "capabilities": capabilities, "effects": effects},
    })
    .to_string()
}

fn sh_program(script: &str) -> flow::ast::Program {
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    parser::parse(&format!("run sh with script \"{escaped}\"\n")).unwrap()
}

fn untrusted_registry(manifest: &str, grant: serde_json::Value) -> Registry {
    let registry = Registry::from_sources(&[manifest.to_owned()]).unwrap();
    let policy = CapabilityPolicy::from_json(
        &serde_json::json!({"version": 1, "applets": {"sh": grant}}).to_string(),
    )
    .unwrap();
    registry.set_host_state(Some(policy));
    registry
}

fn trusted_registry(manifest: &str, grant: serde_json::Value) -> Registry {
    let registry = untrusted_registry(manifest, grant);
    registry.set_host_state(Some(ExecutionMode::Trusted));
    registry
}

/// The OS sandbox facility itself is missing or unusable on this host: skip,
/// so CI without bubblewrap or sandbox-exec stays green. Any other refusal
/// (a broken profile, a policy error) must panic in the caller instead.
fn sandbox_unavailable(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("is unavailable")
        || text.contains("required namespaces")
        || text.contains("root-owned bubblewrap")
        || text.contains("could not start")
}

/// Canonicalize so grant paths match the sandbox's resolved view
/// (`/var` versus `/private/var` on macOS).
fn workdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let canonical = directory.path().canonicalize().unwrap();
    (directory, canonical)
}

struct Outcome {
    kind: String,
    signal: Option<i64>,
    message: String,
    stages: Vec<std::collections::BTreeMap<String, flow::ast::Value>>,
}

fn text(value: Option<&flow::ast::Value>) -> String {
    match value {
        Some(flow::ast::Value::String(text)) => text.clone(),
        other => panic!("expected a text field, got {other:?}"),
    }
}

fn kind_of(error: &anyhow::Error) -> Outcome {
    let record = match runtime::execution_result::error_value(error) {
        flow::ast::Value::Record(fields) => fields,
        other => panic!("expected an execution result record, got {other:?}"),
    };
    let kind = text(record.get("kind"));
    let signal = match record.get("signal") {
        Some(flow::ast::Value::Integer(signal)) => Some(*signal),
        Some(flow::ast::Value::Null) | None => None,
        other => panic!("expected an integer signal, got {other:?}"),
    };
    let message = match record.get("message") {
        Some(flow::ast::Value::String(text)) => text.clone(),
        Some(flow::ast::Value::Null) | None => String::new(),
        other => panic!("expected a text message, got {other:?}"),
    };
    let stages = match record.get("stages") {
        Some(flow::ast::Value::List(stages)) => stages
            .iter()
            .map(|stage| match stage {
                flow::ast::Value::Record(fields) => fields.clone(),
                other => panic!("expected a stage record, got {other:?}"),
            })
            .collect(),
        other => panic!("expected a stage list, got {other:?}"),
    };
    Outcome {
        kind,
        signal,
        message,
        stages,
    }
}

fn stage_text(stage: &std::collections::BTreeMap<String, flow::ast::Value>, field: &str) -> String {
    text(stage.get(field))
}

fn stage_signal(stage: &std::collections::BTreeMap<String, flow::ast::Value>) -> Option<i64> {
    match stage.get("signal") {
        Some(flow::ast::Value::Integer(signal)) => Some(*signal),
        Some(flow::ast::Value::Null) | None => None,
        other => panic!("expected an integer stage signal, got {other:?}"),
    }
}

#[test]
fn untrusted_children_run_with_finite_default_limits() {
    let (_directory, cwd) = workdir();
    let registry = untrusted_registry(
        &sh_manifest(&["process.exec"], &["process.exec"]),
        serde_json::json!({
            "capabilities": ["process.exec"],
            "filesystem": {"read": [], "write": []},
        }),
    );
    let program = sh_program("ulimit -t; ulimit -f; ulimit -n; ulimit -u");
    let result = match runtime::run(&program, &registry, &options(&cwd)) {
        Ok(result) => result,
        Err(error) if sandbox_unavailable(&error) => {
            eprintln!("skipping: OS sandbox facility unavailable: {error:#}");
            return;
        }
        Err(error) => panic!("sandboxed run failed: {error:#}"),
    };
    // Defaults from `capabilities::DEFAULT_*`: finite on every knob.
    // `ulimit -f` prints 512-byte blocks on Linux but 1024-byte blocks on
    // macOS (verified empirically): 1024 MiB is 2097152 or 1048576 blocks.
    #[cfg(target_os = "macos")]
    let expected = "120\n1048576\n1024\n2048\n";
    #[cfg(target_os = "linux")]
    let expected = "120\n2097152\n1024\n2048\n";
    assert_eq!(String::from_utf8(result.output).unwrap(), expected);
}

#[test]
fn untrusted_explicit_limits_are_installed() {
    // `rlim_t` is 64 bits on every supported target; the widening cast
    // Read the test runner's own soft ceilings so the explicit bounds below
    // can never ask to raise a hard limit. Each `getrlimit` names its
    // constant directly: the constant types differ per platform, so no
    // helper signature may name them. `rlim_t` is 64 bits on every
    // supported target, so no cast is needed.
    // SAFETY: `nproc_limit` is a live stack struct the kernel fills.
    let mut nproc_limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `getrlimit` only writes into the provided struct.
    let nproc_result = unsafe { libc::getrlimit(libc::RLIMIT_NPROC, nproc_limit.as_mut_ptr()) };
    assert_eq!(nproc_result, 0, "reading the test's own process limit");
    // SAFETY: `getrlimit` succeeded, so the struct is initialized.
    let nproc = unsafe { nproc_limit.assume_init().rlim_cur };
    // SAFETY: `nofile_limit` is a live stack struct the kernel fills.
    let mut nofile_limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `getrlimit` only writes into the provided struct.
    let nofile_result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, nofile_limit.as_mut_ptr()) };
    assert_eq!(nofile_result, 0, "reading the test's own file limit");
    // SAFETY: `getrlimit` succeeded, so the struct is initialized.
    let nofile = unsafe { nofile_limit.assume_init().rlim_cur };

    let (_directory, cwd) = workdir();
    let registry = untrusted_registry(
        &sh_manifest(&["process.exec"], &["process.exec"]),
        serde_json::json!({
            "capabilities": ["process.exec"],
            "filesystem": {"read": [], "write": []},
            "resource_limits": {
                "cpu_secs": 7,
                "file_size_mb": 1,
                "max_processes": nproc,
                "open_files": nofile,
            },
        }),
    );
    let program = sh_program("ulimit -t; ulimit -f; ulimit -u; ulimit -n");
    let result = match runtime::run(&program, &registry, &options(&cwd)) {
        Ok(result) => result,
        Err(error) if sandbox_unavailable(&error) => {
            eprintln!("skipping: OS sandbox facility unavailable: {error:#}");
            return;
        }
        Err(error) => panic!("sandboxed run failed: {error:#}"),
    };
    // The child's limits are exactly the configured values: the per-uid
    // process count is read back rather than tripped, so this never risks a
    // fork denial for the test runner itself. `ulimit -f` block size is
    // platform-specific (see the defaults test).
    #[cfg(target_os = "macos")]
    let expected = format!("7\n1024\n{nproc}\n{nofile}\n");
    #[cfg(target_os = "linux")]
    let expected = format!("7\n2048\n{nproc}\n{nofile}\n");
    assert_eq!(String::from_utf8(result.output).unwrap(), expected);
}

#[test]
fn untrusted_cpu_limit_kills_with_typed_resource_failure() {
    let (_directory, cwd) = workdir();
    let registry = untrusted_registry(
        &sh_manifest(&["process.exec"], &["process.exec"]),
        serde_json::json!({
            "capabilities": ["process.exec"],
            "filesystem": {"read": [], "write": []},
            "resource_limits": {"cpu_secs": 1},
        }),
    );
    let program = sh_program("while :; do :; done");
    let error = match runtime::run(&program, &registry, &options(&cwd)) {
        Err(error) if sandbox_unavailable(&error) => {
            eprintln!("skipping: OS sandbox facility unavailable: {error:#}");
            return;
        }
        Err(error) => error,
        Ok(_) => panic!("a busy loop under a 1-second CPU limit must die"),
    };
    let outcome = kind_of(&error);
    assert_eq!(outcome.kind, "resource_exhaust");
    assert_eq!(outcome.signal, Some(libc::SIGXCPU as i64));
    assert!(
        outcome.message.contains("CPU time"),
        "unexpected message: {}",
        outcome.message
    );
    let stage = &outcome.stages[0];
    assert_eq!(stage_text(stage, "state"), "signaled");
    assert_eq!(stage_text(stage, "kind"), "resource_exhaust");
    assert_eq!(stage_signal(stage), Some(libc::SIGXCPU as i64));
}

#[test]
fn untrusted_file_size_limit_kills_with_typed_resource_failure() {
    let (_directory, cwd) = workdir();
    std::fs::create_dir_all(cwd.join("granted")).unwrap();
    let registry = untrusted_registry(
        &sh_manifest(
            &["process.exec", "filesystem.write"],
            &["process.exec", "filesystem.write"],
        ),
        serde_json::json!({
            "capabilities": ["process.exec", "filesystem.write"],
            "filesystem": {"read": [], "write": [cwd.join("granted")]},
            "resource_limits": {"file_size_mb": 1},
        }),
    );
    // Shell builtins only: the confined profile denies executing other
    // binaries. The loop writes 100KB per iteration until SIGXFSZ kills it
    // at 1 MiB; no shell variables are used, so nothing can interpolate.
    let program = sh_program("while :; do printf '%100000s' ' '; done > granted/big.bin");
    let error = match runtime::run(&program, &registry, &options(&cwd)) {
        Err(error) if sandbox_unavailable(&error) => {
            eprintln!("skipping: OS sandbox facility unavailable: {error:#}");
            return;
        }
        Err(error) => error,
        Ok(_) => panic!("writing 2MB under a 1 MiB file-size limit must die"),
    };
    let outcome = kind_of(&error);
    assert_eq!(outcome.kind, "resource_exhaust");
    assert_eq!(outcome.signal, Some(libc::SIGXFSZ as i64));
    assert!(
        outcome.message.contains("file size"),
        "unexpected message: {}",
        outcome.message
    );
    let stage = &outcome.stages[0];
    assert_eq!(stage_text(stage, "state"), "signaled");
    assert_eq!(stage_text(stage, "kind"), "resource_exhaust");
    assert_eq!(stage_signal(stage), Some(libc::SIGXFSZ as i64));
    // The cap bounds the single file, whatever the kill signal raced with.
    let length = std::fs::metadata(cwd.join("granted/big.bin"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    assert!(
        length <= 1024 * 1024 + 4096,
        "file grew past the cap: {length}"
    );
}

#[test]
fn trusted_mode_runs_unbounded_unless_the_policy_asks() {
    let (_directory, cwd) = workdir();
    // No explicit bounds: trusted mode keeps the inherited environment.
    let registry = trusted_registry(
        &sh_manifest(&["process.exec"], &["process.exec"]),
        serde_json::json!({
            "capabilities": ["process.exec"],
            "filesystem": {"read": [], "write": []},
        }),
    );
    let program = sh_program("ulimit -t");
    let result = runtime::run(&program, &registry, &options(&cwd))
        .expect("trusted run without explicit limits must spawn");
    assert_eq!(
        String::from_utf8(result.output).unwrap().trim(),
        "unlimited"
    );
    // An explicit bound is the policy asking: trusted mode applies it too.
    let registry = trusted_registry(
        &sh_manifest(&["process.exec"], &["process.exec"]),
        serde_json::json!({
            "capabilities": ["process.exec"],
            "filesystem": {"read": [], "write": []},
            "resource_limits": {"cpu_secs": 7},
        }),
    );
    let result = runtime::run(&program, &registry, &options(&cwd))
        .expect("trusted run with explicit limits must spawn");
    assert_eq!(String::from_utf8(result.output).unwrap().trim(), "7");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_address_space_bounds_refuse_at_spawn() {
    let (_directory, cwd) = workdir();
    let registry = untrusted_registry(
        &sh_manifest(&["process.exec"], &["process.exec"]),
        serde_json::json!({
            "capabilities": ["process.exec"],
            "filesystem": {"read": [], "write": []},
            "resource_limits": {"address_space_mb": 64},
        }),
    );
    let program = sh_program("exit 0");
    let error = runtime::run(&program, &registry, &options(&cwd))
        .err()
        .expect("macOS cannot install RLIMIT_AS, so the spawn must be refused");
    assert!(
        error
            .downcast_ref::<flow::capabilities::SandboxRefused>()
            .is_some(),
        "expected a typed sandbox refusal, got: {error:#}"
    );
    assert!(format!("{error:#}").contains("macOS"));
}
