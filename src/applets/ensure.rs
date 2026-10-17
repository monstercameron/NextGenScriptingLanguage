//! Idempotent filesystem operations: `fs.ensure_file`, `fs.ensure_line`,
//! `fs.ensure_symlink` and `fs.ensure_dir`.
//!
//! Each operation reads the current state first, without locking. When the state
//! already matches, it returns `changed: false` and writes nothing, so no lock
//! file is created either. When it must change something, a file write takes the
//! same sibling advisory lock as `fs.write_atomic` and uses its compare-and-swap
//! replace: a concurrent edit between the read and the write makes the call fail
//! with a conflict instead of silently losing that edit.
//!
//! `mode` is an octal text such as `"0644"`, so it reads like `chmod`. It is
//! enforced on Unix. On other platforms the result reports `mode_applied: false`
//! and the mode is not changed. File and line writes follow an existing symbolic
//! link to its target, as `tee` does. `fs.ensure_symlink` replaces only an
//! existing link, never a regular file or directory, and the replacement is a
//! temporary link renamed over the old one. It takes no advisory lock.

use super::filesystem::{
    atomic_replace_locked, default_lock_path, hash, resolve_path, with_path_lock,
};
use super::{invalid, io_error, required_string, AppletError};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Current bytes and Unix mode of a regular file.
struct Current {
    bytes: Vec<u8>,
    mode: Option<u32>,
}

pub(super) fn ensure_file(args: &Map<String, Value>, cwd: &Path) -> Result<Value, AppletError> {
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let content = required_string(args, "content")?;
    let mode = parse_mode(args)?;
    let current = read_current(&path)?;
    let content_matches = current
        .as_ref()
        .is_some_and(|current| current.bytes == content.as_bytes());
    let mode_matches = match (mode, current.as_ref().and_then(|current| current.mode)) {
        (Some(wanted), Some(actual)) => wanted == actual,
        _ => true,
    };
    let changed = !(content_matches && mode_matches);
    if changed {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, content.as_bytes(), &expected, mode)?;
    }
    Ok(json!({
        "changed": changed,
        "path": text,
        "mode_applied": mode.is_some() && cfg!(unix),
    }))
}

pub(super) fn ensure_line(args: &Map<String, Value>, cwd: &Path) -> Result<Value, AppletError> {
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let line = required_string(args, "line")?;
    if line.contains(['\n', '\r']) {
        return Err(invalid("args.line must be a single line without CR or LF"));
    }
    let present = match args.get("state").map(Value::as_str) {
        None => true,
        Some(Some("present")) => true,
        Some(Some("absent")) => false,
        Some(_) => return Err(invalid("args.state must be \"present\" or \"absent\"")),
    };
    let current = read_current(&path)?;
    let existing = match &current {
        Some(current) => Some(
            std::str::from_utf8(&current.bytes)
                .map_err(|error| invalid(format!("file is not UTF-8 text: {error}")))?,
        ),
        None => None,
    };
    let updated: Option<String> = match (present, existing) {
        (true, None) => Some(format!("{line}\n")),
        (true, Some(content)) => {
            if content
                .split_inclusive('\n')
                .any(|piece| line_matches(piece, line))
            {
                None
            } else {
                let mut out = content.to_owned();
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(line);
                out.push('\n');
                Some(out)
            }
        }
        (false, None) => None,
        (false, Some(content)) => {
            let kept: String = content
                .split_inclusive('\n')
                .filter(|piece| !line_matches(piece, line))
                .collect();
            (kept.len() != content.len()).then_some(kept)
        }
    };
    if let Some(content) = &updated {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, content.as_bytes(), &expected, None)?;
    }
    Ok(json!({"changed": updated.is_some(), "path": text}))
}

pub(super) fn ensure_symlink(args: &Map<String, Value>, cwd: &Path) -> Result<Value, AppletError> {
    let text = nonempty_path(args)?;
    let link = resolve_path(text, cwd);
    let target = required_string(args, "target")?;
    if target.is_empty() || target.contains('\0') {
        return Err(invalid("args.target must be nonempty and contain no NUL"));
    }
    let existing = match fs::symlink_metadata(&link) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(error, format!("checking {}", link.display()))),
    };
    // The previous target lets a caller roll the link back; it is empty when no
    // link existed before this call.
    let (changed, previous) = match existing {
        Some(metadata) if metadata.file_type().is_symlink() => {
            let current = fs::read_link(&link)
                .map_err(|error| io_error(error, format!("reading {}", link.display())))?;
            let previous = current.to_string_lossy().into_owned();
            if current == Path::new(target) {
                (false, previous)
            } else {
                let temp = temporary_link_path(&link);
                make_link(target, &temp, &link)?;
                if let Err(error) = fs::rename(&temp, &link) {
                    let _ = fs::remove_file(&temp);
                    return Err(io_error(
                        error,
                        format!("replacing link {}", link.display()),
                    ));
                }
                (true, previous)
            }
        }
        Some(_) => {
            return Err(invalid(format!(
                "refusing to replace a non-link path: {}",
                link.display()
            )))
        }
        None => {
            make_link(target, &link, &link)?;
            (true, String::new())
        }
    };
    Ok(json!({
        "changed": changed,
        "path": text,
        "target": target,
        "previous": previous,
    }))
}

pub(super) fn ensure_dir(args: &Map<String, Value>, cwd: &Path) -> Result<Value, AppletError> {
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let mode = parse_mode(args)?;
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
    };
    let changed = match metadata {
        Some(metadata) if metadata.is_dir() => match (mode, file_mode(&metadata)) {
            (Some(wanted), Some(actual)) if wanted != actual => {
                set_mode(&path, wanted)?;
                true
            }
            _ => false,
        },
        Some(_) => {
            return Err(invalid(format!(
                "path exists and is not a directory: {}",
                path.display()
            )))
        }
        None => {
            fs::create_dir_all(&path)
                .map_err(|error| io_error(error, format!("creating {}", path.display())))?;
            if let Some(wanted) = mode {
                set_mode(&path, wanted)?;
            }
            true
        }
    };
    Ok(json!({
        "changed": changed,
        "path": text,
        "mode_applied": mode.is_some() && cfg!(unix),
    }))
}

fn nonempty_path(args: &Map<String, Value>) -> Result<&str, AppletError> {
    let text = required_string(args, "path")?;
    if text.is_empty() || text.contains('\0') {
        return Err(invalid("args.path must be nonempty and contain no NUL"));
    }
    Ok(text)
}

/// Parse a permission mode written as octal text, such as `"0644"` or `"600"`.
fn parse_mode(args: &Map<String, Value>) -> Result<Option<u32>, AppletError> {
    let Some(value) = args.get("mode") else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| invalid("args.mode must be octal text such as \"0644\""))?;
    if text.is_empty() || text.len() > 4 || !text.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
    {
        return Err(invalid(format!(
            "args.mode {text:?} must be 1 to 4 octal digits"
        )));
    }
    u32::from_str_radix(text, 8)
        .map(Some)
        .map_err(|_| invalid(format!("args.mode {text:?} is not an octal mode")))
}

fn read_current(path: &Path) -> Result<Option<Current>, AppletError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
    };
    if !metadata.is_file() {
        return Err(invalid(format!(
            "path is not a regular file: {}",
            path.display()
        )));
    }
    let file = fs::File::open(path)
        .map_err(|error| io_error(error, format!("opening {}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(error, format!("reading {}", path.display())))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(invalid(format!(
            "file exceeds the {MAX_FILE_BYTES}-byte limit: {}",
            path.display()
        )));
    }
    Ok(Some(Current {
        bytes,
        mode: file_mode(&metadata),
    }))
}

fn expected_hash(current: Option<&Current>) -> String {
    current.map_or_else(|| "missing".to_owned(), |current| hash(&current.bytes))
}

/// Replace a file under the sibling advisory lock, expecting `expected`.
fn write_locked(
    path: &Path,
    content: &[u8],
    expected: &str,
    mode: Option<u32>,
) -> Result<(), AppletError> {
    let lock = default_lock_path(path)?;
    with_path_lock(&lock, LOCK_TIMEOUT, || {
        let (policy, mode) = match mode {
            Some(mode) if cfg!(unix) => ("exact", Some(mode)),
            _ => ("preserve", None),
        };
        atomic_replace_locked(path, content, expected, "preserve", policy, mode).map(|_| ())
    })
}

/// A line matches when its text, without its line break, equals `line`.
fn line_matches(piece: &str, line: &str) -> bool {
    let body = piece.strip_suffix('\n').unwrap_or(piece);
    let body = body.strip_suffix('\r').unwrap_or(body);
    body == line
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), AppletError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| io_error(error, format!("setting mode on {}", path.display())))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), AppletError> {
    Ok(())
}

fn temporary_link_path(link: &Path) -> PathBuf {
    let sequence = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let name = link
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    link.with_file_name(format!(
        ".{name}.execute-applet.{}.{sequence}.tmp",
        std::process::id()
    ))
}

/// Create a symbolic link at `at`. `link` locates the link for relative target
/// resolution on Windows, where the kind of target must be chosen up front.
fn make_link(target: &str, at: &Path, link: &Path) -> Result<(), AppletError> {
    let result = create_symlink(target, at, link);
    result.map_err(|error| io_error(error, format!("creating link {}", at.display())))
}

#[cfg(unix)]
fn create_symlink(target: &str, at: &Path, _link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(windows)]
fn create_symlink(target: &str, at: &Path, link: &Path) -> std::io::Result<()> {
    let resolved = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        link.parent().unwrap_or(Path::new(".")).join(target)
    };
    if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, at)
    } else {
        std::os::windows::fs::symlink_file(target, at)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &str, _at: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "symbolic links are not supported on this platform",
    ))
}
