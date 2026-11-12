//! Installation and desktop-session checks behind `execute doctor` (#424).
//!
//! Every check reads its facts through [`Environment`], so the tests run against a
//! fake and never touch the real machine. [`SystemEnvironment`] is the only
//! implementation that reads the process environment, the file system, PATH or a
//! helper binary.
//!
//! Where each requirement comes from:
//! - Helpers: `src/runtime.rs` (`bundled_helper`) looks for `execute-applet` and
//!   `execute-model` beside the running executable (`.exe` on Windows), refuses a
//!   symbolic link, and runs `<name> --version`, which must print `<name> <version>`
//!   exactly (`verify_helper_version_handshake`).
//! - Manifests: `src/main.rs` (`manifest_candidates`) looks in `<install>/manifests` and
//!   `<install>/share/execute/manifests`, where `<install>` is the parent of the
//!   executable's directory. `src/bundled_manifests.rs` (`directory_matches`) loads a
//!   directory from the compiled-in index only when its JSON names match the index.
//!   Without a directory `Registry::default()` is empty (`src/manifest.rs`), so no
//!   applet resolves.
//! - Home: `src/home.rs` (`expand_home_path`) reads HOME, then USERPROFILE.
//! - State: `src/agent/what_if_key.rs` (`key_path`) is the only state location in the
//!   code: `LOCALAPPDATA\execute` on Windows, otherwise `$XDG_STATE_HOME` (absolute) or
//!   `$HOME/.local/state`, then `execute`.
//! - Cache: there is no default cache location; `--cache-dir` names one per run.
//! - Desktop: `src/desktop.rs` (`verified_xdg_runtime_directory`) requires an absolute
//!   `XDG_RUNTIME_DIR`. The Hyprland event socket is
//!   `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock`
//!   (`docs/applet-contracts.md`). No code reads `WAYLAND_DISPLAY`, so that check is
//!   advisory.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Protocol id of the `--json` rendering, in the `execute.<name>/<n>` house style.
pub const PROTOCOL: &str = "execute.doctor/1";

/// Helper binaries the runtime looks for beside the running executable:
/// (check id, title, binary name without `.exe`).
const HELPERS: [(&str, &str, &str); 2] = [
    (
        "helpers.execute-applet",
        "Helper binary execute-applet",
        "execute-applet",
    ),
    (
        "helpers.execute-model",
        "Helper binary execute-model",
        "execute-model",
    ),
];

const BUNDLED_INDEX_ID: &str = "manifests.bundled-index";
const MANIFEST_DIR_ID: &str = "manifests.directory";
const HOME_ID: &str = "dirs.home";
const STATE_ID: &str = "dirs.state";
const CACHE_ID: &str = "dirs.cache";
const XDG_RUNTIME_ID: &str = "desktop.xdg-runtime-dir";
const HYPRLAND_ID: &str = "desktop.hyprland-signature";
const WAYLAND_ID: &str = "desktop.wayland-display";

const INSTALL_REMEDY: &str = "reinstall so execute, execute-model and execute-applet sit in one directory at the same version (docs/releases.md)";
const MANIFEST_REMEDY: &str = "install manifests/ beside bin/ or under share/execute/ (docs/releases.md); without it no applet resolves";

/// Outcome of one check. `Skipped` means the check does not apply here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skipped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skipped => "skipped",
        }
    }
}

/// One verified fact about the installation or the desktop session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    /// One line describing what was found.
    pub detail: String,
    /// One line saying what to do, present for problems only.
    pub remedy: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Missing,
    File,
    Directory,
    Symlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirState {
    Writable,
    /// Does not exist yet, and its nearest existing parent is writable.
    Absent,
    NotWritable,
    NotADirectory,
}

/// Every fact the checks read. Only [`SystemEnvironment`] touches the real machine.
pub trait Environment {
    fn platform(&self) -> Platform;
    /// The version this build reports; helper binaries must report the same one.
    fn build_version(&self) -> &str;
    /// The raw value of an environment variable, empty values included.
    fn env_var(&self, name: &str) -> Option<String>;
    /// The running executable, the anchor for helpers and manifests.
    fn current_exe(&self) -> Option<PathBuf>;
    /// The kind of entry at `path`, without following a final symbolic link.
    fn file_kind(&self, path: &Path) -> Result<FileKind, String>;
    /// Whether a directory can be written, or whether it would be created there.
    fn dir_state(&self, path: &Path) -> DirState;
    /// Entry names in a directory.
    fn list_names(&self, dir: &Path) -> Result<Vec<String>, String>;
    /// Runs `<helper> --version` and returns its trimmed standard output.
    fn helper_version(&self, helper: &Path) -> Result<String, String>;
    /// The first executable of that name on PATH.
    fn find_on_path(&self, binary: &str) -> Option<PathBuf>;
    /// File names of the manifests compiled into this binary.
    fn bundled_manifest_files(&self) -> Result<Vec<String>, String>;
}

/// The real machine: process environment, file system, PATH and helper processes.
pub struct SystemEnvironment;

impl Environment for SystemEnvironment {
    fn platform(&self) -> Platform {
        if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Other
        }
    }

    fn build_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
    }

    fn current_exe(&self) -> Option<PathBuf> {
        std::env::current_exe().ok()
    }

    fn file_kind(&self, path: &Path) -> Result<FileKind, String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Ok(FileKind::Symlink),
            Ok(metadata) if metadata.is_dir() => Ok(FileKind::Directory),
            Ok(_) => Ok(FileKind::File),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileKind::Missing),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Writability is probed by creating and removing a small file, because the
    /// permission bits alone do not answer for ACLs or read-only mounts.
    fn dir_state(&self, path: &Path) -> DirState {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                if probe_writable(path) {
                    DirState::Writable
                } else {
                    DirState::NotWritable
                }
            }
            Ok(_) => DirState::NotADirectory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let existing = path
                    .ancestors()
                    .skip(1)
                    .find(|candidate| candidate.exists());
                match existing {
                    Some(parent) if parent.is_dir() && probe_writable(parent) => DirState::Absent,
                    _ => DirState::NotWritable,
                }
            }
            Err(_) => DirState::NotWritable,
        }
    }

    fn list_names(&self, dir: &Path) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn helper_version(&self, helper: &Path) -> Result<String, String> {
        let output = std::process::Command::new(helper)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!("exited with {}", output.status));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn find_on_path(&self, binary: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let suffixes: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
        std::env::split_paths(&path)
            .flat_map(|dir| {
                suffixes
                    .iter()
                    .map(move |suffix| dir.join(format!("{binary}{suffix}")))
            })
            .find(|candidate| candidate.is_file())
    }

    fn bundled_manifest_files(&self) -> Result<Vec<String>, String> {
        let index = crate::bundled_manifests::index().map_err(|error| format!("{error:#}"))?;
        Ok(index
            .entries()
            .iter()
            .map(|entry| entry.file().to_owned())
            .collect())
    }
}

fn probe_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".execute-doctor-probe-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Run every check in a fixed order. Stable ids make the output safe to script against.
pub fn run_checks(env: &dyn Environment) -> Vec<Check> {
    let mut checks = Vec::new();
    for (id, title, name) in HELPERS {
        checks.push(check_helper(env, id, title, name));
    }
    checks.push(check_bundled_index(env));
    checks.push(check_manifest_dir(env));
    checks.push(check_home(env));
    checks.push(check_state(env));
    checks.push(check(
        CACHE_ID,
        "Cache directory",
        Status::Skipped,
        "no default cache directory; run and resume take --cache-dir",
        None,
    ));
    checks.extend(desktop_checks(env));
    checks
}

/// True when any check failed. Warnings and skipped checks do not fail the run.
pub fn has_failures(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.status == Status::Fail)
}

/// The stable `--json` rendering, protocol [`PROTOCOL`].
pub fn render_json(checks: &[Check], build_version: &str) -> Value {
    let count = |status: Status| checks.iter().filter(|check| check.status == status).count();
    json!({
        "protocol": PROTOCOL,
        "execute_version": build_version,
        "ok": !has_failures(checks),
        "counts": {
            "ok": count(Status::Ok),
            "warn": count(Status::Warn),
            "fail": count(Status::Fail),
            "skipped": count(Status::Skipped),
        },
        "checks": checks.iter().map(|check| json!({
            "id": check.id,
            "title": check.title,
            "status": check.status.label(),
            "detail": check.detail,
            "remedy": check.remedy,
        })).collect::<Vec<_>>(),
    })
}

/// The plain text rendering: one line per check, a `fix:` line under each problem.
pub fn render_text(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let tag = format!("[{}]", check.status.label());
        let _ = writeln!(
            out,
            "{tag:<10}{} ({}): {}",
            check.title, check.id, check.detail
        );
        if let Some(remedy) = &check.remedy {
            let _ = writeln!(out, "          fix: {remedy}");
        }
    }
    let count = |status: Status| checks.iter().filter(|check| check.status == status).count();
    let _ = writeln!(
        out,
        "{} failed, {} warnings, {} skipped, {} ok",
        count(Status::Fail),
        count(Status::Warn),
        count(Status::Skipped),
        count(Status::Ok)
    );
    out
}

fn check(
    id: &'static str,
    title: &'static str,
    status: Status,
    detail: impl Into<String>,
    remedy: Option<&str>,
) -> Check {
    Check {
        id,
        title,
        status,
        detail: one_line(detail.into()),
        remedy: remedy.map(|text| one_line(text.to_owned())),
    }
}

/// Keeps every detail and remedy on one line, whatever a child process printed.
fn one_line(text: String) -> String {
    text.replace(['\r', '\n'], " ")
}

fn var(env: &dyn Environment, name: &str) -> Option<String> {
    env.env_var(name).filter(|value| !value.is_empty())
}

/// The directories the runtime looks in for helpers and manifests, in the order
/// `src/main.rs` searches them.
fn manifest_candidates(env: &dyn Environment) -> Option<[PathBuf; 2]> {
    let exe = env.current_exe()?;
    let install_root = exe.parent()?.parent()?;
    Some([
        install_root.join("manifests"),
        install_root.join("share").join("execute").join("manifests"),
    ])
}

fn check_helper(env: &dyn Environment, id: &'static str, title: &'static str, name: &str) -> Check {
    let file_name = if env.platform() == Platform::Windows {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let Some(dir) = env
        .current_exe()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return check(
            id,
            title,
            Status::Fail,
            "cannot locate the running executable",
            Some("run execute from its installed bin directory"),
        );
    };
    let path = dir.join(&file_name);
    match env.file_kind(&path) {
        Ok(FileKind::File) => {}
        Ok(FileKind::Missing) => {
            return check(
                id,
                title,
                Status::Fail,
                format!(
                    "{file_name} is not beside the executable in {}",
                    dir.display()
                ),
                Some(INSTALL_REMEDY),
            );
        }
        Ok(FileKind::Symlink) => {
            return check(
                id,
                title,
                Status::Fail,
                format!("{file_name} is a symbolic link; the runtime refuses links"),
                Some("replace the link with the real binary"),
            );
        }
        Ok(FileKind::Directory) => {
            return check(
                id,
                title,
                Status::Fail,
                format!("{file_name} is a directory, not a binary"),
                Some(INSTALL_REMEDY),
            );
        }
        Err(error) => {
            return check(
                id,
                title,
                Status::Fail,
                format!("cannot inspect {file_name}: {error}"),
                Some(INSTALL_REMEDY),
            );
        }
    }
    let expected = format!("{name} {}", env.build_version());
    match env.helper_version(&path) {
        Ok(reported) if reported == expected => check(
            id,
            title,
            Status::Ok,
            format!("{file_name} reports `{expected}`"),
            None,
        ),
        Ok(reported) => check(
            id,
            title,
            Status::Fail,
            format!("{file_name} reports `{reported}`, expected `{expected}`"),
            Some("rebuild or reinstall all three binaries from the same version"),
        ),
        Err(error) => check(
            id,
            title,
            Status::Fail,
            format!("{file_name} did not answer --version: {error}"),
            Some(INSTALL_REMEDY),
        ),
    }
}

fn check_bundled_index(env: &dyn Environment) -> Check {
    match env.bundled_manifest_files() {
        Err(error) => check(
            BUNDLED_INDEX_ID,
            "Bundled manifest index",
            Status::Fail,
            format!("the index compiled into this binary did not load: {error}"),
            Some("rebuild the binary; the index is part of it"),
        ),
        Ok(files) if files.is_empty() => check(
            BUNDLED_INDEX_ID,
            "Bundled manifest index",
            Status::Fail,
            "the index compiled into this binary lists no manifests",
            Some("rebuild the binary; the index is part of it"),
        ),
        Ok(files) => check(
            BUNDLED_INDEX_ID,
            "Bundled manifest index",
            Status::Ok,
            format!("{} bundled manifests indexed", files.len()),
            None,
        ),
    }
}

fn check_manifest_dir(env: &dyn Environment) -> Check {
    let title = "Manifest directory";
    let Some(candidates) = manifest_candidates(env) else {
        return check(
            MANIFEST_DIR_ID,
            title,
            Status::Fail,
            "cannot locate the running executable",
            Some("run execute from its installed bin directory"),
        );
    };
    let Some(root) = candidates.iter().find(|candidate| {
        matches!(
            env.file_kind(candidate),
            Ok(FileKind::Directory | FileKind::Symlink)
        )
    }) else {
        let searched = candidates
            .iter()
            .map(|candidate| candidate.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ");
        return check(
            MANIFEST_DIR_ID,
            title,
            Status::Fail,
            format!("no manifest directory; looked in {searched}"),
            Some(MANIFEST_REMEDY),
        );
    };
    let actual: BTreeSet<String> = match env.list_names(root) {
        Ok(names) => names
            .into_iter()
            .filter(|name| name.ends_with(".json"))
            .collect(),
        Err(error) => {
            return check(
                MANIFEST_DIR_ID,
                title,
                Status::Fail,
                format!("cannot read {}: {error}", root.display()),
                Some("give this user read access to the manifest directory"),
            );
        }
    };
    let expected: BTreeSet<String> = match env.bundled_manifest_files() {
        Ok(files) => files.into_iter().collect(),
        Err(_) => {
            return check(
                MANIFEST_DIR_ID,
                title,
                Status::Skipped,
                "the bundled index did not load (see manifests.bundled-index)",
                None,
            );
        }
    };
    let missing: Vec<&String> = expected.difference(&actual).collect();
    let extra: Vec<&String> = actual.difference(&expected).collect();
    if let Some(first) = missing.first() {
        check(
            MANIFEST_DIR_ID,
            title,
            Status::Fail,
            format!(
                "{} lacks {} bundled manifest(s), first {first}",
                root.display(),
                missing.len()
            ),
            Some("restore the missing manifests from the release (docs/releases.md)"),
        )
    } else if let Some(first) = extra.first() {
        check(
            MANIFEST_DIR_ID,
            title,
            Status::Warn,
            format!(
                "{} has {} manifest(s) the bundled index does not list, first {first}",
                root.display(),
                extra.len()
            ),
            Some("remove the extra files; the directory then matches the bundled index"),
        )
    } else {
        check(
            MANIFEST_DIR_ID,
            title,
            Status::Ok,
            format!(
                "{} holds the {} bundled manifests",
                root.display(),
                expected.len()
            ),
            None,
        )
    }
}

fn check_home(env: &dyn Environment) -> Check {
    let Some(home) = var(env, "HOME").or_else(|| var(env, "USERPROFILE")) else {
        return check(
            HOME_ID,
            "Home directory",
            Status::Fail,
            "neither HOME nor USERPROFILE is set, so ~ paths cannot expand",
            Some("set HOME to this user's home directory"),
        );
    };
    dir_check(
        env,
        HOME_ID,
        "Home directory",
        Path::new(&home),
        "give this user write access to the home directory, or set HOME to one that is writable",
    )
}

/// The state directory, resolved the way `src/agent/what_if_key.rs` resolves it.
fn state_dir(env: &dyn Environment) -> Result<PathBuf, String> {
    if env.platform() == Platform::Windows {
        let base = var(env, "LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
        return Ok(PathBuf::from(base).join("execute"));
    }
    let base = match var(env, "XDG_STATE_HOME") {
        Some(path) if path.starts_with('/') => PathBuf::from(path),
        _ => {
            let home = var(env, "HOME").ok_or("HOME is not set")?;
            PathBuf::from(home).join(".local").join("state")
        }
    };
    Ok(base.join("execute"))
}

fn check_state(env: &dyn Environment) -> Check {
    let remedy = "give this user write access to the state directory, or point XDG_STATE_HOME (LOCALAPPDATA on Windows) at a writable location";
    match state_dir(env) {
        Ok(path) => dir_check(env, STATE_ID, "State directory", &path, remedy),
        Err(error) => check(
            STATE_ID,
            "State directory",
            Status::Fail,
            format!("the state directory does not resolve: {error}"),
            Some("set HOME (or LOCALAPPDATA on Windows) so the state directory resolves"),
        ),
    }
}

fn dir_check(
    env: &dyn Environment,
    id: &'static str,
    title: &'static str,
    path: &Path,
    remedy: &str,
) -> Check {
    match env.dir_state(path) {
        DirState::Writable => check(
            id,
            title,
            Status::Ok,
            format!("{} is writable", path.display()),
            None,
        ),
        DirState::Absent => check(
            id,
            title,
            Status::Ok,
            format!(
                "{} does not exist yet; its parent is writable",
                path.display()
            ),
            None,
        ),
        DirState::NotWritable => check(
            id,
            title,
            Status::Fail,
            format!("{} is not writable", path.display()),
            Some(remedy),
        ),
        DirState::NotADirectory => check(
            id,
            title,
            Status::Fail,
            format!("{} is not a directory", path.display()),
            Some(remedy),
        ),
    }
}

/// The desktop-session checks, decided from environment variables only. They do not
/// inspect the runtime directory's owner or mode, which `src/desktop.rs` enforces
/// when a program starts.
fn desktop_checks(env: &dyn Environment) -> Vec<Check> {
    if env.platform() != Platform::Linux {
        let detail = "desktop session checks apply to Linux only";
        return vec![
            check(
                XDG_RUNTIME_ID,
                "XDG_RUNTIME_DIR",
                Status::Skipped,
                detail,
                None,
            ),
            check(
                HYPRLAND_ID,
                "Hyprland session",
                Status::Skipped,
                detail,
                None,
            ),
            check(WAYLAND_ID, "Wayland display", Status::Skipped, detail, None),
        ];
    }
    let xdg = match var(env, "XDG_RUNTIME_DIR") {
        None => check(
            XDG_RUNTIME_ID,
            "XDG_RUNTIME_DIR",
            Status::Fail,
            "XDG_RUNTIME_DIR is not set",
            Some("log in through systemd-logind or another session manager that sets it"),
        ),
        Some(path) if !path.starts_with('/') => check(
            XDG_RUNTIME_ID,
            "XDG_RUNTIME_DIR",
            Status::Fail,
            format!("XDG_RUNTIME_DIR is `{path}`, not an absolute path"),
            Some("set XDG_RUNTIME_DIR to an absolute private directory such as /run/user/<uid>"),
        ),
        Some(path) => check(
            XDG_RUNTIME_ID,
            "XDG_RUNTIME_DIR",
            Status::Ok,
            format!("XDG_RUNTIME_DIR is {path} (owner and mode not inspected)"),
            None,
        ),
    };
    let hyprland = match var(env, "HYPRLAND_INSTANCE_SIGNATURE") {
        None => check(
            HYPRLAND_ID,
            "Hyprland session",
            Status::Warn,
            "HYPRLAND_INSTANCE_SIGNATURE is not set, so this is not a Hyprland session",
            Some("run from a Hyprland session; hypr.* applets need it"),
        ),
        Some(signature) => check(
            HYPRLAND_ID,
            "Hyprland session",
            Status::Ok,
            format!("HYPRLAND_INSTANCE_SIGNATURE is set; its event socket is $XDG_RUNTIME_DIR/hypr/{signature}/.socket2.sock"),
            None,
        ),
    };
    let wayland = match var(env, "WAYLAND_DISPLAY") {
        None => check(
            WAYLAND_ID,
            "Wayland display",
            Status::Warn,
            "WAYLAND_DISPLAY is not set, so no Wayland compositor is advertised",
            Some("start the session from Wayland; this check is advisory"),
        ),
        Some(display) => check(
            WAYLAND_ID,
            "Wayland display",
            Status::Ok,
            format!("WAYLAND_DISPLAY is {display}"),
            None,
        ),
    };
    vec![xdg, hyprland, wayland]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const VERSION: &str = "9.9.9";

    /// A fake machine. Anything not set reads as missing, unwritable or unavailable.
    struct Fake {
        platform: Platform,
        version: String,
        exe: Option<PathBuf>,
        vars: BTreeMap<String, String>,
        kinds: BTreeMap<PathBuf, FileKind>,
        dir_states: BTreeMap<PathBuf, DirState>,
        listings: BTreeMap<PathBuf, Result<Vec<String>, String>>,
        versions: BTreeMap<PathBuf, Result<String, String>>,
        bundled: Result<Vec<String>, String>,
    }

    impl Environment for Fake {
        fn platform(&self) -> Platform {
            self.platform
        }
        fn build_version(&self) -> &str {
            &self.version
        }
        fn env_var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn current_exe(&self) -> Option<PathBuf> {
            self.exe.clone()
        }
        fn file_kind(&self, path: &Path) -> Result<FileKind, String> {
            Ok(self.kinds.get(path).copied().unwrap_or(FileKind::Missing))
        }
        fn dir_state(&self, path: &Path) -> DirState {
            self.dir_states
                .get(path)
                .copied()
                .unwrap_or(DirState::NotWritable)
        }
        fn list_names(&self, dir: &Path) -> Result<Vec<String>, String> {
            self.listings
                .get(dir)
                .cloned()
                .unwrap_or_else(|| Err("no such directory".to_owned()))
        }
        fn helper_version(&self, helper: &Path) -> Result<String, String> {
            self.versions
                .get(helper)
                .cloned()
                .unwrap_or_else(|| Err("not runnable".to_owned()))
        }
        fn find_on_path(&self, _binary: &str) -> Option<PathBuf> {
            None
        }
        fn bundled_manifest_files(&self) -> Result<Vec<String>, String> {
            self.bundled.clone()
        }
    }

    fn bin() -> PathBuf {
        PathBuf::from("/opt/execute/bin")
    }

    fn manifests() -> PathBuf {
        PathBuf::from("/opt/execute/share/execute/manifests")
    }

    /// A healthy Linux install with a desktop session.
    fn healthy() -> Fake {
        let mut fake = Fake {
            platform: Platform::Linux,
            version: VERSION.to_owned(),
            exe: Some(bin().join("execute")),
            vars: BTreeMap::new(),
            kinds: BTreeMap::new(),
            dir_states: BTreeMap::new(),
            listings: BTreeMap::new(),
            versions: BTreeMap::new(),
            bundled: Ok(vec!["awk.json".to_owned(), "cat.json".to_owned()]),
        };
        for (key, value) in [
            ("HOME", "/home/u"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "abc"),
            ("WAYLAND_DISPLAY", "wayland-1"),
        ] {
            fake.vars.insert(key.to_owned(), value.to_owned());
        }
        for name in ["execute-applet", "execute-model"] {
            let path = bin().join(name);
            fake.kinds.insert(path.clone(), FileKind::File);
            fake.versions.insert(path, Ok(format!("{name} {VERSION}")));
        }
        fake.kinds.insert(manifests(), FileKind::Directory);
        fake.listings.insert(
            manifests(),
            Ok(vec![
                "awk.json".to_owned(),
                "cat.json".to_owned(),
                "README.txt".to_owned(),
            ]),
        );
        fake.dir_states
            .insert(PathBuf::from("/home/u"), DirState::Writable);
        fake.dir_states.insert(
            PathBuf::from("/home/u/.local/state/execute"),
            DirState::Absent,
        );
        fake
    }

    fn find<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks
            .iter()
            .find(|check| check.id == id)
            .expect("check present")
    }

    #[test]
    fn healthy_linux_install_is_ok_except_the_cache_check() {
        let checks = run_checks(&healthy());
        let ids: Vec<&str> = checks.iter().map(|check| check.id).collect();
        assert_eq!(
            ids,
            [
                "helpers.execute-applet",
                "helpers.execute-model",
                "manifests.bundled-index",
                "manifests.directory",
                "dirs.home",
                "dirs.state",
                "dirs.cache",
                "desktop.xdg-runtime-dir",
                "desktop.hyprland-signature",
                "desktop.wayland-display",
            ]
        );
        for check in &checks {
            let expected = if check.id == "dirs.cache" {
                Status::Skipped
            } else {
                Status::Ok
            };
            assert_eq!(check.status, expected, "{}: {}", check.id, check.detail);
            assert!(
                check.remedy.is_none(),
                "{} has no remedy when healthy",
                check.id
            );
        }
        assert!(!has_failures(&checks));
    }

    #[test]
    fn missing_helper_fails_with_remedy() {
        let mut fake = healthy();
        fake.kinds.remove(&bin().join("execute-applet"));
        let checks = run_checks(&fake);
        let check = find(&checks, "helpers.execute-applet");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("execute-applet is not beside"));
        assert!(check.remedy.is_some());
        assert_eq!(find(&checks, "helpers.execute-model").status, Status::Ok);
        assert!(has_failures(&checks));
    }

    #[test]
    fn symlinked_helper_fails() {
        let mut fake = healthy();
        fake.kinds
            .insert(bin().join("execute-model"), FileKind::Symlink);
        let checks = run_checks(&fake);
        let check = find(&checks, "helpers.execute-model");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("symbolic link"));
    }

    #[test]
    fn mismatched_helper_version_fails() {
        let mut fake = healthy();
        fake.versions.insert(
            bin().join("execute-applet"),
            Ok("execute-applet 0.0.1".to_owned()),
        );
        let checks = run_checks(&fake);
        let check = find(&checks, "helpers.execute-applet");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("expected `execute-applet 9.9.9`"));
        assert!(check.remedy.is_some());
    }

    #[test]
    fn helper_that_does_not_answer_version_fails() {
        let mut fake = healthy();
        fake.versions.insert(
            bin().join("execute-model"),
            Err("exited with exit code: 1".to_owned()),
        );
        let checks = run_checks(&fake);
        let check = find(&checks, "helpers.execute-model");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("did not answer --version"));
    }

    #[test]
    fn no_running_executable_fails_both_helpers() {
        let mut fake = healthy();
        fake.exe = None;
        let checks = run_checks(&fake);
        assert_eq!(find(&checks, "helpers.execute-applet").status, Status::Fail);
        assert_eq!(find(&checks, "helpers.execute-model").status, Status::Fail);
        assert_eq!(find(&checks, "manifests.directory").status, Status::Fail);
    }

    #[test]
    fn windows_helpers_are_looked_up_with_exe_suffix() {
        let mut fake = healthy();
        fake.platform = Platform::Windows;
        fake.kinds.clear();
        fake.versions.clear();
        for name in ["execute-applet", "execute-model"] {
            let path = bin().join(format!("{name}.exe"));
            fake.kinds.insert(path.clone(), FileKind::File);
            fake.versions.insert(path, Ok(format!("{name} {VERSION}")));
        }
        let checks = run_checks(&fake);
        assert_eq!(find(&checks, "helpers.execute-applet").status, Status::Ok);
        assert_eq!(find(&checks, "helpers.execute-model").status, Status::Ok);

        fake.kinds.remove(&bin().join("execute-applet.exe"));
        let checks = run_checks(&fake);
        let check = find(&checks, "helpers.execute-applet");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("execute-applet.exe"));
    }

    #[test]
    fn bundled_index_ok_fail_and_empty() {
        let mut fake = healthy();
        assert_eq!(
            find(&run_checks(&fake), "manifests.bundled-index").status,
            Status::Ok
        );

        fake.bundled = Err("inflating bundled manifest bodies".to_owned());
        let checks = run_checks(&fake);
        let check = find(&checks, "manifests.bundled-index");
        assert_eq!(check.status, Status::Fail);
        assert!(check.remedy.is_some());
        // The directory check cannot compare against an index that did not load.
        assert_eq!(find(&checks, "manifests.directory").status, Status::Skipped);

        fake.bundled = Ok(Vec::new());
        assert_eq!(
            find(&run_checks(&fake), "manifests.bundled-index").status,
            Status::Fail
        );
    }

    #[test]
    fn missing_manifest_directory_fails_and_names_both_locations() {
        let mut fake = healthy();
        fake.kinds.remove(&manifests());
        let checks = run_checks(&fake);
        let check = find(&checks, "manifests.directory");
        assert_eq!(check.status, Status::Fail);
        let install = PathBuf::from("/opt/execute");
        assert!(check
            .detail
            .contains(&install.join("manifests").display().to_string()));
        assert!(check.detail.contains(
            &install
                .join("share")
                .join("execute")
                .join("manifests")
                .display()
                .to_string()
        ));
        assert!(check.remedy.as_deref().unwrap_or("").contains("bin/"));
    }

    #[test]
    fn symlinked_manifest_directory_is_accepted() {
        let mut fake = healthy();
        fake.kinds.insert(manifests(), FileKind::Symlink);
        assert_eq!(
            find(&run_checks(&fake), "manifests.directory").status,
            Status::Ok
        );
    }

    #[test]
    fn manifest_directory_missing_a_bundled_file_fails() {
        let mut fake = healthy();
        fake.listings
            .insert(manifests(), Ok(vec!["awk.json".to_owned()]));
        let checks = run_checks(&fake);
        let check = find(&checks, "manifests.directory");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("cat.json"));
    }

    #[test]
    fn manifest_directory_with_an_extra_manifest_warns() {
        let mut fake = healthy();
        fake.listings.insert(
            manifests(),
            Ok(vec![
                "awk.json".to_owned(),
                "cat.json".to_owned(),
                "local.json".to_owned(),
            ]),
        );
        let checks = run_checks(&fake);
        let check = find(&checks, "manifests.directory");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("local.json"));
        assert!(!has_failures(&checks));
    }

    #[test]
    fn unreadable_manifest_directory_fails() {
        let mut fake = healthy();
        fake.listings
            .insert(manifests(), Err("permission denied".to_owned()));
        let check = find(&run_checks(&fake), "manifests.directory").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("permission denied"));
    }

    #[test]
    fn home_unset_fails_and_state_follows_it() {
        let mut fake = healthy();
        fake.vars.remove("HOME");
        let checks = run_checks(&fake);
        assert_eq!(find(&checks, "dirs.home").status, Status::Fail);
        assert_eq!(find(&checks, "dirs.state").status, Status::Fail);
    }

    #[test]
    fn userprofile_is_the_home_fallback() {
        let mut fake = healthy();
        fake.vars.remove("HOME");
        fake.vars
            .insert("USERPROFILE".to_owned(), "/home/u".to_owned());
        assert_eq!(find(&run_checks(&fake), "dirs.home").status, Status::Ok);
    }

    #[test]
    fn unwritable_home_fails() {
        let mut fake = healthy();
        fake.dir_states
            .insert(PathBuf::from("/home/u"), DirState::NotWritable);
        let check = find(&run_checks(&fake), "dirs.home").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.remedy.is_some());
    }

    #[test]
    fn state_directory_resolves_through_xdg_state_home() {
        let mut fake = healthy();
        fake.vars
            .insert("XDG_STATE_HOME".to_owned(), "/srv/state".to_owned());
        fake.dir_states
            .insert(PathBuf::from("/srv/state/execute"), DirState::Writable);
        // Ok only if the resolved path is the one the fake knows as writable.
        assert_eq!(find(&run_checks(&fake), "dirs.state").status, Status::Ok);
    }

    #[test]
    fn relative_xdg_state_home_is_ignored() {
        let mut fake = healthy();
        fake.vars
            .insert("XDG_STATE_HOME".to_owned(), "relative/state".to_owned());
        fake.dir_states.insert(
            PathBuf::from("/home/u/.local/state/execute"),
            DirState::Writable,
        );
        assert_eq!(find(&run_checks(&fake), "dirs.state").status, Status::Ok);
    }

    #[test]
    fn unwritable_state_directory_fails() {
        let mut fake = healthy();
        fake.dir_states.insert(
            PathBuf::from("/home/u/.local/state/execute"),
            DirState::NotWritable,
        );
        assert_eq!(find(&run_checks(&fake), "dirs.state").status, Status::Fail);
    }

    #[test]
    fn state_directory_that_is_a_file_fails() {
        let mut fake = healthy();
        fake.dir_states.insert(
            PathBuf::from("/home/u/.local/state/execute"),
            DirState::NotADirectory,
        );
        assert_eq!(find(&run_checks(&fake), "dirs.state").status, Status::Fail);
    }

    #[test]
    fn windows_state_directory_uses_localappdata() {
        let mut fake = healthy();
        fake.platform = Platform::Windows;
        fake.vars.insert(
            "LOCALAPPDATA".to_owned(),
            "/users/u/AppData/Local".to_owned(),
        );
        fake.dir_states.insert(
            PathBuf::from("/users/u/AppData/Local/execute"),
            DirState::Writable,
        );
        assert_eq!(find(&run_checks(&fake), "dirs.state").status, Status::Ok);

        fake.vars.remove("LOCALAPPDATA");
        assert_eq!(find(&run_checks(&fake), "dirs.state").status, Status::Fail);
    }

    #[test]
    fn cache_check_is_always_skipped() {
        let check = find(&run_checks(&healthy()), "dirs.cache").clone();
        assert_eq!(check.status, Status::Skipped);
        assert!(check.detail.contains("--cache-dir"));
    }

    #[test]
    fn desktop_checks_are_skipped_off_linux() {
        for platform in [Platform::MacOs, Platform::Windows, Platform::Other] {
            let mut fake = healthy();
            fake.platform = platform;
            let checks = run_checks(&fake);
            for id in [XDG_RUNTIME_ID, HYPRLAND_ID, WAYLAND_ID] {
                assert_eq!(
                    find(&checks, id).status,
                    Status::Skipped,
                    "{id} on {platform:?}"
                );
            }
        }
    }

    #[test]
    fn xdg_runtime_dir_fails_when_unset_or_relative() {
        let mut fake = healthy();
        fake.vars.remove("XDG_RUNTIME_DIR");
        let check = find(&run_checks(&fake), XDG_RUNTIME_ID).clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.remedy.is_some());

        fake.vars
            .insert("XDG_RUNTIME_DIR".to_owned(), "run/user/1000".to_owned());
        let check = find(&run_checks(&fake), XDG_RUNTIME_ID).clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("not an absolute path"));
    }

    #[test]
    fn hyprland_signature_warns_when_unset() {
        let mut fake = healthy();
        fake.vars.remove("HYPRLAND_INSTANCE_SIGNATURE");
        let check = find(&run_checks(&fake), HYPRLAND_ID).clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.remedy.is_some());
        assert!(!has_failures(&run_checks(&fake)));
    }

    #[test]
    fn hyprland_signature_reports_the_event_socket_when_set() {
        let check = find(&run_checks(&healthy()), HYPRLAND_ID).clone();
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("/hypr/abc/.socket2.sock"));
    }

    #[test]
    fn wayland_display_warns_when_unset_and_ok_when_set() {
        let mut fake = healthy();
        assert_eq!(find(&run_checks(&fake), WAYLAND_ID).status, Status::Ok);
        fake.vars.remove("WAYLAND_DISPLAY");
        let check = find(&run_checks(&fake), WAYLAND_ID).clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.remedy.is_some());
    }

    #[test]
    fn json_rendering_has_protocol_counts_and_null_remedies() {
        let mut fake = healthy();
        fake.kinds.remove(&bin().join("execute-applet"));
        let checks = run_checks(&fake);
        let value = render_json(&checks, VERSION);
        assert_eq!(value["protocol"], "execute.doctor/1");
        assert_eq!(value["execute_version"], VERSION);
        assert_eq!(value["ok"], false);
        assert_eq!(value["counts"]["fail"], 1);
        assert_eq!(value["counts"]["skipped"], 1);
        assert_eq!(value["counts"]["ok"], 8);
        let entries = value["checks"].as_array().expect("checks array");
        assert_eq!(entries.len(), checks.len());
        let applet = entries
            .iter()
            .find(|entry| entry["id"] == "helpers.execute-applet")
            .expect("applet entry");
        assert_eq!(applet["status"], "fail");
        assert!(applet["remedy"].is_string());
        let healthy_entry = entries
            .iter()
            .find(|entry| entry["id"] == "helpers.execute-model")
            .expect("model entry");
        assert!(healthy_entry["remedy"].is_null());
    }

    #[test]
    fn json_is_ok_when_nothing_failed() {
        let checks = run_checks(&healthy());
        assert_eq!(render_json(&checks, VERSION)["ok"], true);
    }

    #[test]
    fn text_rendering_shows_status_detail_and_fix_lines() {
        let mut fake = healthy();
        fake.kinds.remove(&bin().join("execute-applet"));
        let text = render_text(&run_checks(&fake));
        assert!(text.contains("[fail]"));
        assert!(text.contains("helpers.execute-applet"));
        assert!(text.contains("fix: reinstall so execute"));
        assert!(text.contains("[skipped]"));
        assert!(text.ends_with("1 failed, 0 warnings, 1 skipped, 8 ok\n"));
    }

    #[test]
    fn details_stay_on_one_line() {
        let check = check(
            "x.y",
            "Title",
            Status::Fail,
            "first\nsecond",
            Some("fix\r\nit"),
        );
        assert_eq!(check.detail, "first second");
        assert_eq!(check.remedy.as_deref(), Some("fix  it"));
    }
}
