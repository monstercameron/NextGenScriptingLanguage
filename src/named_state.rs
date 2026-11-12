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
//! Unix. Files are created owner-only (0600). Reads never create directories or
//! entries; a flag read may create the hidden lock file beside the flag.
//!
//! Every operation refuses a state path whose existing components include a
//! symbolic link or, on Windows, a reparse point. Flag and debounce operations
//! take an exclusive advisory lock on a sibling `.<name>~lock` file for their
//! read-decide-write, so concurrent callers in other processes or threads are
//! serialised.

use std::fs::{self, DirBuilder, File, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

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

/// Longest a caller waits for another holder of an entry's lock.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// Pause between attempts to take a busy entry lock.
const LOCK_POLL: Duration = Duration::from_millis(1);

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

/// Whether `metadata` describes a symbolic link or, on Windows, a reparse point.
fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        platform::windows::is_reparse(metadata)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Probe one path component: `None` when it does not exist, otherwise whether
/// it is a link.
fn probe_link(path: &Path) -> io::Result<Option<bool>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(is_link(&metadata))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Walk `chain` in order and refuse the first component that `probe` reports as
/// a link. A missing component ends the walk, since nothing below it can exist.
/// The probe is a parameter so the walk can be tested without creating links.
fn refuse_links<F>(chain: &[PathBuf], mut probe: F) -> io::Result<()>
where
    F: FnMut(&Path) -> io::Result<Option<bool>>,
{
    for component in chain {
        match probe(component)? {
            None => return Ok(()),
            Some(false) => {}
            Some(true) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing state path {}: it is a symbolic link or reparse point",
                        component.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Validate `name` and map it under `state_dir/<kind>/`. The path is refused
/// when any existing directory from the state directory down to the entry's
/// parent is a link. This runs before any read or write, so a planted link
/// cannot redirect either. The entry itself is checked under its lock, by
/// [`lock_entry`].
fn entry_path(state_dir: &Path, kind: &str, name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    let mut current = state_dir.join(kind);
    let mut chain = vec![state_dir.to_path_buf(), current.clone()];
    let mut segments = name.split('/').peekable();
    while let Some(segment) = segments.next() {
        current.push(segment);
        if segments.peek().is_some() {
            chain.push(current.clone());
        }
    }
    refuse_links(&chain, probe_link)?;
    Ok(current)
}

/// An exclusive advisory lock on the sibling lock file of one entry. Dropping
/// it releases the OS lock.
struct EntryLock {
    _file: File,
}

/// Take the exclusive lock for `path`, waiting up to [`LOCK_WAIT`], then refuse
/// the entry if it is a link. The parent directory must exist.
///
/// Every access to an entry happens under its lock. On Windows a handle held by
/// a probe that overlaps a removal leaves the name pending deletion, so a create
/// of the same name then fails. Probing the entry only here, under the lock,
/// keeps those handles from overlapping.
///
/// This uses the same primitive as the applets' path lock (`File::try_lock`,
/// flock on Unix and LockFileEx on Windows). It does not call that helper,
/// which lives in a private applets module, nor `agent::instance`'s named
/// locks, whose lock directory must be owned by the user and not writable by
/// others. Requiring that here would change what existing callers see.
///
/// The lock file is `.<name>~lock` beside the entry. `~` never appears in a
/// validated name, so the lock cannot be mistaken for an entry. Like the
/// temporary files, it is left in place, so every locker shares one inode.
fn lock_entry(path: &Path) -> io::Result<EntryLock> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("state name has no file name"))?;
    let lock_path = path.with_file_name(format!(".{file_name}~lock"));
    refuse_links(std::slice::from_ref(&lock_path), probe_link)?;
    let mut options = platform::private_options();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // No-follow so a link planted after the probe is refused, not opened.
        // Nonblocking so a FIFO planted at the lock path cannot hang the open.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(&lock_path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid(format!(
            "lock path is not a regular file: {}",
            lock_path.display()
        )));
    }
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => {
                refuse_links(&[path.to_path_buf()], probe_link)?;
                return Ok(EntryLock { _file: file });
            }
            Err(TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
                thread::sleep(LOCK_POLL);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for lock {}", lock_path.display()),
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
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
    let _lock = lock_entry(&path)?;
    flag_create(&path)
}

/// Create the flag's file if it is absent. The caller holds the entry lock.
fn flag_create(path: &Path) -> io::Result<()> {
    platform::private_options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(())
}

/// Remove the flag's file if it exists. The caller holds the entry lock.
fn flag_remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether a flag's file exists at `path`.
fn flag_present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Clear a flag. Clearing an absent flag is not an error. When the flag's
/// directory does not exist, the flag is absent, so nothing is created here.
pub fn flag_clear(state_dir: &Path, name: &str) -> io::Result<()> {
    let path = entry_path(state_dir, FLAG_DIR, name)?;
    if !path.parent().is_some_and(Path::is_dir) {
        return Ok(());
    }
    let _lock = lock_entry(&path)?;
    flag_remove(&path)
}

/// Whether the flag's file exists. The read takes the entry lock, so it cannot
/// overlap a concurrent toggle. A missing directory means the flag is absent.
pub fn flag_is_set(state_dir: &Path, name: &str) -> io::Result<bool> {
    let path = entry_path(state_dir, FLAG_DIR, name)?;
    if !path.parent().is_some_and(Path::is_dir) {
        return Ok(false);
    }
    let _lock = lock_entry(&path)?;
    flag_present(&path)
}

/// Flip a flag and return its new state: `true` when it is now set. The read
/// and the write happen under the entry lock, so concurrent toggles leave the
/// flag in the state that the same number of serial toggles would, and each
/// returned state matches some serial order.
pub fn flag_toggle(state_dir: &Path, name: &str) -> io::Result<bool> {
    let path = entry_path(state_dir, FLAG_DIR, name)?;
    create_parents(&path)?;
    let _lock = lock_entry(&path)?;
    if flag_present(&path)? {
        flag_remove(&path)?;
        Ok(false)
    } else {
        flag_create(&path)?;
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
/// tests are deterministic. The read, the decision and the write happen under
/// the entry lock, so of any number of concurrent callers in one window, exactly
/// one returns `true`.
pub fn debounce(state_dir: &Path, name: &str, window: Duration, now: Duration) -> io::Result<bool> {
    let path = entry_path(state_dir, DEBOUNCE_DIR, name)?;
    create_parents(&path)?;
    let _lock = lock_entry(&path)?;
    let now_ms = millis(now);
    if let Some(last) = read_stamp(&path)? {
        if last <= now_ms && now_ms - last < millis(window) {
            return Ok(false);
        }
    }
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

    #[cfg(unix)]
    fn assert_refused<T: std::fmt::Debug>(result: io::Result<T>) {
        let error = result.expect_err("a state path through a link must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn concurrent_debounce_allows_exactly_one_in_a_window() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let callers = 50;
        let barrier = Arc::new(Barrier::new(callers));
        let handles: Vec<_> = (0..callers)
            .map(|_| {
                let state = state.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    debounce(&state, "press", at(250), at(1_000))
                })
            })
            .collect();
        let allowed = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .filter(|allowed| *allowed)
            .count();
        assert_eq!(allowed, 1);
    }

    #[test]
    fn concurrent_toggles_match_a_serial_sequence() {
        use std::sync::{Arc, Barrier};
        for callers in [50_usize, 51] {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().to_path_buf();
            let barrier = Arc::new(Barrier::new(callers));
            let handles: Vec<_> = (0..callers)
                .map(|_| {
                    let state = state.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        flag_toggle(&state, "hypr/blur")
                    })
                })
                .collect();
            let results: Vec<bool> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap().unwrap())
                .collect();
            // Serial toggles from a clear flag return set, clear, set, ... so
            // the results hold ceil(n / 2) sets and the flag ends set when n is odd.
            let sets = results.iter().filter(|set| **set).count();
            assert_eq!(sets, callers.div_ceil(2), "{callers} toggles");
            assert_eq!(
                flag_is_set(&state, "hypr/blur").unwrap(),
                callers % 2 == 1,
                "{callers} toggles"
            );
        }
    }

    #[test]
    fn link_walk_refuses_the_first_linked_component_and_stops_at_a_missing_one() {
        let chain: Vec<PathBuf> = ["state", "state/flags", "state/flags/a", "state/flags/a/b"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let mut probed = 0;
        let error = refuse_links(&chain, |path| {
            probed += 1;
            Ok(Some(path.ends_with("a")))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(probed, 3, "components below the link are never probed");

        let mut probed = 0;
        refuse_links(&chain, |_| {
            probed += 1;
            Ok(if probed == 2 { None } else { Some(false) })
        })
        .unwrap();
        assert_eq!(probed, 2, "a missing component ends the walk");

        refuse_links(&chain, |_| Ok(Some(false))).unwrap();
    }

    #[test]
    fn probe_reports_missing_and_plain_directories_as_not_linked() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(probe_link(&dir.path().join("absent")).unwrap(), None);
        assert_eq!(probe_link(dir.path()).unwrap(), Some(false));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_directory_is_refused_before_any_write() {
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("target");
        fs::create_dir(&target).unwrap();
        let state = base.path().join("state");
        std::os::unix::fs::symlink(&target, &state).unwrap();
        assert_refused(flag_set(&state, "reboot-required"));
        assert_refused(flag_clear(&state, "reboot-required"));
        assert_refused(flag_is_set(&state, "reboot-required"));
        assert_refused(flag_toggle(&state, "reboot-required"));
        assert_refused(value_write(&state, "k", "v"));
        assert_refused(value_read(&state, "k"));
        assert_refused(debounce(&state, "d", at(250), at(0)));
        assert!(
            fs::read_dir(&target).unwrap().next().is_none(),
            "nothing was written through the link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_kind_directory_and_entry_are_refused() {
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("state");
        fs::create_dir(&state).unwrap();
        let elsewhere = base.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, state.join("flags")).unwrap();
        assert_refused(flag_set(&state, "a"));
        assert_refused(flag_is_set(&state, "a"));
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());

        fs::remove_file(state.join("flags")).unwrap();
        fs::create_dir_all(state.join("flags")).unwrap();
        let victim = base.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, state.join("flags").join("b")).unwrap();
        assert_refused(flag_clear(&state, "b"));
        assert_refused(flag_toggle(&state, "b"));
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
    }
}
