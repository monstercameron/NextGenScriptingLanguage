//! Stage cargo-built helper binaries into private single-link copies for
//! trust-gated flows.
//!
//! The trusted-input gate ([`verify_trusted_input_file`]) refuses file leaves
//! with extra hardlinks (`nlink != 1`): another directory entry reaching the
//! same inode could swap what runs after the check. A release `cargo build`
//! hardlinks `target/release/<bin>` to its `deps/<bin>-<hash>` twin, and the
//! shared target cache can restore that aliasing, so pointing a manifest's
//! `binary` (or a program's `run` path) directly at `CARGO_BIN_EXE_*` inherits
//! a build-tree layout that an installed tree never has. Installed helpers are
//! real files with a single link.
//!
//! These helpers copy each cargo-built binary once per test process into a
//! privately owned temporary directory (a fresh copy always has exactly one
//! link) and hand out that path instead. The gate itself is untouched:
//! genuinely multi-linked executables are still refused, which
//! `tests/staged_helpers.rs` covers directly.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

struct Staged {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn stage(source: &Path, file_name: &str) -> Staged {
    let dir = tempfile::tempdir().expect("stage a private directory for the trusted helper copy");
    let destination = dir.path().join(file_name);
    std::fs::copy(source, &destination)
        .expect("copy the cargo-built helper into the private directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
            .expect("the staged helper must be user-executable and not group/world-writable");
        let links = std::fs::metadata(&destination)
            .expect("stat the staged helper")
            .nlink();
        assert_eq!(
            links,
            1,
            "a fresh copy of {} must have exactly one link, found {links}",
            destination.display()
        );
    }
    Staged {
        _dir: dir,
        path: destination,
    }
}

/// The staged `execute-model` helper for manifests and programs under review.
///
/// Staged once per test process so every review in the process fingerprints
/// the same executable path, exactly as a single installed helper would.
#[cfg(feature = "model")]
#[allow(dead_code)]
pub fn execute_model() -> &'static Path {
    static STAGED: OnceLock<Staged> = OnceLock::new();
    STAGED
        .get_or_init(|| {
            stage(
                Path::new(env!("CARGO_BIN_EXE_execute-model")),
                "execute-model",
            )
        })
        .path
        .as_path()
}

/// A private directory holding single-link copies of `execute`, `execute-model` and
/// `execute-applet`, and the path to the staged `execute`.
///
/// Cargo hard-links each bundled binary into `deps/`, so a spawned `execute` finds its
/// sibling helpers refused as multi-linked. Running the staged `execute` makes the
/// sibling helper lookup find single-link copies, which the trust gate accepts.
#[cfg(feature = "model")]
#[allow(dead_code)]
pub fn execute_bundle() -> &'static str {
    static BUNDLE: OnceLock<Staged> = OnceLock::new();
    BUNDLE
        .get_or_init(|| {
            let source = Path::new(env!("CARGO_BIN_EXE_execute"));
            let directory = source.parent().expect("Cargo binaries have a parent");
            let directory_stage =
                tempfile::tempdir().expect("stage a private directory for the bundle");
            let suffix = if cfg!(windows) { ".exe" } else { "" };
            let mut execute = None;
            for name in ["execute", "execute-model", "execute-applet"] {
                let file_name = format!("{name}{suffix}");
                let source = directory.join(&file_name);
                if !source.exists() {
                    continue;
                }
                let destination = directory_stage.path().join(&file_name);
                std::fs::copy(&source, &destination).expect("copy a bundled binary");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
                        .expect("make the staged binary executable");
                    assert_eq!(
                        std::fs::metadata(&destination).expect("stat").nlink(),
                        1,
                        "a fresh copy of {} must have exactly one link",
                        destination.display()
                    );
                }
                if name == "execute" {
                    execute = Some(destination);
                }
            }
            Staged {
                _dir: directory_stage,
                path: execute.expect("the execute binary is staged"),
            }
        })
        .path
        .to_str()
        .expect("the staged execute path is UTF-8")
}

/// The staged `native-fixture` helper for manifests and programs under test.
///
/// Staged once per test process for the same fingerprint-stability reason as
/// [`execute_model`].
#[cfg(feature = "test-fixtures")]
#[allow(dead_code)]
pub fn native_fixture() -> &'static Path {
    static STAGED: OnceLock<Staged> = OnceLock::new();
    STAGED
        .get_or_init(|| {
            stage(
                Path::new(env!("CARGO_BIN_EXE_native-fixture")),
                if cfg!(windows) {
                    "native-fixture.exe"
                } else {
                    "native-fixture"
                },
            )
        })
        .path
        .as_path()
}
