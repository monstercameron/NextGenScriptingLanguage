//! Portable shell-tool binaries for integration tests (#521).
//!
//! Unix returns the current spelling unchanged, so converted programs and
//! manifests stay byte-identical there and Unix coverage cannot drift.
//! Windows returns the absolute Git for Windows MSYS path (forward slashes),
//! mirroring `tests/windows_native.rs`, because bare names would resolve
//! through whatever the runner `PATH` happens to hold. CI always provides
//! Git for Windows (see the `Verify Git for Windows MSYS printf fixture`
//! step in `.github/workflows/ci.yml`); other machines get a panic that
//! names the missing tool instead of a late spawn failure.

#[cfg(windows)]
use std::path::PathBuf;

/// Portable spelling of `current` (a bare name like `"printf"` or an
/// absolute Unix path like `"/bin/sh"`) for manifest `binary` fields.
#[allow(dead_code)]
pub fn shell_binary(current: &str) -> String {
    #[cfg(unix)]
    {
        current.to_owned()
    }
    #[cfg(windows)]
    {
        let stem = current.rsplit('/').next().unwrap_or(current);
        let path = find_msys_tool(stem).unwrap_or_else(|| {
            panic!(
                "Git for Windows MSYS {stem}.exe with sibling msys-2.0.dll is required \
                 to run the shell-tool integration tests on Windows"
            )
        });
        path.to_string_lossy().replace('\\', "/")
    }
}

/// Absolute `python3` for test worker scripts: the system interpreter on
/// Unix, a `PATH` interpreter on Windows. CI always provides Python (see the
/// Windows job); other machines get a panic that names the missing tool.
#[allow(dead_code)]
pub fn python() -> String {
    #[cfg(unix)]
    {
        for candidate in ["/usr/bin/python3", "/bin/python3"] {
            if std::path::Path::new(candidate).is_file() {
                return candidate.to_owned();
            }
        }
        "python3".to_owned()
    }
    #[cfg(windows)]
    {
        for name in ["python3", "python"] {
            let exe = format!("{name}.exe");
            let found = std::env::var_os("PATH").into_iter().flat_map(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join(&exe))
                    .collect::<Vec<_>>()
            });
            if let Some(path) = found.into_iter().find(|path| path.is_file()) {
                return path.to_string_lossy().replace('\\', "/");
            }
        }
        panic!(
            "a Python 3 interpreter on PATH is required to run the worker-script \
             integration tests on Windows"
        )
    }
}

/// Graceful process interruption for test watchdogs: `SIGINT` on Unix,
/// `kill` on Windows. Unix behaviour is unchanged.
#[allow(dead_code)]
pub fn interrupt_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: kill takes only integers and touches no memory. Callers pass
        // a live child that `try_wait` reported as still running, so `id`
        // still names it.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGINT);
        }
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
    }
}

/// Graceful process termination for test watchdogs: `SIGTERM` on Unix,
/// `kill` on Windows. Unix behaviour is unchanged.
#[allow(dead_code)]
pub fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: kill takes only integers and touches no memory. Callers pass
        // a live child that `try_wait` reported as still running, so `id`
        // still names it.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
    }
}

#[cfg(windows)]
fn find_msys_tool(stem: &str) -> Option<PathBuf> {
    let exe = format!("{stem}.exe");
    let mut candidates = Vec::new();
    for root in [
        std::env::var_os("ProgramFiles"),
        std::env::var_os("ProgramFiles(x86)"),
    ]
    .into_iter()
    .flatten()
    {
        let root = PathBuf::from(root);
        candidates.push(root.join("Git/usr/bin").join(&exe));
        candidates.push(root.join("Git/bin").join(&exe));
    }
    candidates.extend(
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(&exe)),
    );
    candidates.into_iter().find(|path| {
        path.is_file()
            && path
                .parent()
                .is_some_and(|directory| directory.join("msys-2.0.dll").is_file())
    })
}
