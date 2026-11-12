//! Named state: flags, toggles, small values and debounce files (#351).
//!
//! Every operation takes the state directory explicitly. The caller passes the
//! XDG state location; tests pass a temporary directory. Under that directory
//! each kind of state lives in its own subdirectory, so a flag, a value and a
//! debounce record can share a name without colliding:
//!
//! - `flags/<name>`: a flag is the file's existence.
//! - `values/<name>`: a small UTF-8 value, replaced atomically on write.
//! - `debounce/<name>`: the last recorded time in milliseconds, in decimal.
//!
//! A state name is one or more `/`-separated segments. Each segment is
//! non-empty, is not `.` or `..`, and uses only ASCII letters, digits, `-`,
//! `_` and `.`. Names that fail these rules are refused before any filesystem
//! access, so a name can never leave its kind directory.
//!
//! Mutating operations create missing parent directories, owner-only (0700) on
//! Unix. Files are created owner-only (0600). Read-only operations never create
//! anything.

use std::fs::{self, DirBuilder, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::platform;

/// Longest accepted state name in bytes, separators included.
pub const MAX_NAME_BYTES: usize = 1024;
/// Longest accepted single segment of a state name, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 255;
/// Largest value that [`value_write`] accepts and [`value_read`] returns.
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

const FLAG_DIR: &str = "flags";
const VALUE_DIR: &str = "values";
const DEBOUNCE_DIR: &str = "debounce";

/// Largest debounce record accepted. A decimal millisecond count fits easily.
const MAX_STAMP_BYTES: usize = 32;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Device names that Windows maps to devices regardless of directory or
/// extension. Refused on every platform so a name stays portable.
fn is_reserved_device(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    const FIXED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    if FIXED.iter().any(|device| stem.eq_ignore_ascii_case(device)) {
        return true;
    }
    let numbered = |prefix: &str| {
        let bytes = stem.as_bytes();
        bytes.len() == prefix.len() + 1
            && bytes[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
            && matches!(bytes[prefix.len()], b'1'..=b'9')
    };
    numbered("COM") || numbered("LPT")
}

/// Check a state name. Runs before any filesystem access.
///
/// A valid name is a short list of segments of ASCII letters, digits, `-`, `_`
/// and `.`, separated by `/`. Empty segments, `.`, `..`, a leading `/`,
/// backslashes, NUL, non-ASCII text, over-long names and Windows device names
/// are all refused with [`io::ErrorKind::InvalidInput`].
pub fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty() {
        return Err(invalid("state name is empty"));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(invalid(format!(
            "state name exceeds {MAX_NAME_BYTES} bytes"
        )));
    }
    if name.starts_with('/') {
        return Err(invalid("state name must not start with '/'"));
    }
    for segment in name.split('/') {
        if segment.is_empty() {
            return Err(invalid("state name has an empty segment"));
        }
        if segment == "." || segment == ".." {
            return Err(invalid("state name segment must not be '.' or '..'"));
        }
        if segment.len() > MAX_SEGMENT_BYTES {
            return Err(invalid(format!(
                "state name segment exceeds {MAX_SEGMENT_BYTES} bytes"
            )));
        }
        if !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(invalid(
                "state name may contain only ASCII letters, digits, '-', '_', '.' and '/'",
            ));
        }
        if is_reserved_device(segment) {
            return Err(invalid("state name segment is a reserved device name"));
        }
    }
    Ok(())
}

/// Validate `name` and map it under `state_dir/<kind>/`.
fn entry_path(state_dir: &Path, kind: &str, name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    Ok(state_dir.join(kind).join(name))
}

/// Create the parent directories of `path` with owner-only permissions.
fn create_parents(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)
}

/// Read a file of at most `limit` bytes. Returns `None` when it does not
/// exist. A file over the limit is refused with [`io::ErrorKind::InvalidData`].
fn read_bounded(path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stored state exceeds {limit} bytes"),
        ));
    }
    Ok(Some(bytes))
}

/// Write `bytes` to a temporary sibling of `path`, flush it, then rename it
/// over `path`. The temporary name contains `~`, which validated names never
/// contain, so it cannot clash with another state entry.
fn replace_contents(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("state name has no file name"))?;
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_file_name(format!(
        ".{file_name}~{}.{sequence}.tmp",
        std::process::id()
    ));
    let result = write_and_publish(&temp, path, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_and_publish(temp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = platform::private_options()
        .write(true)
        .create_new(true)
        .open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    platform::replace_file(temp, path).map_err(|error| error.error)
}

/// Set a flag. Setting an already-set flag is not an error.
pub fn flag_set(state_dir: &Path, name: &str) -> io::Result<()> {
    let path = entry_path(state_dir, FLAG_DIR, name)?;
    create_parents(&path)?;
    platform::private_options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    Ok(())
}

/// Clear a flag. Clearing an absent flag is not an error.
pub fn flag_clear(state_dir: &Path, name: &str) -> io::Result<()> {
    let path = entry_path(state_dir, FLAG_DIR, name)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether the flag's file exists.
pub fn flag_is_set(state_dir: &Path, name: &str) -> io::Result<bool> {
    let path = entry_path(state_dir, FLAG_DIR, name)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Flip a flag and return its new state: `true` when it is now set.
pub fn flag_toggle(state_dir: &Path, name: &str) -> io::Result<bool> {
    if flag_is_set(state_dir, name)? {
        flag_clear(state_dir, name)?;
        Ok(false)
    } else {
        flag_set(state_dir, name)?;
        Ok(true)
    }
}

/// Read a value. Returns `None` when no value has been written. Stored bytes
/// that are not UTF-8, or that exceed [`MAX_VALUE_BYTES`], are refused.
pub fn value_read(state_dir: &Path, name: &str) -> io::Result<Option<String>> {
    let path = entry_path(state_dir, VALUE_DIR, name)?;
    let Some(bytes) = read_bounded(&path, MAX_VALUE_BYTES)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stored value is not UTF-8"))
}

/// Replace a value atomically. Text over [`MAX_VALUE_BYTES`] is refused before
/// anything is created.
pub fn value_write(state_dir: &Path, name: &str, text: &str) -> io::Result<()> {
    let path = entry_path(state_dir, VALUE_DIR, name)?;
    if text.len() > MAX_VALUE_BYTES {
        return Err(invalid(format!(
            "value exceeds the {MAX_VALUE_BYTES}-byte limit"
        )));
    }
    create_parents(&path)?;
    replace_contents(&path, text.as_bytes())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Read the last recorded debounce time. An unreadable or over-long record
/// counts as absent, so a damaged record cannot block forever. Other I/O
/// errors are returned.
fn read_stamp(path: &Path) -> io::Result<Option<u64>> {
    match read_bounded(path, MAX_STAMP_BYTES) {
        Ok(None) => Ok(None),
        Ok(Some(bytes)) => Ok(std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(None),
        Err(error) => Err(error),
    }
}

/// Allow one event per `window`. Returns `true` and records `now` when no time
/// is recorded, when the recorded time is at least `window` before `now`, or
/// when the recorded time lies in the future (a clock step must not block
/// forever). Returns `false` and records nothing otherwise. Times are whole
/// milliseconds.
///
/// The caller supplies `now` (for example, the time since the Unix epoch), so
/// tests are deterministic. This is a read-then-write without a lock: two
/// concurrent callers can both see an expired window and both return `true`.
pub fn debounce(state_dir: &Path, name: &str, window: Duration, now: Duration) -> io::Result<bool> {
    let path = entry_path(state_dir, DEBOUNCE_DIR, name)?;
    let now_ms = millis(now);
    if let Some(last) = read_stamp(&path)? {
        if last <= now_ms && now_ms - last < millis(window) {
            return Ok(false);
        }
    }
    create_parents(&path)?;
    replace_contents(&path, now_ms.to_string().as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn refused(name: &str) -> bool {
        validate_name(name).is_err_and(|error| error.kind() == io::ErrorKind::InvalidInput)
    }

    #[test]
    fn plain_and_nested_names_are_accepted() {
        for name in ["flag", "a/b", "a-b_c.d", "x/y/z", "x.y/z-1", "...", "a..b"] {
            assert!(validate_name(name).is_ok(), "{name} should be accepted");
        }
    }

    #[test]
    fn hostile_names_are_refused() {
        let long_segment = "a".repeat(MAX_SEGMENT_BYTES + 1);
        let long_name = format!("{}/{}", "a".repeat(MAX_NAME_BYTES), "b");
        let cases = [
            "",
            ".",
            "..",
            "../x",
            "x/..",
            "a/../b",
            "a//b",
            "a/./b",
            "a/",
            "/abs",
            "/",
            "a\\b",
            "..\\x",
            "a\0b",
            "a b",
            "a:b",
            "caf\u{e9}",
            "CON",
            "nul.txt",
            "lpt1",
            "Com9.log",
            long_segment.as_str(),
            long_name.as_str(),
        ];
        for name in cases {
            assert!(refused(name), "{name:?} should be refused");
        }
    }

    #[test]
    fn names_are_checked_before_touching_disk() {
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("state");
        fs::create_dir(&state).unwrap();
        assert!(flag_set(&state, "../escape").is_err());
        assert!(value_write(&state, "../escape", "x").is_err());
        assert!(debounce(&state, "../escape", at(10), at(0)).is_err());
        assert!(!base.path().join("escape").exists());
        assert!(!state.join("flags").exists());
        assert!(!state.join("values").exists());
        assert!(!state.join("debounce").exists());
    }

    #[test]
    fn flag_set_clear_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!flag_is_set(dir.path(), "reboot-required").unwrap());
        flag_set(dir.path(), "reboot-required").unwrap();
        assert!(flag_is_set(dir.path(), "reboot-required").unwrap());
        flag_set(dir.path(), "reboot-required").unwrap();
        assert!(flag_is_set(dir.path(), "reboot-required").unwrap());
        flag_clear(dir.path(), "reboot-required").unwrap();
        assert!(!flag_is_set(dir.path(), "reboot-required").unwrap());
    }

    #[test]
    fn clearing_an_absent_flag_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        flag_clear(dir.path(), "never/set").unwrap();
        assert!(!flag_is_set(dir.path(), "never/set").unwrap());
    }

    #[test]
    fn nested_flag_creates_parents_and_reads_do_not() {
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("state");
        assert!(!flag_is_set(&state, "a/b").unwrap());
        assert!(!state.exists(), "a read must not create directories");
        flag_set(&state, "a/b").unwrap();
        assert!(state.join("flags").join("a").join("b").is_file());
    }

    #[test]
    fn toggle_flips_and_reports_the_new_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(flag_toggle(dir.path(), "hypr/blur").unwrap());
        }
        assert_eq!(seen, [true, false, true, false]);
        assert!(!flag_is_set(dir.path(), "hypr/blur").unwrap());
        assert!(flag_toggle(dir.path(), "hypr/blur").unwrap());
        assert!(flag_is_set(dir.path(), "hypr/blur").unwrap());
    }

    #[test]
    fn absent_value_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(value_read(dir.path(), "missing/value").unwrap(), None);
    }

    #[test]
    fn value_round_trips_multi_byte_text_and_replaces_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let first = "h\u{e9}llo \u{2014} \u{65e5}\u{672c}\u{8a9e} \u{1f642}\n";
        value_write(dir.path(), "greeting", first).unwrap();
        assert_eq!(
            value_read(dir.path(), "greeting").unwrap().as_deref(),
            Some(first)
        );
        value_write(dir.path(), "greeting", "short").unwrap();
        assert_eq!(
            value_read(dir.path(), "greeting").unwrap().as_deref(),
            Some("short")
        );
        value_write(dir.path(), "empty", "").unwrap();
        assert_eq!(
            value_read(dir.path(), "empty").unwrap().as_deref(),
            Some("")
        );
    }

    #[test]
    fn value_write_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        value_write(dir.path(), "k/v", "one").unwrap();
        value_write(dir.path(), "k/v", "two").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path().join("values").join("k"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, ["v"]);
    }

    #[test]
    fn oversized_value_is_refused_before_anything_is_created() {
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("state");
        let big = "x".repeat(MAX_VALUE_BYTES + 1);
        let error = value_write(&state, "big", &big).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!state.exists());
    }

    #[test]
    fn stored_non_utf8_value_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values").join("raw");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let error = value_read(dir.path(), "raw").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn debounce_allows_first_then_blocks_inside_the_window() {
        let dir = tempfile::tempdir().unwrap();
        assert!(debounce(dir.path(), "volume", at(250), at(1_000)).unwrap());
        assert!(!debounce(dir.path(), "volume", at(250), at(1_000)).unwrap());
        assert!(!debounce(dir.path(), "volume", at(250), at(1_249)).unwrap());
    }

    #[test]
    fn debounce_window_boundary_is_inclusive_and_refusals_do_not_extend_it() {
        let dir = tempfile::tempdir().unwrap();
        assert!(debounce(dir.path(), "press", at(250), at(0)).unwrap());
        // Refused at 100 and 249; the record stays at 0, so 250 is allowed.
        assert!(!debounce(dir.path(), "press", at(250), at(100)).unwrap());
        assert!(!debounce(dir.path(), "press", at(250), at(249)).unwrap());
        assert!(debounce(dir.path(), "press", at(250), at(250)).unwrap());
        assert!(!debounce(dir.path(), "press", at(250), at(499)).unwrap());
    }

    #[test]
    fn debounce_names_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(debounce(dir.path(), "a", at(1_000), at(5)).unwrap());
        assert!(debounce(dir.path(), "b", at(1_000), at(5)).unwrap());
        assert!(!debounce(dir.path(), "a", at(1_000), at(6)).unwrap());
    }

    #[test]
    fn zero_window_always_allows() {
        let dir = tempfile::tempdir().unwrap();
        assert!(debounce(dir.path(), "z", at(0), at(7)).unwrap());
        assert!(debounce(dir.path(), "z", at(0), at(7)).unwrap());
    }

    #[test]
    fn future_record_is_stale_so_a_clock_step_cannot_block() {
        let dir = tempfile::tempdir().unwrap();
        assert!(debounce(dir.path(), "step", at(250), at(10_000)).unwrap());
        // The clock stepped back: the recorded time is now in the future.
        assert!(debounce(dir.path(), "step", at(250), at(20)).unwrap());
        assert!(!debounce(dir.path(), "step", at(250), at(100)).unwrap());
    }

    #[test]
    fn damaged_debounce_record_counts_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debounce").join("junk");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not a number").unwrap();
        assert!(debounce(dir.path(), "junk", at(250), at(5)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn created_directories_and_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("state");
        value_write(&state, "private/value", "x").unwrap();
        let dir_mode = fs::metadata(state.join("values").join("private"))
            .unwrap()
            .permissions()
            .mode();
        let file_mode = fs::metadata(state.join("values").join("private").join("value"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o077, 0);
        assert_eq!(file_mode & 0o077, 0);
    }
}
