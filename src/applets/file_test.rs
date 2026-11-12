//! `fs.test`: typed file predicates for `if` conditions.
//!
//! Like `test(1)`, an absent path answers `false` rather than failing. Every
//! kind except `symlink` follows symbolic links to their targets, so a dangling
//! link does not `exist`; `symlink` inspects the entry itself. Other stat
//! failures (for example permission denied while searching a parent directory)
//! are reported as errors instead of being mistaken for absence.
//!
//! Unix `readable`, `writable` and `executable` call `access(2)` with the real
//! user ID, as `test -r`, `-w` and `-x` do. Windows has no equivalent mode bits,
//! so the approximation is: `readable` means the file opens for reading (or a
//! directory can be listed), `writable` means the read-only attribute is clear,
//! and `executable` means a regular file with an `.exe`, `.com`, `.bat` or
//! `.cmd` extension. Windows ACLs are not evaluated.

use super::filesystem::resolve_path;
use super::{invalid, io_error, required_string, AppletError, ExecutionContext};
use crate::terminal_text::{stream_facts, StreamFacts};
use serde_json::{json, Map, Value};
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

/// `terminal.info`: whether the standard streams are terminals, the terminal size when the
/// OS reports one, and the `NO_COLOR` and `TERM` environment values, as one JSON record.
pub(super) fn terminal_info(
    _args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    Ok(terminal_record(&stream_facts(), &|name| {
        std::env::var(name).ok()
    }))
}

/// Build the `terminal.info` record from injected stream facts and an injected environment
/// lookup, so the result does not depend on the real terminal. `NO_COLOR` counts as set only
/// when non-empty, as the no-color.org convention states. Unknown size reports `null`.
fn terminal_record(streams: &StreamFacts, environment: &dyn Fn(&str) -> Option<String>) -> Value {
    json!({
        "stdin_tty": streams.stdin_tty,
        "stdout_tty": streams.stdout_tty,
        "stderr_tty": streams.stderr_tty,
        "columns": streams.size.map(|(columns, _)| columns),
        "rows": streams.size.map(|(_, rows)| rows),
        "no_color": environment("NO_COLOR").is_some_and(|value| !value.is_empty()),
        "term": environment("TERM"),
    })
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
        let args = json!({"kind": kind, "path": path.to_str().unwrap()});
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

    fn no_environment(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn piped_stdout_reports_not_a_terminal() {
        let piped = StreamFacts {
            stdin_tty: false,
            stdout_tty: false,
            stderr_tty: true,
            size: None,
        };
        let record = terminal_record(&piped, &no_environment);
        assert_eq!(record["stdout_tty"], json!(false));
        assert_eq!(record["stdin_tty"], json!(false));
        assert_eq!(record["stderr_tty"], json!(true));
        assert_eq!(record["columns"], Value::Null);
        assert_eq!(record["rows"], Value::Null);
        assert_eq!(record["no_color"], json!(false));
        assert_eq!(record["term"], Value::Null);
    }

    #[test]
    fn no_color_set_in_the_environment_is_reported() {
        let streams = StreamFacts {
            stdin_tty: true,
            stdout_tty: true,
            stderr_tty: true,
            size: Some((120, 40)),
        };
        let set = |name: &str| (name == "NO_COLOR").then(|| "1".to_string());
        assert_eq!(terminal_record(&streams, &set)["no_color"], json!(true));
        let empty = |name: &str| (name == "NO_COLOR").then(String::new);
        assert_eq!(terminal_record(&streams, &empty)["no_color"], json!(false));
        assert_eq!(
            terminal_record(&streams, &no_environment)["no_color"],
            json!(false)
        );
    }

    #[test]
    fn known_size_and_term_are_reported() {
        let streams = StreamFacts {
            stdin_tty: true,
            stdout_tty: true,
            stderr_tty: true,
            size: Some((120, 40)),
        };
        let term = |name: &str| (name == "TERM").then(|| "xterm-256color".to_string());
        let record = terminal_record(&streams, &term);
        assert_eq!(record["columns"], json!(120));
        assert_eq!(record["rows"], json!(40));
        assert_eq!(record["term"], json!("xterm-256color"));
    }

    #[test]
    fn terminal_info_reports_every_field_for_the_real_process() {
        let record = terminal_info(&Map::new(), &ExecutionContext::unrestricted()).unwrap();
        for field in [
            "stdin_tty",
            "stdout_tty",
            "stderr_tty",
            "columns",
            "rows",
            "no_color",
            "term",
        ] {
            assert!(record.get(field).is_some(), "missing {field}");
        }
    }
}
