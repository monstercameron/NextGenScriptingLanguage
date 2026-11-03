//! `fs.test`: typed file predicates for `if` conditions.
//!
//! Like `test(1)`, an absent path answers `false` rather than failing. Every
//! kind except `symlink` follows symbolic links to their targets, so a dangling
//! link does not `exist`; `symlink` inspects the entry itself. Other stat
//! failures (for example permission denied while searching a parent directory)
//! are reported as errors instead of being mistaken for absence.
//!
//! `socket` and `fifo` match Unix special files (always false on Windows),
//! `owned_by_me` compares the file owner against the effective user ID (Unix
//! only), `nonempty` negates `empty`, and `newer_than`/`older_than` compare
//! modification times against a second `other` path like `[[ a -nt b ]]`;
//! equal times and absent paths are both false.
//!
//! Unix `readable`, `writable` and `executable` call `access(2)` with the real
//! user ID, as `test -r`, `-w` and `-x` do. Windows has no equivalent mode bits,
//! so the approximation is: `readable` means the file opens for reading (or a
//! directory can be listed), `writable` means the read-only attribute is clear,
//! and `executable` means a regular file with an `.exe`, `.com`, `.bat` or
//! `.cmd` extension. Windows ACLs are not evaluated.

use super::filesystem::resolve_path;
use super::{invalid, io_error, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use std::fs::{self, Metadata};
use std::io;
use std::path::Path;

const KINDS: &[&str] = &[
    "exists",
    "file",
    "directory",
    "symlink",
    "readable",
    "writable",
    "executable",
    "empty",
    "socket",
    "fifo",
    "owned_by_me",
    "nonempty",
    "newer_than",
    "older_than",
];

pub(super) fn test(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    // A single stat cannot be interrupted once issued; this entry check keeps
    // an already-exceeded budget from starting the syscalls below.
    context.checkpoint()?;
    let kind = required_string(args, "kind")?;
    if !KINDS.contains(&kind) {
        return Err(invalid(format!(
            "args.kind must be one of {}",
            KINDS.join(", ")
        )));
    }
    let text = required_string(args, "path")?;
    if text.is_empty() || text.contains('\0') {
        return Err(invalid("args.path must be nonempty and contain no NUL"));
    }
    let path = resolve_path(text, cwd);
    // The comparison target for `newer_than`/`older_than` (like `[[ a -nt b ]]`).
    // Other kinds ignore it, mirroring how extra arguments are ignored elsewhere.
    let other = if kind == "newer_than" || kind == "older_than" {
        let text = required_string(args, "other")?;
        if text.is_empty() || text.contains('\0') {
            return Err(invalid("args.other must be nonempty and contain no NUL"));
        }
        Some(resolve_path(text, cwd))
    } else {
        None
    };
    // Recheck between the stat and any follow-up probe so a budget that
    // expired during the first syscall surfaces before the second one starts.
    let answer = match kind {
        "symlink" => lookup(&path, false)?.is_some_and(|metadata| metadata.is_symlink()),
        "exists" => lookup(&path, true)?.is_some(),
        "file" => lookup(&path, true)?.is_some_and(|metadata| metadata.is_file()),
        "directory" => lookup(&path, true)?.is_some_and(|metadata| metadata.is_dir()),
        "empty" => match lookup(&path, true)? {
            None => false,
            Some(metadata) if metadata.is_file() => metadata.len() == 0,
            Some(metadata) if metadata.is_dir() => {
                context.checkpoint()?;
                directory_is_empty(&path)?
            }
            Some(_) => false,
        },
        "nonempty" => match lookup(&path, true)? {
            None => false,
            Some(metadata) if metadata.is_file() => metadata.len() > 0,
            Some(metadata) if metadata.is_dir() => {
                context.checkpoint()?;
                !directory_is_empty(&path)?
            }
            Some(_) => false,
        },
        "socket" | "fifo" => match lookup(&path, true)? {
            None => false,
            Some(metadata) => {
                context.checkpoint()?;
                special_kind(&metadata.file_type(), kind)
            }
        },
        "owned_by_me" => match lookup(&path, true)? {
            None => false,
            Some(metadata) => {
                context.checkpoint()?;
                owned_by_caller(&metadata)?
            }
        },
        "newer_than" | "older_than" => {
            // Both paths are resolved above; `other` is always present here.
            let other = other.as_ref().ok_or_else(|| {
                invalid("args.other is required when kind is newer_than or older_than")
            })?;
            context.checkpoint()?;
            compare_mtime(kind, &path, other)?
        }
        _ => match lookup(&path, true)? {
            None => false,
            Some(metadata) => {
                context.checkpoint()?;
                permission_check(kind, &path, &metadata)?
            }
        },
    };
    context.checkpoint()?;
    Ok(Value::Bool(answer))
}

/// Stat `path`, returning `None` when it is absent. `NotADirectory` covers a
/// path whose intermediate component is a regular file.
fn lookup(path: &Path, follow: bool) -> Result<Option<Metadata>, AppletError> {
    let result = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    match result {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if is_absent(&error) => Ok(None),
        Err(error) => Err(io_error(error, format!("testing {}", path.display()))),
    }
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn directory_is_empty(path: &Path) -> Result<bool, AppletError> {
    let mut entries = fs::read_dir(path)
        .map_err(|error| io_error(error, format!("reading {}", path.display())))?;
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(error)) => Err(io_error(error, format!("reading {}", path.display()))),
    }
}

fn special_kind(file_type: &fs::FileType, kind: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if kind == "fifo" {
            file_type.is_fifo()
        } else {
            file_type.is_socket()
        }
    }
    #[cfg(not(unix))]
    {
        // The standard library reports no socket or FIFO file types here.
        let _ = (file_type, kind);
        false
    }
}

#[cfg(unix)]
fn owned_by_caller(metadata: &Metadata) -> Result<bool, AppletError> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` takes no arguments and only reads process state.
    Ok(metadata.uid() == unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn owned_by_caller(_metadata: &Metadata) -> Result<bool, AppletError> {
    Err(invalid(
        "args.kind \"owned_by_me\" is supported only on Unix",
    ))
}

/// Strict modification-time comparison like `[[ a -nt b ]]`: equal times and
/// absent paths are both false rather than errors.
fn compare_mtime(kind: &str, path: &Path, other: &Path) -> Result<bool, AppletError> {
    let left = lookup(path, true)?;
    let right = lookup(other, true)?;
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(false);
    };
    let left = left
        .modified()
        .map_err(|error| io_error(error, format!("testing {}", path.display())))?;
    let right = right
        .modified()
        .map_err(|error| io_error(error, format!("testing {}", other.display())))?;
    Ok(if kind == "newer_than" {
        left > right
    } else {
        left < right
    })
}

#[cfg(unix)]
fn permission_check(kind: &str, path: &Path, _metadata: &Metadata) -> Result<bool, AppletError> {
    use std::os::unix::ffi::OsStrExt;

    let mode = match kind {
        "readable" => libc::R_OK,
        "writable" => libc::W_OK,
        _ => libc::X_OK,
    };
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| invalid("args.path must contain no NUL"))?;
    // SAFETY: `c_path` is a NUL-terminated string owned by this frame and stays
    // alive for the duration of the call. `access` reads the string and does
    // not retain the pointer.
    let status = unsafe { libc::access(c_path.as_ptr(), mode) };
    if status == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code)
            if code == libc::EACCES
                || code == libc::EROFS
                || code == libc::ETXTBSY
                || code == libc::ENOENT
                || code == libc::ENOTDIR =>
        {
            Ok(false)
        }
        _ => Err(io_error(error, format!("testing {}", path.display()))),
    }
}

#[cfg(not(unix))]
fn permission_check(kind: &str, path: &Path, metadata: &Metadata) -> Result<bool, AppletError> {
    Ok(match kind {
        "readable" => {
            if metadata.is_dir() {
                fs::read_dir(path).is_ok()
            } else {
                fs::File::open(path).is_ok()
            }
        }
        "writable" => !metadata.permissions().readonly(),
        _ => {
            metadata.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        ["exe", "com", "bat", "cmd"]
                            .iter()
                            .any(|known| extension.eq_ignore_ascii_case(known))
                    })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(kind: &str, path: &Path) -> Result<Value, AppletError> {
        let mut args = json!({"kind": kind, "path": path.to_str().unwrap()});
        if kind == "newer_than" || kind == "older_than" {
            args["other"] = json!(path.to_str().unwrap());
        }
        test(
            args.as_object().unwrap(),
            Path::new("/"),
            &ExecutionContext::unrestricted(),
        )
    }

    fn check_other(kind: &str, path: &Path, other: &Path) -> Result<Value, AppletError> {
        let args =
            json!({"kind": kind, "path": path.to_str().unwrap(), "other": other.to_str().unwrap()});
        test(
            args.as_object().unwrap(),
            Path::new("/"),
            &ExecutionContext::unrestricted(),
        )
    }

    #[test]
    fn absent_paths_are_false_and_bad_kinds_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        for kind in KINDS {
            assert_eq!(check(kind, &missing).unwrap(), json!(false), "{kind}");
        }
        assert!(check("regular", &missing).is_err());
    }

    #[test]
    fn file_and_directory_kinds_follow_links_except_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        let directory = temp.path().join("dir");
        fs::create_dir(&directory).unwrap();
        assert_eq!(check("file", &file).unwrap(), json!(true));
        assert_eq!(check("directory", &file).unwrap(), json!(false));
        assert_eq!(check("directory", &directory).unwrap(), json!(true));
        assert_eq!(check("symlink", &file).unwrap(), json!(false));
        assert_eq!(check("empty", &file).unwrap(), json!(false));
        assert_eq!(check("empty", &directory).unwrap(), json!(true));
        fs::write(directory.join("child"), b"").unwrap();
        assert_eq!(check("empty", &directory).unwrap(), json!(false));
    }

    #[test]
    fn empty_file_and_permission_kinds_match_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let empty = temp.path().join("empty");
        fs::write(&empty, b"").unwrap();
        assert_eq!(check("empty", &empty).unwrap(), json!(true));
        assert_eq!(check("exists", &empty).unwrap(), json!(true));
        assert_eq!(check("readable", &empty).unwrap(), json!(true));
        assert_eq!(check("writable", &empty).unwrap(), json!(true));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_exists_only_as_a_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("dangling");
        symlink(temp.path().join("nowhere"), &link).unwrap();
        assert_eq!(check("symlink", &link).unwrap(), json!(true));
        assert_eq!(check("exists", &link).unwrap(), json!(false));
        assert_eq!(check("file", &link).unwrap(), json!(false));
    }

    #[test]
    fn nonempty_negates_empty_for_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        let empty = temp.path().join("empty.txt");
        fs::write(&empty, b"").unwrap();
        let directory = temp.path().join("dir");
        fs::create_dir(&directory).unwrap();
        assert_eq!(check("nonempty", &file).unwrap(), json!(true));
        assert_eq!(check("nonempty", &empty).unwrap(), json!(false));
        assert_eq!(check("nonempty", &directory).unwrap(), json!(false));
        fs::write(directory.join("child"), b"").unwrap();
        assert_eq!(check("nonempty", &directory).unwrap(), json!(true));
        assert_eq!(
            check("nonempty", &temp.path().join("missing")).unwrap(),
            json!(false)
        );
    }

    #[test]
    fn time_comparison_needs_both_paths_and_is_strict() {
        let temp = tempfile::tempdir().unwrap();
        let older = temp.path().join("older");
        fs::write(&older, b"old").unwrap();
        let newer = temp.path().join("newer");
        fs::write(&newer, b"new").unwrap();
        // Pinned mtimes keep the ordering exact without sleeping for clock
        // granularity.
        super::super::file_meta::set_test_mtime(&older, 1_700_000_000);
        super::super::file_meta::set_test_mtime(&newer, 1_800_000_000);
        assert_eq!(
            check_other("newer_than", &newer, &older).unwrap(),
            json!(true)
        );
        assert_eq!(
            check_other("older_than", &older, &newer).unwrap(),
            json!(true)
        );
        assert_eq!(
            check_other("newer_than", &older, &newer).unwrap(),
            json!(false)
        );
        assert_eq!(
            check_other("older_than", &newer, &older).unwrap(),
            json!(false)
        );
        assert_eq!(
            check_other("newer_than", &older, &older).unwrap(),
            json!(false)
        );
        assert_eq!(
            check_other("older_than", &older, &older).unwrap(),
            json!(false)
        );
        let missing = temp.path().join("missing");
        assert_eq!(
            check_other("newer_than", &older, &missing).unwrap(),
            json!(false)
        );
        assert_eq!(
            check_other("older_than", &missing, &older).unwrap(),
            json!(false)
        );
        let args = json!({"kind": "newer_than", "path": older.to_str().unwrap()});
        let error = test(
            args.as_object().unwrap(),
            Path::new("/"),
            &ExecutionContext::unrestricted(),
        )
        .unwrap_err();
        assert!(error.message.contains("args.other"));
    }

    #[cfg(unix)]
    #[test]
    fn special_files_and_ownership_use_unix_identity() {
        use std::os::unix::net::UnixListener;
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        assert_eq!(check("socket", &file).unwrap(), json!(false));
        assert_eq!(check("fifo", &file).unwrap(), json!(false));
        assert_eq!(check("owned_by_me", &file).unwrap(), json!(true));
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        assert_eq!(check("socket", &socket_path).unwrap(), json!(true));
        assert_eq!(check("fifo", &socket_path).unwrap(), json!(false));
        let fifo_path = temp.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(check("fifo", &fifo_path).unwrap(), json!(true));
        assert_eq!(check("socket", &fifo_path).unwrap(), json!(false));
    }

    #[cfg(not(unix))]
    #[test]
    fn special_files_are_false_and_ownership_is_unix_only_on_windows() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        assert_eq!(check("socket", &file).unwrap(), json!(false));
        assert_eq!(check("fifo", &file).unwrap(), json!(false));
        assert!(check("owned_by_me", &file).is_err());
    }
}
