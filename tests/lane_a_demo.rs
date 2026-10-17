//! CI check for examples/demo: check, explain, a failing checkpointed run, and a resume
//! after the fake compositor is fixed. Unix only, because the fake tool is a shell script.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn demo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo")
}

fn inputs(root: &Path) -> Vec<String> {
    [
        format!("theme_name=dark"),
        format!("themes_root={}", root.join("themes").display()),
        format!("current_link={}", root.join("current/theme").display()),
        format!("templates={}", demo().join("templates/*.tpl").display()),
        "values_json={\"background\":\"#1e1e2e\",\"foreground\":\"#cdd6f4\"}".to_owned(),
    ]
    .into_iter()
    .flat_map(|pair| ["--input".to_owned(), pair])
    .collect()
}

/// The fake tools, copied so that their executable bit is set on any checkout.
fn fake_tools(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("hyprctl");
    fs::copy(demo().join("fixtures/bin/hyprctl"), &tool).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Run `execute VERB FILE --manifest-dir DIR --input ... EXTRA`, with the fake tools first on PATH.
fn execute(root: &Path, bin: &Path, verb: &str, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(root)
        .arg(verb)
        .arg(demo().join("theme-refresh.program"))
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .args(inputs(root))
        .args(extra);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    command
        .env("PATH", path)
        .env("DEMO_LOG", root.join("calls.log"));
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

#[test]
fn the_walk_through_fails_at_the_reload_and_resumes_after_the_fix() {
    let root = TempDir::new().unwrap();
    let bin = fake_tools(root.path());
    let state = root.path().join("state").display().to_string();

    let check = execute(root.path(), &bin, "check", &[], &[]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );

    let check_json = execute(root.path(), &bin, "check", &["--json"], &[]);
    let report: serde_json::Value = serde_json::from_slice(&check_json.stdout).unwrap();
    assert_eq!(report["ok"], true);

    let explain = execute(root.path(), &bin, "explain", &[], &[]);
    assert!(
        explain.status.success(),
        "{}",
        String::from_utf8_lossy(&explain.stderr)
    );
    assert!(String::from_utf8_lossy(&explain.stdout).contains("hyprctl"));
    assert!(
        !root.path().join("current/theme").exists(),
        "explain changed the link"
    );

    let failed = execute(root.path(), &bin, "run", &["--checkpoint", &state], &[]);
    assert!(
        !failed.status.success(),
        "the reload should fail until the fake is fixed"
    );
    assert!(String::from_utf8_lossy(&failed.stderr).contains("reload failed"));
    assert_eq!(
        fs::read_link(root.path().join("current/theme")).unwrap(),
        root.path().join("themes/dark")
    );

    let resumed = execute(
        root.path(),
        &bin,
        "resume",
        &["--checkpoint", &state],
        &[("DEMO_RELOAD_OK", "1")],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("theme dark active"));

    let rendered = fs::read_to_string(root.path().join("themes/dark/alacritty.toml")).unwrap();
    assert!(rendered.contains("background = \"#1e1e2e\""), "{rendered}");
    let calls = fs::read_to_string(root.path().join("calls.log")).unwrap();
    assert_eq!(calls.matches("hyprctl reload").count(), 2, "{calls}");
}
