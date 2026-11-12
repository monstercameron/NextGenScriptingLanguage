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
//! temporary link renamed over the old one. That rename runs under the same
//! sibling advisory lock as `fs.write_atomic`, held across a recheck that the
//! path is still a link, so a cooperating applet cannot install a regular
//! file between the check and the rename. The lock is advisory only: writers
//! that do not take it are not excluded.

use super::filesystem::{
    atomic_replace_locked, default_lock_path, hash, resolve_path, with_path_lock,
};
use super::transaction::WriteJournal;
use super::{
    invalid, io_error, optional_bool, optional_string, required_string, AppletError,
    ExecutionContext,
};
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

pub(super) fn ensure_file(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let content = required_string(args, "content")?;
    let mode = parse_mode(args)?;
    let current = read_current(&path, context)?;
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
        write_locked(&path, content.as_bytes(), &expected, mode, None, context)?;
    }
    Ok(json!({
        "changed": changed,
        "path": text,
        "mode_applied": mode.is_some() && cfg!(unix),
    }))
}

pub(super) fn ensure_line(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
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
    let current = read_current(&path, context)?;
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
                out.push_str(line_ending(content));
                Some(out)
            }
        }
        (false, None) => None,
        (false, Some(content)) => {
            let kept: String = content
                .split_inclusive('\n')
                .filter_map(|piece| {
                    if line_matches(piece, line) {
                        piece.starts_with('\u{feff}').then_some("\u{feff}")
                    } else {
                        Some(piece)
                    }
                })
                .collect();
            (kept.len() != content.len()).then_some(kept)
        }
    };
    if let Some(content) = &updated {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, content.as_bytes(), &expected, None, None, context)?;
    }
    Ok(json!({"changed": updated.is_some(), "path": text}))
}

/// `conf.set`: set one `key` to `value` in a key/value config file, changing the
/// file only when that line must change. Reads and writes exactly as
/// `fs.ensure_line` does, so an absent file is created and a concurrent edit
/// fails with a conflict.
pub(super) fn conf_set(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let key = required_string(args, "key")?;
    let value = required_string(args, "value")?;
    let style = conf_style(args)?;
    let current = read_current(&path, context)?;
    let existing = match &current {
        Some(current) => std::str::from_utf8(&current.bytes)
            .map_err(|error| invalid(format!("file is not UTF-8 text: {error}")))?,
        None => "",
    };
    let updated = set_key_text(existing, key, value, style).map_err(invalid)?;
    if let Some(content) = &updated {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, content.as_bytes(), &expected, None, None, context)?;
    }
    Ok(json!({"changed": updated.is_some(), "path": text}))
}

/// `conf.ensure_block`: make the managed block `id` hold exactly `content`,
/// appending the block when the file has none. Only that block changes. Reads
/// and writes as `conf.set` does, so a missing file is created, and a malformed
/// block structure is refused before anything is written.
pub(super) fn conf_ensure_block(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let id = required_string(args, "id")?;
    let content = required_string(args, "content")?;
    let comment = conf_comment(args)?;
    let current = read_current(&path, context)?;
    let existing = match &current {
        Some(current) => std::str::from_utf8(&current.bytes)
            .map_err(|error| invalid(format!("file is not UTF-8 text: {error}")))?,
        None => "",
    };
    let updated = ensure_block_text(existing, id, content, comment).map_err(invalid)?;
    if let Some(new_text) = &updated {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, new_text.as_bytes(), &expected, None, None, context)?;
    }
    Ok(json!({"changed": updated.is_some(), "path": text}))
}

/// `conf.remove_block`: remove the managed block `id` and at most one blank line
/// that separated it. A missing file or a file without that block is left
/// untouched and reports `changed: false`.
pub(super) fn conf_remove_block(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let id = required_string(args, "id")?;
    let comment = conf_comment(args)?;
    let current = read_current(&path, context)?;
    let existing = match &current {
        Some(current) => std::str::from_utf8(&current.bytes)
            .map_err(|error| invalid(format!("file is not UTF-8 text: {error}")))?,
        None => "",
    };
    let updated = remove_block_text(existing, id, comment).map_err(invalid)?;
    if let Some(new_text) = &updated {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, new_text.as_bytes(), &expected, None, None, context)?;
    }
    Ok(json!({"changed": updated.is_some(), "path": text}))
}

/// `conf.ensure_line_near`: make sure the whole line `line` is present exactly
/// once, inserting it `before` or `after` the first line equal to `anchor` when
/// it is absent. A missing anchor is an error, never an append. Reads and writes
/// as `conf.set` does.
pub(super) fn conf_ensure_line_near(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let line = required_string(args, "line")?;
    let anchor = required_string(args, "anchor")?;
    let placement = match required_string(args, "position")? {
        "before" => Placement::Before,
        "after" => Placement::After,
        _ => return Err(invalid("args.position must be \"before\" or \"after\"")),
    };
    let current = read_current(&path, context)?;
    let existing = match &current {
        Some(current) => std::str::from_utf8(&current.bytes)
            .map_err(|error| invalid(format!("file is not UTF-8 text: {error}")))?,
        None => "",
    };
    let updated = ensure_line_near_text(existing, line, anchor, placement).map_err(invalid)?;
    if let Some(new_text) = &updated {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, new_text.as_bytes(), &expected, None, None, context)?;
    }
    Ok(json!({"changed": updated.is_some(), "path": text}))
}

/// `conf.remove_matching`: remove every line that equals `line` as a whole line
/// (`match` = "line", the default) or starts with it (`match` = "prefix"). Lines
/// inside a managed block are left alone. Reads and writes as `conf.set` does;
/// a missing file is left untouched and reports `changed: false`.
pub(super) fn conf_remove_matching(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let line = required_string(args, "line")?;
    let mode = match optional_string(args, "match", "line")? {
        "line" => MatchMode::Line,
        "prefix" => MatchMode::Prefix,
        _ => return Err(invalid("args.match must be \"line\" or \"prefix\"")),
    };
    let current = read_current(&path, context)?;
    let existing = match &current {
        Some(current) => std::str::from_utf8(&current.bytes)
            .map_err(|error| invalid(format!("file is not UTF-8 text: {error}")))?,
        None => "",
    };
    let updated = remove_matching_text(existing, line, mode).map_err(invalid)?;
    if let Some(new_text) = &updated {
        let expected = expected_hash(current.as_ref());
        write_locked(&path, new_text.as_bytes(), &expected, None, None, context)?;
    }
    Ok(json!({"changed": updated.is_some(), "path": text}))
}

/// Read the optional `comment` marker shared by `conf.set` and the block applets.
fn conf_comment(args: &Map<String, Value>) -> Result<&'static str, AppletError> {
    match optional_string(args, "comment", "#")? {
        "#" => Ok("#"),
        ";" => Ok(";"),
        "//" => Ok("//"),
        "--" => Ok("--"),
        _ => Err(invalid(
            "args.comment must be \"#\", \";\", \"//\" or \"--\"",
        )),
    }
}

/// Read the optional `conf.set` style fields. The text forms map onto the
/// static strings that [`ConfStyle`] holds, so any other value is refused here.
fn conf_style(args: &Map<String, Value>) -> Result<ConfStyle, AppletError> {
    let separator = match optional_string(args, "separator", "=")? {
        "=" => "=",
        ":" => ":",
        " " => " ",
        _ => return Err(invalid("args.separator must be \"=\", \":\" or \" \"")),
    };
    let comment = conf_comment(args)?;
    Ok(ConfStyle {
        separator,
        comment,
        spaced: optional_bool(args, "spaced", false)?,
        uncomment: optional_bool(args, "uncomment", false)?,
    })
}

pub(super) fn ensure_symlink(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
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
    // link existed before this call. Each single link or rename call below can
    // still block inside the operating system; only the slices between calls
    // are interruptible.
    context.checkpoint()?;
    let (changed, previous) = match existing {
        Some(metadata) if metadata.file_type().is_symlink() => {
            context.checkpoint()?;
            let current = fs::read_link(&link)
                .map_err(|error| io_error(error, format!("reading {}", link.display())))?;
            // `previous` must restore this link exactly, as the stored target
            // text. A lossy conversion would point a rollback at a different
            // path, so refuse to replace a target it cannot represent.
            let previous = match current.to_str() {
                Some(text) => text.to_owned(),
                None if current == Path::new(target) => current.to_string_lossy().into_owned(),
                None => {
                    return Err(invalid(format!(
                        "refusing to replace symlink {}: its current target is not UTF-8 \
                         text, so `previous` could not restore it exactly",
                        link.display()
                    )))
                }
            };
            if current == Path::new(target) {
                (false, previous)
            } else {
                check_previous_restores_link_kind(&metadata, &link, &current)?;
                context.checkpoint()?;
                let temp = temporary_link_path(&link);
                let mut temp_cleanup = TempLinkCleanup(Some(temp.clone()));
                make_link(target, &temp, &link)?;
                // Hold the sibling lock `fs.write_atomic` takes across the
                // recheck and the rename (#723). Without it a cooperating
                // caller could install a regular file after the recheck and
                // the rename would replace it. On any error the cleanup
                // guard removes the temporary link.
                context.checkpoint()?;
                let lock = default_lock_path(&link)?;
                with_path_lock(&lock, LOCK_TIMEOUT, context, || {
                    context.checkpoint()?;
                    // Recheck immediately before the rename: a regular file
                    // or directory that appeared since the first check must
                    // not be replaced.
                    if let Ok(current) = fs::symlink_metadata(&link) {
                        if !current.file_type().is_symlink() {
                            return Err(io_error(
                                std::io::Error::new(
                                    ErrorKind::AlreadyExists,
                                    "link path is no longer a symbolic link",
                                ),
                                format!("replacing link {}", link.display()),
                            ));
                        }
                    }
                    fs::rename(&temp, &link).map_err(|error| {
                        io_error(error, format!("replacing link {}", link.display()))
                    })
                })?;
                temp_cleanup.done();
                context.checkpoint()?;
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
            context.checkpoint()?;
            make_link(target, &link, &link)?;
            context.checkpoint()?;
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

pub(super) fn ensure_dir(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = nonempty_path(args)?;
    let path = resolve_path(text, cwd);
    let mode = parse_mode(args)?;
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
    };
    // Each single create or mode call below can still block inside the
    // operating system; only the slices between calls are interruptible.
    context.checkpoint()?;
    let changed = match metadata {
        Some(metadata) if metadata.is_dir() => match (mode, file_mode(&metadata)) {
            (Some(wanted), Some(actual)) if wanted != actual => {
                context.checkpoint()?;
                set_mode(&path, wanted)?;
                context.checkpoint()?;
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
            context.checkpoint()?;
            fs::create_dir_all(&path)
                .map_err(|error| io_error(error, format!("creating {}", path.display())))?;
            context.checkpoint()?;
            if let Some(wanted) = mode {
                set_mode(&path, wanted)?;
            }
            context.checkpoint()?;
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

fn read_current(
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Option<Current>, AppletError> {
    context.checkpoint()?;
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
    // Re-open through the bounded helper: the metadata check above follows
    // the path, so a FIFO swapped in before the open must not hang it. The
    // handle check refuses anything that is not a regular file.
    let mut file = super::open_regular_input(path, path, false, context)?;
    let mut bytes = Vec::new();
    read_chunked(&mut file, &mut bytes, path, context)?;
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

/// Replace a file under the sibling advisory lock, expecting `expected`. With a
/// journal, the prior state is recorded before the replacement (#393).
fn write_locked(
    path: &Path,
    content: &[u8],
    expected: &str,
    mode: Option<u32>,
    journal: Option<&mut WriteJournal>,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    let lock = default_lock_path(path)?;
    with_path_lock(&lock, LOCK_TIMEOUT, context, || {
        let (policy, mode) = match mode {
            Some(mode) if cfg!(unix) => ("exact", Some(mode)),
            _ => ("preserve", None),
        };
        match journal {
            Some(journal) => journal.replace(path, content, expected, policy, mode, context),
            None => {
                atomic_replace_locked(path, content, expected, "preserve", policy, mode, context)
                    .map(|_| ())
            }
        }
    })
}

/// Copy at most `MAX_FILE_BYTES + 1` bytes from `file` in chunks, checking
/// the execution budget between chunks. Each individual `read` call can
/// still block on a stalled filesystem; only the slices between calls are
/// interruptible.
fn read_chunked(
    file: &mut fs::File,
    bytes: &mut Vec<u8>,
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    const CHUNK_BYTES: usize = 64 * 1024;
    let limit = MAX_FILE_BYTES + 1;
    let mut chunk = vec![0u8; CHUNK_BYTES];
    loop {
        context.checkpoint()?;
        let end = bytes.len().saturating_add(CHUNK_BYTES).min(limit);
        let want = end.saturating_sub(bytes.len());
        if want == 0 {
            break;
        }
        let read = file
            .read(&mut chunk[..want])
            .map_err(|error| io_error(error, format!("reading {}", path.display())))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() >= limit {
            break;
        }
    }
    Ok(())
}

/// A line matches when its text, without its line break, equals `line`.
fn line_matches(piece: &str, line: &str) -> bool {
    line_body(piece) == line
}

/// The text of one line without its line ending and without a leading byte
/// order mark (only the first line of a file can carry one).
fn line_body(piece: &str) -> &str {
    let body = piece.strip_suffix('\n').unwrap_or(piece);
    let body = body.strip_suffix('\r').unwrap_or(body);
    if piece.starts_with('\u{feff}') {
        body.strip_prefix('\u{feff}').unwrap_or(body)
    } else {
        body
    }
}

fn line_ending(content: &str) -> &'static str {
    match content.find('\n') {
        Some(index) if index > 0 && content.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
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
    let mut name = super::filesystem::sibling_prefix(link.file_name().unwrap_or_default());
    name.push(format!(".{}.{sequence}.tmp", std::process::id()));
    link.with_file_name(name)
}

/// Remove a half-replaced symlink temp when a later checkpoint or rename
/// fails. Disarm after the rename lands so the live link is never touched.
struct TempLinkCleanup(Option<PathBuf>);

impl TempLinkCleanup {
    fn done(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempLinkCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Refuse to replace a link whose kind a rollback through `previous` would
/// not recreate. Windows chooses between file and directory links by whether
/// the target currently resolves as a directory, so a dangling directory
/// link would come back as a file link; its original kind is read from the
/// link's own attributes, which survive a missing target. Unix links carry
/// no kind, so the same stored target always recreates the same link there.
#[cfg(windows)]
fn check_previous_restores_link_kind(
    metadata: &fs::Metadata,
    link: &Path,
    current: &Path,
) -> Result<(), AppletError> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    let was_directory = metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0;
    let resolved = if current.is_absolute() {
        current.to_path_buf()
    } else {
        link.parent().unwrap_or(Path::new(".")).join(current)
    };
    if was_directory == resolved.is_dir() {
        return Ok(());
    }
    let (kind, wanted) = if was_directory {
        ("directory", "a directory")
    } else {
        ("file", "a file")
    };
    Err(invalid(format!(
        "refusing to replace symlink {}: it is a {kind} link, but its target does not \
         currently resolve as {wanted}, so `previous` could not restore its link kind",
        link.display()
    )))
}

#[cfg(not(windows))]
fn check_previous_restores_link_kind(
    _metadata: &fs::Metadata,
    _link: &Path,
    _current: &Path,
) -> Result<(), AppletError> {
    Ok(())
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

/// Dialect of a line-oriented config file for [`set_key_text`].
#[derive(Clone, Copy)]
struct ConfStyle {
    /// `"="`, `":"` or `" "` (one or more blanks).
    separator: &'static str,
    /// Line comment marker: `"#"`, `";"`, `"//"` or `"--"`.
    comment: &'static str,
    /// Append `key = value` rather than `key=value`. Ignored for `" "`.
    spaced: bool,
    /// When no live `key` line exists, rewrite the first commented `key` line in
    /// place instead of appending a new line.
    uncomment: bool,
}

/// Set `key` to `value` in `text`, touching only the line that must change.
///
/// Returns `Ok(None)` when the text already sets `key` to `value`, so the caller
/// writes nothing. Otherwise returns the new text, in which every line other than
/// the edited one (comments and blank lines included) keeps its exact bytes. The
/// edited line keeps its indentation, key, separator spacing, line ending and a
/// leading byte order mark. Its value runs to the end of the line, so an inline
/// comment after the value on that same line is replaced with it.
///
/// Only a live line counts as setting `key`: a commented `# key = ...` line never
/// matches unless `uncomment` asks for it. Appends go to the end of the text, so
/// the function does not know about sections.
fn set_key_text(
    text: &str,
    key: &str,
    value: &str,
    style: ConfStyle,
) -> Result<Option<String>, String> {
    check_style(style)?;
    check_key_and_value(key, value, style)?;
    let pieces: Vec<&str> = text.split_inclusive('\n').collect();

    for (index, piece) in pieces.iter().enumerate() {
        let (bom, body, eol) = split_piece(piece);
        if let Some(found) = assignment(body, key, style) {
            if found.value == value {
                return Ok(None);
            }
            let line = format!("{bom}{}{value}{eol}", found.head);
            return Ok(Some(replace_piece(&pieces, index, &line)));
        }
    }

    if style.uncomment {
        for (index, piece) in pieces.iter().enumerate() {
            let (bom, body, eol) = split_piece(piece);
            let Some((indent, rest)) = commented(body, style) else {
                continue;
            };
            if let Some(found) = assignment(rest, key, style) {
                let line = format!("{bom}{indent}{}{value}{eol}", found.head.trim_start());
                return Ok(Some(replace_piece(&pieces, index, &line)));
            }
        }
    }

    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + key.len() + value.len() + 8);
    out.push_str(text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(key);
    if style.separator == " " {
        out.push(' ');
    } else if style.separator == "=" && style.spaced {
        out.push_str(" = ");
    } else {
        out.push_str(style.separator);
        if style.spaced {
            out.push(' ');
        }
    }
    out.push_str(value);
    out.push_str(eol);
    Ok(Some(out))
}

fn check_style(style: ConfStyle) -> Result<(), String> {
    if !matches!(style.separator, "=" | ":" | " ") {
        return Err(format!(
            "separator {:?} must be \"=\", \":\" or \" \"",
            style.separator
        ));
    }
    if !matches!(style.comment, "#" | ";" | "//" | "--") {
        return Err(format!(
            "comment {:?} must be \"#\", \";\", \"//\" or \"--\"",
            style.comment
        ));
    }
    Ok(())
}

fn check_key_and_value(key: &str, value: &str, style: ConfStyle) -> Result<(), String> {
    if key.is_empty()
        || key.contains(|c: char| c.is_whitespace() || c == '\0' || "=:#;".contains(c))
        || key.contains(style.comment)
    {
        return Err(format!(
            "key {key:?} must be nonempty and contain no blank, NUL, \"=\", \":\", \"#\", \";\" \
             or the comment marker"
        ));
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err("value must be a single line without CR, LF or NUL".to_owned());
    }
    Ok(())
}

/// Split one line (with its line terminator, if any) into its byte order mark,
/// its body and its line ending.
fn split_piece(piece: &str) -> (&'static str, &str, &'static str) {
    let (body, eol) = if let Some(rest) = piece.strip_suffix("\r\n") {
        (rest, "\r\n")
    } else if let Some(rest) = piece.strip_suffix('\n') {
        (rest, "\n")
    } else {
        (piece, "")
    };
    match body.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest, eol),
        None => ("", body, eol),
    }
}

/// A `key SEP value` line. `head` is the text up to the value (indentation, key,
/// separator and its spacing); `value` is the value without trailing blanks.
struct Assignment<'a> {
    head: &'a str,
    value: &'a str,
}

fn assignment<'a>(body: &'a str, key: &str, style: ConfStyle) -> Option<Assignment<'a>> {
    let blank = [' ', '\t'];
    let after_indent = body.trim_start_matches(blank);
    let after_key = after_indent.strip_prefix(key)?;
    let after_gap = after_key.trim_start_matches(blank);
    let after_sep = if style.separator == " " {
        // A whitespace separator must be present, or `font-size` would match `font`.
        if after_gap.len() == after_key.len() {
            return None;
        }
        after_gap
    } else {
        after_gap.strip_prefix(style.separator)?
    };
    let value_start = after_sep.trim_start_matches(blank);
    let head = &body[..body.len() - value_start.len()];
    Some(Assignment {
        head,
        value: value_start.trim_end_matches(blank),
    })
}

/// A commented line: the indentation before its comment marker, and the text
/// after the marker.
fn commented(body: &str, style: ConfStyle) -> Option<(&str, &str)> {
    let after_indent = body.trim_start_matches([' ', '\t']);
    let rest = after_indent.strip_prefix(style.comment)?;
    Some((&body[..body.len() - after_indent.len()], rest))
}

fn replace_piece(pieces: &[&str], index: usize, line: &str) -> String {
    let mut out = String::new();
    for (position, piece) in pieces.iter().enumerate() {
        out.push_str(if position == index { line } else { piece });
    }
    out
}

/// Longest managed block id, in bytes.
const BLOCK_ID_MAX_BYTES: usize = 200;

/// Reject a comment marker outside the four forms `conf.set` accepts.
fn check_block_comment(comment: &str) -> Result<(), String> {
    if matches!(comment, "#" | ";" | "//" | "--") {
        Ok(())
    } else {
        Err(format!(
            "comment {comment:?} must be \"#\", \";\", \"//\" or \"--\""
        ))
    }
}

/// A block id is 1 to `BLOCK_ID_MAX_BYTES` bytes of printable ASCII that never
/// contains the comment marker, so it cannot break or forge a marker line.
fn check_block_id(id: &str, comment: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > BLOCK_ID_MAX_BYTES {
        return Err(format!("block id must be 1 to {BLOCK_ID_MAX_BYTES} bytes"));
    }
    if !id.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err(format!("block id {id:?} must be printable ASCII"));
    }
    if id.contains(comment) {
        return Err(format!(
            "block id {id:?} must not contain the comment marker {comment:?}"
        ));
    }
    Ok(())
}

/// The text that starts a begin (`word` = "BEGIN") or end (`word` = "END")
/// marker line. A full marker is this prefix followed by the block id.
fn block_prefix(comment: &str, word: &str) -> String {
    format!("{comment} {word} execute managed block: ")
}

/// A line without its line ending (`\n` or `\r\n`).
fn line_text(piece: &str) -> &str {
    match piece.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => piece,
    }
}

/// The text of line `index` without its line ending. Only the first line can
/// carry a byte order mark, so only that line has it removed.
fn block_body(index: usize, piece: &str) -> &str {
    let body = line_text(piece);
    if index == 0 {
        body.strip_prefix('\u{feff}').unwrap_or(body)
    } else {
        body
    }
}

/// The body lines for a block. A body line may not carry a bare CR or a NUL,
/// and may not start like a begin or end marker of any block, or it would
/// change the block structure.
fn block_lines<'a>(body: &'a str, comment: &str) -> Result<Vec<&'a str>, String> {
    if body.contains('\0') {
        return Err("block content must not contain NUL".to_owned());
    }
    let begin_prefix = block_prefix(comment, "BEGIN");
    let end_prefix = block_prefix(comment, "END");
    let lines: Vec<&str> = body.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.contains('\r') {
            return Err(format!(
                "block content line {} contains a bare carriage return",
                index + 1
            ));
        }
        if line.starts_with(&begin_prefix) || line.starts_with(&end_prefix) {
            return Err(format!(
                "block content line {} is a managed block marker",
                index + 1
            ));
        }
    }
    Ok(lines)
}

/// A complete managed block: the piece indexes of its begin and end markers.
struct ManagedBlock<'a> {
    id: &'a str,
    begin: usize,
    end: usize,
}

/// Find every managed block in `pieces`. A malformed structure (an unclosed
/// begin, an end without a begin, an end that names another block, a nested
/// block, or a repeated id) is an error that names the line.
fn find_managed_blocks<'a>(
    pieces: &[&'a str],
    comment: &str,
) -> Result<Vec<ManagedBlock<'a>>, String> {
    let begin_prefix = block_prefix(comment, "BEGIN");
    let end_prefix = block_prefix(comment, "END");
    let mut blocks: Vec<ManagedBlock<'a>> = Vec::new();
    let mut open: Option<(usize, &str)> = None;
    for (index, &piece) in pieces.iter().enumerate() {
        let line = index + 1;
        let body = block_body(index, piece);
        if let Some(id) = body.strip_prefix(begin_prefix.as_str()) {
            if let Some((opened, open_id)) = open {
                return Err(format!(
                    "line {line}: block {id:?} begins inside block {open_id:?} opened at \
                     line {}; managed blocks cannot nest",
                    opened + 1
                ));
            }
            if blocks.iter().any(|block| block.id == id) {
                return Err(format!(
                    "line {line}: block {id:?} is already defined earlier in this file"
                ));
            }
            open = Some((index, id));
        } else if let Some(id) = body.strip_prefix(end_prefix.as_str()) {
            match open.take() {
                Some((begin, open_id)) if open_id == id => {
                    blocks.push(ManagedBlock {
                        id,
                        begin,
                        end: index,
                    });
                }
                Some((begin, open_id)) => {
                    return Err(format!(
                        "line {line}: end marker for block {id:?} does not match block \
                         {open_id:?} begun at line {}",
                        begin + 1
                    ))
                }
                None => {
                    return Err(format!(
                        "line {line}: end marker for block {id:?} has no begin marker"
                    ))
                }
            }
        }
    }
    if let Some((begin, id)) = open {
        return Err(format!(
            "line {}: block {id:?} has no end marker",
            begin + 1
        ));
    }
    Ok(blocks)
}

/// Make the managed block `id` in `text` hold exactly `body`, and nothing else.
///
/// Returns `Ok(None)` when the block already holds `body`, so the caller writes
/// nothing. An existing block has its body replaced in place; a missing block is
/// appended at the end after exactly one blank line, unless the text already ends
/// with a blank line (an empty text gets no blank line). The markers are
/// `"<comment> BEGIN execute managed block: <id>"` and the matching END line.
/// Lines use the text's own line ending (CRLF when the text contains CRLF, else
/// LF), and a missing final newline stays missing on an existing block.
fn ensure_block_text(
    text: &str,
    id: &str,
    body: &str,
    comment: &str,
) -> Result<Option<String>, String> {
    check_block_comment(comment)?;
    check_block_id(id, comment)?;
    let lines = block_lines(body, comment)?;
    let pieces: Vec<&str> = text.split_inclusive('\n').collect();
    let blocks = find_managed_blocks(&pieces, comment)?;
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let begin = format!("{}{id}", block_prefix(comment, "BEGIN"));
    let end = format!("{}{id}", block_prefix(comment, "END"));
    let mut out = String::with_capacity(text.len() + body.len() + begin.len() + end.len() + 8);

    if let Some(block) = blocks.iter().find(|block| block.id == id) {
        let current: Vec<&str> = pieces[block.begin + 1..block.end]
            .iter()
            .map(|piece| line_text(piece))
            .collect();
        if current == lines {
            return Ok(None);
        }
        for piece in &pieces[..=block.begin] {
            out.push_str(piece);
        }
        for line in &lines {
            out.push_str(line);
            out.push_str(eol);
        }
        for piece in &pieces[block.end..] {
            out.push_str(piece);
        }
        return Ok(Some(out));
    }

    let ends_blank = pieces
        .last()
        .is_some_and(|piece| block_body(pieces.len() - 1, piece).trim().is_empty());
    out.push_str(text);
    if !text.is_empty() {
        if !text.ends_with('\n') {
            out.push_str(eol);
        }
        if !ends_blank {
            out.push_str(eol);
        }
    }
    for line in std::iter::once(begin.as_str())
        .chain(lines.iter().copied())
        .chain(std::iter::once(end.as_str()))
    {
        out.push_str(line);
        out.push_str(eol);
    }
    Ok(Some(out))
}

/// Remove the managed block `id` from `text`, together with at most one blank
/// line that separated it: the blank line before the begin marker, or else the
/// one after the end marker. Returns `Ok(None)` when there is no such block. A
/// byte order mark on the first line stays in the file.
fn remove_block_text(text: &str, id: &str, comment: &str) -> Result<Option<String>, String> {
    check_block_comment(comment)?;
    check_block_id(id, comment)?;
    let pieces: Vec<&str> = text.split_inclusive('\n').collect();
    let blocks = find_managed_blocks(&pieces, comment)?;
    let Some(block) = blocks.iter().find(|block| block.id == id) else {
        return Ok(None);
    };
    let is_blank = |index: usize| block_body(index, pieces[index]).trim().is_empty();
    let mut first = block.begin;
    let mut last = block.end;
    if first > 0 && is_blank(first - 1) {
        first -= 1;
    } else if last + 1 < pieces.len() && is_blank(last + 1) {
        last += 1;
    }
    let mut out = String::with_capacity(text.len());
    if first == 0
        && pieces
            .first()
            .is_some_and(|piece| piece.starts_with('\u{feff}'))
    {
        out.push('\u{feff}');
    }
    for (index, piece) in pieces.iter().enumerate() {
        if index < first || index > last {
            out.push_str(piece);
        }
    }
    Ok(Some(out))
}

/// Where `conf.ensure_line_near` puts a new line, relative to its anchor line.
#[derive(Clone, Copy)]
enum Placement {
    Before,
    After,
}

/// How `conf.remove_matching` compares a line with its argument.
#[derive(Clone, Copy)]
enum MatchMode {
    /// The whole line, without its line ending, equals the argument.
    Line,
    /// The line starts with the argument.
    Prefix,
}

/// Refuse an argument that could split a line or hide bytes in one.
fn check_whole_line_arg(name: &str, value: &str) -> Result<(), String> {
    if value.contains(['\r', '\n', '\0']) {
        return Err(format!(
            "{name} must be a single line without CR, LF or NUL"
        ));
    }
    Ok(())
}

/// Which pieces belong to a managed block, markers included. These editors take
/// no `comment` argument, so every comment marker is scanned: a block written
/// with any of them is owned by that block. A malformed block structure is an
/// error, so nothing is edited next to a block that cannot be read.
fn owned_pieces(pieces: &[&str]) -> Result<Vec<bool>, String> {
    let mut owned = vec![false; pieces.len()];
    for comment in ["#", ";", "//", "--"] {
        for block in find_managed_blocks(pieces, comment)? {
            if let Some(span) = owned.get_mut(block.begin..=block.end) {
                span.fill(true);
            }
        }
    }
    Ok(owned)
}

/// Make sure the whole line `line` is present exactly once in `text`.
///
/// Returns `Ok(None)` when `line` already appears anywhere, including inside a
/// managed block, so the caller writes nothing. Otherwise `line` is inserted
/// immediately before or after the first line that equals `anchor` (literal,
/// whole-line comparison). When no line equals `anchor`, or that first line is
/// inside a managed block, the call is an error and nothing is inserted. The new
/// line uses the text's line ending (CRLF when the text contains CRLF, else LF).
/// A byte order mark stays first in the file, and a missing final newline stays
/// missing.
fn ensure_line_near_text(
    text: &str,
    line: &str,
    anchor: &str,
    placement: Placement,
) -> Result<Option<String>, String> {
    check_whole_line_arg("line", line)?;
    check_whole_line_arg("anchor", anchor)?;
    let pieces: Vec<&str> = text.split_inclusive('\n').collect();
    let owned = owned_pieces(&pieces)?;
    if pieces.iter().any(|piece| line_matches(piece, line)) {
        return Ok(None);
    }
    let Some(index) = pieces.iter().position(|piece| line_matches(piece, anchor)) else {
        return Err(format!(
            "no line equals anchor {anchor:?}; nothing was inserted"
        ));
    };
    if owned.get(index).copied().unwrap_or(false) {
        return Err(format!(
            "anchor {anchor:?} at line {} is inside a managed block; nothing was inserted",
            index + 1
        ));
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + line.len() + eol.len() + 1);
    for (position, piece) in pieces.iter().enumerate() {
        if position != index {
            out.push_str(piece);
            continue;
        }
        match placement {
            Placement::Before => {
                let (bom, rest) = match piece.strip_prefix('\u{feff}') {
                    Some(rest) if index == 0 => ("\u{feff}", rest),
                    _ => ("", *piece),
                };
                out.push_str(bom);
                out.push_str(line);
                out.push_str(eol);
                out.push_str(rest);
            }
            Placement::After => {
                out.push_str(piece);
                if piece.ends_with('\n') {
                    out.push_str(line);
                    out.push_str(eol);
                } else {
                    // The anchor is the last line and has no line ending: the
                    // new line becomes the last line, still without one.
                    out.push_str(eol);
                    out.push_str(line);
                }
            }
        }
    }
    Ok(Some(out))
}

/// Remove every line of `text` that matches `needle` (see [`MatchMode`]).
///
/// Lines inside a managed block are kept, because the block owns them. Returns
/// `Ok(None)` when nothing is removed. An empty `needle` is refused, since it
/// would match every blank line (or every line in prefix mode). A byte order mark
/// on the first line stays in the file. When the removed line was the last one and
/// had no line ending, the line before it loses its line ending too, so a missing
/// final newline stays missing. That line is not changed when it is in a block.
fn remove_matching_text(
    text: &str,
    needle: &str,
    mode: MatchMode,
) -> Result<Option<String>, String> {
    check_whole_line_arg("line", needle)?;
    if needle.is_empty() {
        return Err("line must not be empty".to_owned());
    }
    let pieces: Vec<&str> = text.split_inclusive('\n').collect();
    let owned = owned_pieces(&pieces)?;
    let keep: Vec<bool> = pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            if owned.get(index).copied().unwrap_or(false) {
                return true;
            }
            let body = line_body(piece);
            let hit = match mode {
                MatchMode::Line => body == needle,
                MatchMode::Prefix => body.starts_with(needle),
            };
            !hit
        })
        .collect();
    if keep.iter().all(|kept| *kept) {
        return Ok(None);
    }
    let removed_tail =
        pieces.last().is_some_and(|piece| !piece.ends_with('\n')) && keep.last() == Some(&false);
    let last_kept = keep.iter().rposition(|kept| *kept);
    let mut out = String::with_capacity(text.len());
    if keep.first() == Some(&false)
        && pieces
            .first()
            .is_some_and(|piece| piece.starts_with('\u{feff}'))
    {
        out.push('\u{feff}');
    }
    for (index, (piece, kept)) in pieces.iter().zip(keep.iter()).enumerate() {
        if !kept {
            continue;
        }
        let strip =
            removed_tail && Some(index) == last_kept && !owned.get(index).copied().unwrap_or(false);
        if strip {
            out.push_str(
                piece
                    .strip_suffix("\r\n")
                    .or_else(|| piece.strip_suffix('\n'))
                    .unwrap_or(piece),
            );
        } else {
            out.push_str(piece);
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod set_key_tests {
    use super::{
        conf_ensure_block, conf_ensure_line_near, conf_remove_block, conf_remove_matching,
        ensure_block_text, ensure_line_near_text, remove_block_text, remove_matching_text,
        set_key_text, ConfStyle, ExecutionContext, MatchMode, Placement, BLOCK_ID_MAX_BYTES,
    };
    use serde_json::{json, Map, Value};
    use std::fs;

    const EQ: ConfStyle = ConfStyle {
        separator: "=",
        comment: "#",
        spaced: true,
        uncomment: false,
    };
    const EQ_UNCOMMENT: ConfStyle = ConfStyle {
        uncomment: true,
        ..EQ
    };
    const EQ_TIGHT: ConfStyle = ConfStyle {
        spaced: false,
        ..EQ
    };
    const COLON: ConfStyle = ConfStyle {
        separator: ":",
        comment: "#",
        spaced: true,
        uncomment: false,
    };
    const SPACE: ConfStyle = ConfStyle {
        separator: " ",
        comment: "#",
        spaced: false,
        uncomment: false,
    };

    fn set(text: &str, key: &str, value: &str, style: ConfStyle) -> Option<String> {
        set_key_text(text, key, value, style).expect("valid arguments")
    }

    #[test]
    fn replaces_value_in_place_and_keeps_every_other_line() {
        let text = "# font size\n\n  size   =  10\nother = x # keep\n";
        assert_eq!(
            set(text, "size", "12", EQ).as_deref(),
            Some("# font size\n\n  size   =  12\nother = x # keep\n")
        );
    }

    #[test]
    fn reports_unchanged_when_value_already_set() {
        assert_eq!(set("size = 12\n", "size", "12", EQ), None);
        // Trailing blanks on the live line do not count as part of the value.
        assert_eq!(set("size = 12   \n", "size", "12", EQ), None);
        assert_eq!(set("x = 1\nsize=12\n", "size", "12", EQ_TIGHT), None);
    }

    #[test]
    fn second_run_is_unchanged() {
        let once = set("a = 1\n", "size", "12", EQ).expect("first run changes");
        assert_eq!(once, "a = 1\nsize = 12\n");
        assert_eq!(set(&once, "size", "12", EQ), None);
    }

    #[test]
    fn crlf_file_keeps_crlf_when_replacing() {
        let text = "a = 1\r\nsize = 10\r\nb = 2\r\n";
        assert_eq!(
            set(text, "size", "12", EQ).as_deref(),
            Some("a = 1\r\nsize = 12\r\nb = 2\r\n")
        );
        assert_eq!(set("size = 12\r\n", "size", "12", EQ), None);
    }

    #[test]
    fn crlf_file_keeps_crlf_when_appending() {
        assert_eq!(
            set("a = 1\r\n", "size", "12", EQ).as_deref(),
            Some("a = 1\r\nsize = 12\r\n")
        );
        // A CRLF file that lacks a final newline gets one with its own line ending.
        assert_eq!(
            set("a = 1\r\nb = 2", "size", "12", EQ).as_deref(),
            Some("a = 1\r\nb = 2\r\nsize = 12\r\n")
        );
    }

    #[test]
    fn missing_final_newline_is_not_added_on_replace() {
        assert_eq!(
            set("a = 1\nsize = 10", "size", "12", EQ).as_deref(),
            Some("a = 1\nsize = 12")
        );
    }

    #[test]
    fn key_inside_a_comment_is_not_a_live_match() {
        assert_eq!(
            set("# size = 10\n", "size", "12", EQ).as_deref(),
            Some("# size = 10\nsize = 12\n")
        );
        assert_eq!(
            set(
                "; size = 10\n",
                "size",
                "12",
                ConfStyle { comment: ";", ..EQ }
            )
            .as_deref(),
            Some("; size = 10\nsize = 12\n")
        );
    }

    #[test]
    fn uncomment_rewrites_the_commented_default_in_place() {
        assert_eq!(
            set("x = 1\n# size = 10\ny = 2\n", "size", "12", EQ_UNCOMMENT).as_deref(),
            Some("x = 1\nsize = 12\ny = 2\n")
        );
        assert_eq!(
            set(
                "    #size=10\n",
                "size",
                "12",
                ConfStyle {
                    spaced: false,
                    ..EQ_UNCOMMENT
                }
            )
            .as_deref(),
            Some("    size=12\n")
        );
    }

    #[test]
    fn live_line_wins_over_a_commented_default() {
        assert_eq!(
            set("# size = 10\nsize = 11\n", "size", "12", EQ_UNCOMMENT).as_deref(),
            Some("# size = 10\nsize = 12\n")
        );
    }

    #[test]
    fn prefix_of_another_key_does_not_match() {
        assert_eq!(
            set("font-size = 9\n", "font", "x", EQ).as_deref(),
            Some("font-size = 9\nfont = x\n")
        );
        assert_eq!(
            set("font-size = 9\n", "font", "x", SPACE).as_deref(),
            Some("font-size = 9\nfont x\n")
        );
    }

    #[test]
    fn only_the_first_live_match_is_replaced() {
        assert_eq!(
            set("size = 1\nsize = 2\n", "size", "9", EQ).as_deref(),
            Some("size = 9\nsize = 2\n")
        );
    }

    #[test]
    fn colon_separator_is_preserved_and_appended_with_a_space() {
        assert_eq!(
            set("timeout: 5\nquiet: no\n", "timeout", "1", COLON).as_deref(),
            Some("timeout: 1\nquiet: no\n")
        );
        assert_eq!(
            set("", "quiet", "yes", COLON).as_deref(),
            Some("quiet: yes\n")
        );
    }

    #[test]
    fn whitespace_separator_needs_a_blank_after_the_key() {
        assert_eq!(
            set("LANG C\n", "LANG", "en", SPACE).as_deref(),
            Some("LANG en\n")
        );
        assert_eq!(
            set("LA C\n", "LANG", "en", SPACE).as_deref(),
            Some("LA C\nLANG en\n")
        );
        assert_eq!(set("LANG en\n", "LANG", "en", SPACE), None);
    }

    #[test]
    fn unspaced_separator_is_used_when_appending() {
        assert_eq!(
            set("A=b\n", "ENABLED", "yes", EQ_TIGHT).as_deref(),
            Some("A=b\nENABLED=yes\n")
        );
        assert_eq!(
            set("ENABLED=no\n", "ENABLED", "yes", EQ_TIGHT).as_deref(),
            Some("ENABLED=yes\n")
        );
    }

    #[test]
    fn byte_order_mark_is_kept() {
        assert_eq!(
            set("\u{feff}size = 10\nb = 1\n", "size", "12", EQ).as_deref(),
            Some("\u{feff}size = 12\nb = 1\n")
        );
        assert_eq!(set("\u{feff}size = 12\n", "size", "12", EQ), None);
    }

    #[test]
    fn empty_text_gets_the_new_line() {
        assert_eq!(set("", "size", "12", EQ).as_deref(), Some("size = 12\n"));
    }

    #[test]
    fn empty_value_is_written_as_is() {
        assert_eq!(
            set("size = 1\n", "size", "", EQ).as_deref(),
            Some("size = \n")
        );
    }

    #[test]
    fn rejects_bad_key_value_and_style() {
        for key in ["", "a b", "a=b", "a:b", "#a", "a;b", "a\tb"] {
            assert!(set_key_text("", key, "1", EQ).is_err(), "key {key:?}");
        }
        assert!(set_key_text(
            "",
            "a--b",
            "1",
            ConfStyle {
                comment: "--",
                ..EQ
            }
        )
        .is_err());
        assert!(set_key_text("", "size", "1\n2", EQ).is_err());
        assert!(set_key_text("", "size", "1\r", EQ).is_err());
        assert!(set_key_text("", "size", "1\0", EQ).is_err());
        assert!(set_key_text(
            "",
            "size",
            "1",
            ConfStyle {
                separator: "==",
                ..EQ
            }
        )
        .is_err());
        assert!(set_key_text(
            "",
            "size",
            "1",
            ConfStyle {
                separator: "",
                ..EQ
            }
        )
        .is_err());
        assert!(set_key_text("", "size", "1", ConfStyle { comment: "%", ..EQ }).is_err());
    }

    const BEGIN_NET: &str = "# BEGIN execute managed block: net\n";
    const END_NET: &str = "# END execute managed block: net\n";

    fn ensure(text: &str, id: &str, body: &str) -> Option<String> {
        ensure_block_text(text, id, body, "#").expect("valid block arguments")
    }

    fn remove(text: &str) -> Option<String> {
        remove_block_text(text, "net", "#").expect("valid block arguments")
    }

    fn args(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("expected a JSON object, got {other}"),
        }
    }

    #[test]
    fn ensure_appends_after_one_blank_line_when_text_ends_with_content() {
        assert_eq!(
            ensure("a = 1\n", "net", "port = 1\n").as_deref(),
            Some("a = 1\n\n# BEGIN execute managed block: net\nport = 1\n# END execute managed block: net\n")
        );
    }

    #[test]
    fn ensure_adds_no_blank_line_when_text_already_ends_with_one() {
        assert_eq!(
            ensure("a\n\n", "net", "x").as_deref(),
            Some("a\n\n# BEGIN execute managed block: net\nx\n# END execute managed block: net\n")
        );
        assert_eq!(
            ensure("a\n  \n", "net", "x").as_deref(),
            Some(
                "a\n  \n# BEGIN execute managed block: net\nx\n# END execute managed block: net\n"
            )
        );
    }

    #[test]
    fn ensure_terminates_a_missing_final_newline_before_the_blank_line() {
        assert_eq!(
            ensure("a = 1", "net", "x").as_deref(),
            Some("a = 1\n\n# BEGIN execute managed block: net\nx\n# END execute managed block: net\n")
        );
        // A blank last line without a newline only needs its own terminator.
        assert_eq!(
            ensure("a\n  ", "net", "x").as_deref(),
            Some(
                "a\n  \n# BEGIN execute managed block: net\nx\n# END execute managed block: net\n"
            )
        );
    }

    #[test]
    fn ensure_on_an_empty_text_writes_the_block_alone() {
        assert_eq!(
            ensure("", "net", "x").as_deref(),
            Some("# BEGIN execute managed block: net\nx\n# END execute managed block: net\n")
        );
    }

    #[test]
    fn empty_body_writes_two_markers_and_a_newline_body_writes_one_blank_line() {
        assert_eq!(
            ensure("", "net", "").as_deref(),
            Some("# BEGIN execute managed block: net\n# END execute managed block: net\n")
        );
        assert_eq!(
            ensure("", "net", "\n").as_deref(),
            Some("# BEGIN execute managed block: net\n\n# END execute managed block: net\n")
        );
    }

    #[test]
    fn ensure_replaces_the_body_in_place_and_keeps_every_other_line() {
        let text = format!(
            "top = 1\n{BEGIN_NET}old = 1\n{END_NET}bottom = 2\n# BEGIN execute managed block: other\nz\n# END execute managed block: other\n"
        );
        let expected = format!(
            "top = 1\n{BEGIN_NET}new = 2\n{END_NET}bottom = 2\n# BEGIN execute managed block: other\nz\n# END execute managed block: other\n"
        );
        assert_eq!(
            ensure(&text, "net", "new = 2\n").as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn ensure_replaces_with_a_longer_or_empty_body() {
        let text = format!("{BEGIN_NET}x\n{END_NET}");
        assert_eq!(
            ensure(&text, "net", "a\nb\nc\n").as_deref(),
            Some(format!("{BEGIN_NET}a\nb\nc\n{END_NET}").as_str())
        );
        assert_eq!(
            ensure(&text, "net", "").as_deref(),
            Some(format!("{BEGIN_NET}{END_NET}").as_str())
        );
    }

    #[test]
    fn ensure_reports_none_when_the_block_already_holds_the_body() {
        let text = format!("a\n\n{BEGIN_NET}x\n{END_NET}");
        assert_eq!(ensure(&text, "net", "x\n"), None);
        // Trailing line endings in the body do not change what the block holds.
        assert_eq!(ensure(&text, "net", "x"), None);
    }

    #[test]
    fn ensure_twice_equals_ensure_once() {
        for text in [
            "",
            "a = 1\n",
            "a = 1",
            "a\n\n",
            "x\r\ny\r\n",
            "x\r\ny",
            "\u{feff}a\n",
        ] {
            let once = ensure(text, "net", "k = v\n").expect("first run changes");
            assert_eq!(ensure(&once, "net", "k = v\n"), None, "text {text:?}");
        }
    }

    #[test]
    fn other_comment_markers_are_written_and_recognised() {
        for comment in ["//", ";", "--"] {
            let block = format!("{comment} BEGIN execute managed block: net\nx\n{comment} END execute managed block: net\n");
            let once = ensure_block_text("", "net", "x", comment)
                .expect("valid")
                .expect("changes");
            assert_eq!(once, block, "comment {comment}");
            assert_eq!(ensure_block_text(&once, "net", "x", comment), Ok(None));
        }
    }

    #[test]
    fn crlf_text_gets_crlf_markers_and_body_lines() {
        assert_eq!(
            ensure("a = 1\r\n", "net", "x\ny\n").as_deref(),
            Some("a = 1\r\n\r\n# BEGIN execute managed block: net\r\nx\r\ny\r\n# END execute managed block: net\r\n")
        );
    }

    #[test]
    fn crlf_block_is_replaced_and_stays_crlf() {
        let text =
            "# BEGIN execute managed block: net\r\nold\r\n# END execute managed block: net\r\n";
        assert_eq!(
            ensure(text, "net", "new").as_deref(),
            Some(
                "# BEGIN execute managed block: net\r\nnew\r\n# END execute managed block: net\r\n"
            )
        );
        assert_eq!(
            ensure(
                "# BEGIN execute managed block: net\r\nnew\r\n# END execute managed block: net\r\n",
                "net",
                "new"
            ),
            None
        );
    }

    #[test]
    fn lf_text_gets_lf_only() {
        let written = ensure("a\n", "net", "x\ny\n").expect("changes");
        assert!(!written.contains('\r'));
    }

    #[test]
    fn crlf_text_without_final_newline_keeps_it_missing_on_replace() {
        let text = "# BEGIN execute managed block: net\r\nold\r\n# END execute managed block: net";
        assert_eq!(
            ensure(text, "net", "new").as_deref(),
            Some("# BEGIN execute managed block: net\r\nnew\r\n# END execute managed block: net")
        );
    }

    #[test]
    fn crlf_text_without_final_newline_gets_crlf_before_the_blank_line() {
        assert_eq!(
            ensure("a = 1\r\nb", "net", "x").as_deref(),
            Some("a = 1\r\nb\r\n\r\n# BEGIN execute managed block: net\r\nx\r\n# END execute managed block: net\r\n")
        );
    }

    #[test]
    fn missing_final_newline_stays_missing_when_the_block_is_last() {
        let text = format!("{BEGIN_NET}old\n# END execute managed block: net");
        assert_eq!(
            ensure(&text, "net", "new").as_deref(),
            Some("# BEGIN execute managed block: net\nnew\n# END execute managed block: net")
        );
    }

    #[test]
    fn byte_order_mark_stays_on_the_first_line() {
        let text = format!("\u{feff}{BEGIN_NET}old\n{END_NET}");
        assert_eq!(
            ensure(&text, "net", "new").as_deref(),
            Some(format!("\u{feff}{BEGIN_NET}new\n{END_NET}").as_str())
        );
        assert_eq!(
            ensure("\u{feff}a = 1\n", "net", "x").as_deref(),
            Some("\u{feff}a = 1\n\n# BEGIN execute managed block: net\nx\n# END execute managed block: net\n")
        );
    }

    #[test]
    fn remove_takes_the_block_and_the_blank_line_before_it() {
        assert_eq!(
            remove(
                "a\n\n# BEGIN execute managed block: net\nx\n# END execute managed block: net\nb\n"
            )
            .as_deref(),
            Some("a\nb\n")
        );
    }

    #[test]
    fn remove_takes_the_blank_line_after_when_none_is_before() {
        assert_eq!(
            remove(
                "# BEGIN execute managed block: net\nx\n# END execute managed block: net\n\nrest\n"
            )
            .as_deref(),
            Some("rest\n")
        );
    }

    #[test]
    fn remove_takes_only_one_blank_line() {
        assert_eq!(
            remove(
                "x\n\n# BEGIN execute managed block: net\n# END execute managed block: net\n\ny\n"
            )
            .as_deref(),
            Some("x\n\ny\n")
        );
    }

    #[test]
    fn remove_without_blank_lines_joins_the_neighbours() {
        assert_eq!(
            remove("x\n# BEGIN execute managed block: net\nbody\n# END execute managed block: net\ny\n").as_deref(),
            Some("x\ny\n")
        );
    }

    #[test]
    fn remove_at_end_of_file_without_final_newline() {
        assert_eq!(
            remove(
                "x\n\n# BEGIN execute managed block: net\nbody\n# END execute managed block: net"
            )
            .as_deref(),
            Some("x\n")
        );
    }

    #[test]
    fn remove_of_the_only_block_leaves_an_empty_text() {
        assert_eq!(
            remove(&format!("{BEGIN_NET}x\n{END_NET}")).as_deref(),
            Some("")
        );
    }

    #[test]
    fn remove_keeps_other_blocks() {
        let text = "# BEGIN execute managed block: a\n1\n# END execute managed block: a\n\n# BEGIN execute managed block: net\n2\n# END execute managed block: net\n";
        assert_eq!(
            remove(text).as_deref(),
            Some("# BEGIN execute managed block: a\n1\n# END execute managed block: a\n")
        );
    }

    #[test]
    fn remove_reports_none_when_the_block_is_absent() {
        assert_eq!(remove("x\n"), None);
        assert_eq!(remove(""), None);
        let other = "# BEGIN execute managed block: other\nz\n# END execute managed block: other\n";
        assert_eq!(remove(other), None);
    }

    #[test]
    fn remove_keeps_a_byte_order_mark_when_it_removes_the_first_line() {
        assert_eq!(
            remove(&format!("\u{feff}{BEGIN_NET}x\n{END_NET}rest\n")).as_deref(),
            Some("\u{feff}rest\n")
        );
    }

    #[test]
    fn ensure_then_remove_restores_the_original_text_byte_for_byte() {
        for text in [
            "",
            "a = 1\n",
            "a = 1\nb = 2\n",
            "x\r\ny\r\n",
            "\u{feff}a\n",
            "[sec]\nk=v\n\n[other]\nq=1\n",
        ] {
            let written = ensure(text, "net", "k = v\n").expect("changes");
            assert_eq!(remove(&written).as_deref(), Some(text), "text {text:?}");
        }
    }

    #[test]
    fn ensure_then_remove_drops_the_blank_line_of_a_text_that_already_ended_with_one() {
        // Documented limit: the blank line before an appended block cannot be
        // told apart from one the file already had, so remove takes it.
        let written = ensure("a\n\n", "net", "x").expect("changes");
        assert_eq!(remove(&written).as_deref(), Some("a\n"));
    }

    #[test]
    fn hostile_ids_are_refused() {
        let long = "x".repeat(BLOCK_ID_MAX_BYTES + 1);
        for id in [
            "",
            "a\nb",
            "a\rb",
            "x\n# END execute managed block: y",
            "a#b",
            "tab\there",
            "caf\u{e9}",
            "nul\0",
            long.as_str(),
        ] {
            assert!(ensure_block_text("", id, "x", "#").is_err(), "id {id:?}");
            assert!(remove_block_text("", id, "#").is_err(), "id {id:?}");
        }
        assert!(ensure_block_text("", "a--b", "x", "--").is_err());
        assert!(ensure_block_text("", "a//b", "x", "//").is_err());
    }

    #[test]
    fn longest_printable_id_is_accepted() {
        let id = "x".repeat(BLOCK_ID_MAX_BYTES);
        assert!(ensure_block_text("", &id, "x", "#")
            .expect("valid")
            .is_some());
    }

    #[test]
    fn hostile_bodies_are_refused() {
        for body in [
            "# END execute managed block: net\n",
            "x\n# BEGIN execute managed block: other\n",
            "# END execute managed block: other",
            "a\rb",
            "a\0b",
            "ends in cr\r",
        ] {
            assert!(
                ensure_block_text("", "net", body, "#").is_err(),
                "body {body:?}"
            );
        }
    }

    #[test]
    fn a_body_line_that_only_resembles_a_marker_is_allowed() {
        assert!(
            ensure_block_text("", "net", "#END execute managed block: net\n", "#")
                .expect("valid")
                .is_some()
        );
    }

    #[test]
    fn unknown_comment_marker_is_refused() {
        assert!(ensure_block_text("", "net", "x", "%").is_err());
        assert!(remove_block_text("", "net", "%").is_err());
    }

    #[test]
    fn malformed_states_are_errors_that_name_the_line() {
        let begin_only = "a\n# BEGIN execute managed block: net\nx\n";
        let err = ensure_block_text(begin_only, "net", "y", "#").expect_err("unclosed");
        assert!(
            err.contains("line 2") && err.contains("no end marker"),
            "{err}"
        );
        assert!(remove_block_text(begin_only, "net", "#").is_err());

        let end_only = "a\n# END execute managed block: net\n";
        let err = ensure_block_text(end_only, "net", "y", "#").expect_err("no begin");
        assert!(
            err.contains("line 2") && err.contains("no begin marker"),
            "{err}"
        );
        assert!(remove_block_text(end_only, "net", "#").is_err());

        let twice = format!("{BEGIN_NET}{END_NET}{BEGIN_NET}{END_NET}");
        let err = ensure_block_text(&twice, "net", "y", "#").expect_err("duplicate");
        assert!(
            err.contains("line 3") && err.contains("already defined"),
            "{err}"
        );
        assert!(remove_block_text(&twice, "net", "#").is_err());

        let nested = "# BEGIN execute managed block: a\n# BEGIN execute managed block: b\n# END execute managed block: b\n# END execute managed block: a\n";
        let err = ensure_block_text(nested, "a", "y", "#").expect_err("nested");
        assert!(err.contains("line 2") && err.contains("nest"), "{err}");
        assert!(remove_block_text(nested, "b", "#").is_err());

        let mismatched = "# BEGIN execute managed block: a\nx\n# END execute managed block: b\n";
        let err = ensure_block_text(mismatched, "a", "y", "#").expect_err("mismatch");
        assert!(
            err.contains("line 3") && err.contains("does not match"),
            "{err}"
        );
    }

    #[test]
    fn malformed_state_of_another_block_is_still_an_error() {
        let broken = "# BEGIN execute managed block: other\nx\n";
        assert!(ensure_block_text(broken, "net", "y", "#").is_err());
        assert!(remove_block_text(broken, "net", "#").is_err());
    }

    #[test]
    fn applet_ensure_creates_the_block_then_reports_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        let first = conf_ensure_block(
            &args(json!({"path": "app.conf", "id": "net", "content": "port = 1\n"})),
            dir.path(),
            &context,
        )
        .unwrap();
        assert_eq!(first, json!({"changed": true, "path": "app.conf"}));
        assert_eq!(
            fs::read_to_string(dir.path().join("app.conf")).unwrap(),
            "# BEGIN execute managed block: net\nport = 1\n# END execute managed block: net\n"
        );
        let second = conf_ensure_block(
            &args(json!({"path": "app.conf", "id": "net", "content": "port = 1\n"})),
            dir.path(),
            &context,
        )
        .unwrap();
        assert_eq!(second["changed"], false);
    }

    #[test]
    fn applet_ensure_replaces_only_the_block_body() {
        let dir = tempfile::tempdir().unwrap();
        let original = format!("# top comment\nname = demo\n\n{BEGIN_NET}old\n{END_NET}port=80\n");
        fs::write(dir.path().join("app.conf"), &original).unwrap();
        let output = conf_ensure_block(
            &args(json!({
                "path": "app.conf",
                "id": "net",
                "content": "new\n",
                "comment": "#"
            })),
            dir.path(),
            &ExecutionContext::unrestricted(),
        )
        .unwrap();
        assert_eq!(output["changed"], true);
        assert_eq!(
            fs::read_to_string(dir.path().join("app.conf")).unwrap(),
            format!("# top comment\nname = demo\n\n{BEGIN_NET}new\n{END_NET}port=80\n")
        );
    }

    #[test]
    fn applet_remove_undoes_ensure_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        fs::write(dir.path().join("app.conf"), "name = demo\n").unwrap();
        conf_ensure_block(
            &args(json!({"path": "app.conf", "id": "net", "content": "x\n"})),
            dir.path(),
            &context,
        )
        .unwrap();
        let removed = conf_remove_block(
            &args(json!({"path": "app.conf", "id": "net"})),
            dir.path(),
            &context,
        )
        .unwrap();
        assert_eq!(removed, json!({"changed": true, "path": "app.conf"}));
        assert_eq!(
            fs::read_to_string(dir.path().join("app.conf")).unwrap(),
            "name = demo\n"
        );
        let again = conf_remove_block(
            &args(json!({"path": "app.conf", "id": "net"})),
            dir.path(),
            &context,
        )
        .unwrap();
        assert_eq!(again["changed"], false);
    }

    #[test]
    fn applet_remove_on_a_missing_file_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let output = conf_remove_block(
            &args(json!({"path": "absent.conf", "id": "net"})),
            dir.path(),
            &ExecutionContext::unrestricted(),
        )
        .unwrap();
        assert_eq!(output["changed"], false);
        assert!(!dir.path().join("absent.conf").exists());
    }

    #[test]
    fn applet_refuses_a_malformed_file_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let broken = format!("{BEGIN_NET}x\n");
        fs::write(dir.path().join("app.conf"), &broken).unwrap();
        let context = ExecutionContext::unrestricted();
        assert!(conf_ensure_block(
            &args(json!({"path": "app.conf", "id": "net", "content": "y\n"})),
            dir.path(),
            &context,
        )
        .is_err());
        assert!(conf_remove_block(
            &args(json!({"path": "app.conf", "id": "net"})),
            dir.path(),
            &context,
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("app.conf")).unwrap(),
            broken
        );
    }

    #[test]
    fn applet_refuses_bad_comment_and_missing_fields_without_creating_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        assert!(conf_ensure_block(
            &args(json!({"path": "new.conf", "id": "net", "content": "x", "comment": "%"})),
            dir.path(),
            &context,
        )
        .is_err());
        assert!(conf_ensure_block(
            &args(json!({"path": "new.conf", "content": "x"})),
            dir.path(),
            &context,
        )
        .is_err());
        assert!(
            conf_remove_block(&args(json!({"path": "new.conf"})), dir.path(), &context,).is_err()
        );
        assert!(!dir.path().join("new.conf").exists());
    }

    fn near(text: &str, line: &str, anchor: &str, at: Placement) -> Option<String> {
        ensure_line_near_text(text, line, anchor, at).expect("valid arguments")
    }

    fn near_refused(text: &str, line: &str, anchor: &str, at: Placement) -> String {
        ensure_line_near_text(text, line, anchor, at).expect_err("the edit must be refused")
    }

    fn strip(text: &str, needle: &str, mode: MatchMode) -> Option<String> {
        remove_matching_text(text, needle, mode).expect("valid arguments")
    }

    #[test]
    fn near_inserts_after_the_anchor_and_keeps_other_lines() {
        assert_eq!(
            near("# head\nanchor\nkeep\n", "new", "anchor", Placement::After).as_deref(),
            Some("# head\nanchor\nnew\nkeep\n")
        );
    }

    #[test]
    fn near_inserts_before_the_anchor() {
        assert_eq!(
            near("# head\nanchor\nkeep\n", "new", "anchor", Placement::Before).as_deref(),
            Some("# head\nnew\nanchor\nkeep\n")
        );
    }

    #[test]
    fn near_is_none_when_the_line_is_present_anywhere() {
        for at in [Placement::Before, Placement::After] {
            assert_eq!(near("new\nanchor\n", "new", "anchor", at), None);
            assert_eq!(near("anchor\nx\nnew\n", "new", "anchor", at), None);
        }
    }

    #[test]
    fn near_treats_a_line_only_inside_a_block_as_present() {
        let text = format!("{BEGIN_NET}new\n{END_NET}anchor\n");
        assert_eq!(near(&text, "new", "anchor", Placement::After), None);
    }

    #[test]
    fn near_uses_only_the_first_anchor_line() {
        assert_eq!(
            near("anchor\nx\nanchor\n", "new", "anchor", Placement::After).as_deref(),
            Some("anchor\nnew\nx\nanchor\n")
        );
    }

    #[test]
    fn near_matches_the_anchor_literally_and_whole_line() {
        let text = "auth pam_unix.so\nabc\n* \n";
        for anchor in ["pam_unix", "pam_unix.so ", "a.c", "*", "^abc$", "ab"] {
            let message = near_refused(text, "new", anchor, Placement::After);
            assert!(
                message.contains("no line equals anchor"),
                "{anchor:?}: {message}"
            );
        }
        assert_eq!(
            near(text, "new", "abc", Placement::After).as_deref(),
            Some("auth pam_unix.so\nabc\nnew\n* \n")
        );
    }

    #[test]
    fn near_refuses_a_missing_anchor_instead_of_appending() {
        assert!(ensure_line_near_text("a\nb\n", "new", "absent", Placement::After).is_err());
        assert!(ensure_line_near_text("", "new", "anchor", Placement::After).is_err());
    }

    #[test]
    fn near_crlf_text_gets_crlf_for_the_new_line() {
        assert_eq!(
            near("a\r\nanchor\r\nb\r\n", "new", "anchor", Placement::After).as_deref(),
            Some("a\r\nanchor\r\nnew\r\nb\r\n")
        );
        assert_eq!(
            near("a\r\nanchor\r\nb\r\n", "new", "anchor", Placement::Before).as_deref(),
            Some("a\r\nnew\r\nanchor\r\nb\r\n")
        );
    }

    #[test]
    fn near_keeps_a_missing_final_newline_missing() {
        assert_eq!(
            near("a\nanchor", "new", "anchor", Placement::After).as_deref(),
            Some("a\nanchor\nnew")
        );
        assert_eq!(
            near("a\nanchor", "new", "anchor", Placement::Before).as_deref(),
            Some("a\nnew\nanchor")
        );
        assert_eq!(
            near("a\r\nanchor", "new", "anchor", Placement::After).as_deref(),
            Some("a\r\nanchor\r\nnew")
        );
    }

    #[test]
    fn near_keeps_the_byte_order_mark_first() {
        assert_eq!(
            near("\u{feff}anchor\nb\n", "new", "anchor", Placement::Before).as_deref(),
            Some("\u{feff}new\nanchor\nb\n")
        );
        assert_eq!(
            near("\u{feff}anchor\nb\n", "new", "anchor", Placement::After).as_deref(),
            Some("\u{feff}anchor\nnew\nb\n")
        );
        assert_eq!(
            near("\u{feff}new\nb\n", "new", "anchor", Placement::After),
            None
        );
    }

    #[test]
    fn near_refuses_line_breaks_and_nul_in_line_or_anchor() {
        for bad in ["a\rb", "a\nb", "a\0b"] {
            assert!(ensure_line_near_text("anchor\n", bad, "anchor", Placement::After).is_err());
            assert!(ensure_line_near_text("anchor\n", "new", bad, Placement::After).is_err());
        }
    }

    #[test]
    fn near_twice_equals_once() {
        let once = near("anchor\nkeep\n", "new", "anchor", Placement::After).unwrap();
        assert_eq!(near(&once, "new", "anchor", Placement::After), None);
        assert_eq!(near(&once, "new", "anchor", Placement::Before), None);
    }

    #[test]
    fn near_refuses_an_anchor_inside_a_managed_block() {
        let text = format!("{BEGIN_NET}anchor\n{END_NET}");
        let message = near_refused(&text, "new", "anchor", Placement::After);
        assert!(message.contains("inside a managed block"), "{message}");
    }

    #[test]
    fn near_refuses_when_the_first_anchor_is_in_a_block_even_if_a_later_one_is_not() {
        let text = format!("{BEGIN_NET}anchor\n{END_NET}anchor\n");
        assert!(ensure_line_near_text(&text, "new", "anchor", Placement::After).is_err());
    }

    #[test]
    fn near_leaves_the_block_alone_when_the_anchor_is_outside_it() {
        let text = format!("{BEGIN_NET}x\n{END_NET}anchor\ntail\n");
        assert_eq!(
            near(&text, "new", "anchor", Placement::After),
            Some(format!("{BEGIN_NET}x\n{END_NET}anchor\nnew\ntail\n"))
        );
    }

    #[test]
    fn near_recognises_a_semicolon_block() {
        let text = "; BEGIN execute managed block: net\nanchor\n; END execute managed block: net\n";
        assert!(ensure_line_near_text(text, "new", "anchor", Placement::After).is_err());
    }

    #[test]
    fn near_refuses_a_malformed_block_structure() {
        let text = format!("{BEGIN_NET}x\nanchor\n");
        assert!(ensure_line_near_text(&text, "new", "anchor", Placement::After).is_err());
    }

    #[test]
    fn remove_matching_removes_every_exact_line_and_keeps_the_others() {
        assert_eq!(
            strip("pam a\nkeep\npam a\nend\n", "pam a", MatchMode::Line).as_deref(),
            Some("keep\nend\n")
        );
    }

    #[test]
    fn remove_matching_line_mode_ignores_substrings_and_indented_copies() {
        let text = "  pam_u2f.so\npam_u2f.so x\n# pam_u2f.so\n";
        assert_eq!(strip(text, "pam_u2f.so", MatchMode::Line), None);
    }

    #[test]
    fn remove_matching_prefix_mode_removes_lines_that_start_with_the_text() {
        assert_eq!(
            strip(
                "auth pam_u2f.so\nkeep\nauth pam_u2f.so sufficient\n",
                "auth pam_u2f",
                MatchMode::Prefix
            )
            .as_deref(),
            Some("keep\n")
        );
        assert_eq!(
            strip("  auth pam_u2f\n", "auth pam_u2f", MatchMode::Prefix),
            None
        );
    }

    #[test]
    fn remove_matching_is_none_when_nothing_matches() {
        assert_eq!(strip("a\nb\n", "c", MatchMode::Line), None);
        assert_eq!(strip("", "c", MatchMode::Line), None);
    }

    #[test]
    fn remove_matching_keeps_crlf_on_the_other_lines() {
        assert_eq!(
            strip("a\r\nneedle\r\nb\r\n", "needle", MatchMode::Line).as_deref(),
            Some("a\r\nb\r\n")
        );
    }

    #[test]
    fn remove_matching_keeps_the_byte_order_mark_when_the_first_line_goes() {
        assert_eq!(
            strip("\u{feff}needle\nb\n", "needle", MatchMode::Line).as_deref(),
            Some("\u{feff}b\n")
        );
    }

    #[test]
    fn remove_matching_keeps_a_missing_final_newline_missing() {
        assert_eq!(
            strip("a\nneedle", "needle", MatchMode::Line).as_deref(),
            Some("a")
        );
        assert_eq!(
            strip("a\r\nb\r\nneedle", "needle", MatchMode::Line).as_deref(),
            Some("a\r\nb")
        );
        assert_eq!(
            strip("a\nneedle\n", "needle", MatchMode::Line).as_deref(),
            Some("a\n")
        );
    }

    #[test]
    fn remove_matching_of_every_line_leaves_an_empty_text() {
        assert_eq!(
            strip("needle\n", "needle", MatchMode::Line).as_deref(),
            Some("")
        );
    }

    #[test]
    fn remove_matching_keeps_lines_inside_a_managed_block() {
        let text = format!("needle\n{BEGIN_NET}needle\n{END_NET}needle\n");
        let expected = format!("{BEGIN_NET}needle\n{END_NET}");
        assert_eq!(
            strip(&text, "needle", MatchMode::Line).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn remove_matching_keeps_a_block_end_line_terminated_when_it_is_last() {
        let text = format!("{BEGIN_NET}x\n{END_NET}needle");
        let expected = format!("{BEGIN_NET}x\n{END_NET}");
        assert_eq!(
            strip(&text, "needle", MatchMode::Line).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn remove_matching_refuses_an_empty_needle_and_line_breaks() {
        assert!(remove_matching_text("a\n", "", MatchMode::Line).is_err());
        assert!(remove_matching_text("a\n", "", MatchMode::Prefix).is_err());
        for bad in ["a\rb", "a\nb", "a\0b"] {
            assert!(remove_matching_text("a\n", bad, MatchMode::Line).is_err());
        }
    }

    #[test]
    fn remove_matching_twice_equals_once() {
        let once = strip("a\nneedle\nneedle\nb\n", "needle", MatchMode::Line).unwrap();
        assert_eq!(once, "a\nb\n");
        assert_eq!(strip(&once, "needle", MatchMode::Line), None);
    }

    #[test]
    fn remove_matching_refuses_a_malformed_block_structure() {
        let text = format!("{BEGIN_NET}needle\n");
        assert!(remove_matching_text(&text, "needle", MatchMode::Line).is_err());
    }

    #[test]
    fn applet_ensure_line_near_inserts_once_then_reports_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        fs::write(dir.path().join("tmux.conf"), "# Pane Controls\nbind x\n").unwrap();
        let request = json!({
            "path": "tmux.conf",
            "line": "bind y",
            "anchor": "# Pane Controls",
            "position": "after"
        });
        let first = conf_ensure_line_near(&args(request.clone()), dir.path(), &context).unwrap();
        assert_eq!(first, json!({"changed": true, "path": "tmux.conf"}));
        assert_eq!(
            fs::read_to_string(dir.path().join("tmux.conf")).unwrap(),
            "# Pane Controls\nbind y\nbind x\n"
        );
        let second = conf_ensure_line_near(&args(request), dir.path(), &context).unwrap();
        assert_eq!(second["changed"], false);
    }

    #[test]
    fn applet_ensure_line_near_refuses_a_missing_anchor_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        assert!(conf_ensure_line_near(
            &args(
                json!({"path": "new.conf", "line": "x", "anchor": "absent", "position": "before"})
            ),
            dir.path(),
            &context,
        )
        .is_err());
        assert!(!dir.path().join("new.conf").exists());
    }

    #[test]
    fn applet_ensure_line_near_rejects_a_bad_position() {
        let dir = tempfile::tempdir().unwrap();
        assert!(conf_ensure_line_near(
            &args(json!({"path": "a.conf", "line": "x", "anchor": "y", "position": "middle"})),
            dir.path(),
            &ExecutionContext::unrestricted(),
        )
        .is_err());
    }

    #[test]
    fn applet_remove_matching_removes_once_then_reports_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        fs::write(dir.path().join("app.conf"), "name = demo\nold = 1\n").unwrap();
        let request = json!({"path": "app.conf", "line": "old = 1"});
        let first = conf_remove_matching(&args(request.clone()), dir.path(), &context).unwrap();
        assert_eq!(first, json!({"changed": true, "path": "app.conf"}));
        assert_eq!(
            fs::read_to_string(dir.path().join("app.conf")).unwrap(),
            "name = demo\n"
        );
        let second = conf_remove_matching(&args(request), dir.path(), &context).unwrap();
        assert_eq!(second["changed"], false);
    }

    #[test]
    fn applet_remove_matching_prefix_mode_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        fs::write(dir.path().join("app.conf"), "auth pam_u2f.so a\nkeep\n").unwrap();
        let output = conf_remove_matching(
            &args(json!({"path": "app.conf", "line": "auth pam_u2f", "match": "prefix"})),
            dir.path(),
            &context,
        )
        .unwrap();
        assert_eq!(output["changed"], true);
        assert_eq!(
            fs::read_to_string(dir.path().join("app.conf")).unwrap(),
            "keep\n"
        );
        let absent = conf_remove_matching(
            &args(json!({"path": "absent.conf", "line": "x"})),
            dir.path(),
            &context,
        )
        .unwrap();
        assert_eq!(absent["changed"], false);
        assert!(!dir.path().join("absent.conf").exists());
        assert!(conf_remove_matching(
            &args(json!({"path": "app.conf", "line": "x", "match": "regex"})),
            dir.path(),
            &context,
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn applet_remove_matching_through_a_symlink_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        fs::write(dir.path().join("target.conf"), "a\nneedle\n").unwrap();
        std::os::unix::fs::symlink("target.conf", dir.path().join("link.conf")).unwrap();
        let output = conf_remove_matching(
            &args(json!({"path": "link.conf", "line": "needle"})),
            dir.path(),
            &context,
        )
        .unwrap();
        assert_eq!(output["changed"], true);
        assert!(fs::symlink_metadata(dir.path().join("link.conf"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(dir.path().join("target.conf")).unwrap(),
            "a\n"
        );
    }
}
