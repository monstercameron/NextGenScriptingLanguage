//! Temporary file and directory scopes. Each scope owns a fresh private directory under
//! the system temporary directory, and its path is removed when the scope ends. A run
//! that ends for any reason drops the stack, which removes whatever is still open. A
//! killed process cannot run that cleanup, so the first scope a process creates also
//! removes scope directories whose owner process is gone. That sweep runs once per
//! process, when the first scope is created, so startup pays nothing for it.
use crate::ast::TempKind;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const SCOPE_PREFIX: &str = "execute-scope-";
const OWNER_FILE: &str = "owner";
const ITEM: &str = "item";

struct OpenScope {
    /// Index of the scope's opening step, so its closing step can find it.
    header: usize,
    /// The scope directory. Dropping the scope removes it and everything in it.
    root: PathBuf,
}

impl Drop for OpenScope {
    fn drop(&mut self) {
        // Best effort: a failed removal leaves a directory the next sweep can reclaim.
        let _ = fs::remove_dir_all(&self.root);
    }
}

static SCOPE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh scope directory name. Creation is exclusive, so a clash is retried with a
/// new name rather than shared.
fn create_scope_root(temp: &Path) -> Result<PathBuf> {
    for _ in 0..100 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let number = SCOPE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = temp.join(format!(
            "{SCOPE_PREFIX}{}-{nanos:x}-{number}",
            std::process::id()
        ));
        match create_private_dir(&root) {
            Ok(()) => return Ok(root),
            Err(error) if root.exists() => drop(error),
            Err(error) => return Err(error),
        }
    }
    bail!("could not create a unique temporary scope directory")
}

/// The scopes open in one run, innermost last.
#[derive(Default)]
pub(crate) struct ScopeStack {
    scopes: Vec<OpenScope>,
}

impl ScopeStack {
    /// Create an empty file or an empty directory for the scope opened at `header`,
    /// and return its path. The path stays valid until the scope closes.
    pub(crate) fn open(&mut self, header: usize, kind: TempKind) -> Result<PathBuf> {
        static SWEPT: OnceLock<()> = OnceLock::new();
        SWEPT.get_or_init(|| sweep_stale(&std::env::temp_dir()));
        let root = create_scope_root(&std::env::temp_dir())?;
        // Owns the directory from here on, so any later failure still removes it.
        let scope = OpenScope {
            header,
            root: root.clone(),
        };
        write_owner(&root)?;
        let path = root.join(ITEM);
        match kind {
            TempKind::File => {
                crate::platform::private_options()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .with_context(|| format!("creating temporary file {}", path.display()))?;
            }
            TempKind::Directory => create_private_dir(&path)?,
        }
        self.scopes.push(scope);
        Ok(path)
    }

    /// Close the scope opened at `header`. Scopes opened inside it and left open by a
    /// jump out of their body close too, innermost first.
    pub(crate) fn close(&mut self, header: usize) {
        while self
            .scopes
            .last()
            .is_some_and(|scope| scope.header >= header)
        {
            self.scopes.pop();
        }
    }
}

fn write_owner(root: &Path) -> Result<()> {
    let mut file = crate::platform::private_options()
        .write(true)
        .create_new(true)
        .open(root.join(OWNER_FILE))
        .context("recording the temporary scope owner")?;
    #[cfg(target_os = "linux")]
    {
        let pid = std::process::id();
        let start = process_start_time(pid).context("reading temporary scope owner identity")?;
        write!(file, "{pid}:{start}").context("recording the temporary scope owner")
    }
    #[cfg(not(target_os = "linux"))]
    {
        write!(file, "{}", std::process::id()).context("recording the temporary scope owner")
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating temporary directory {}", path.display()))
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir(path).with_context(|| format!("creating temporary directory {}", path.display()))
}

/// Remove scope directories left by processes that no longer exist. Only directories
/// that are real directories, not links, and owned by this user are touched.
fn sweep_stale(temp: &Path) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(SCOPE_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() || !owned_by_current_user(&meta) {
            continue;
        }
        let owner = path.join(OWNER_FILE);
        let Ok(owner_meta) = fs::symlink_metadata(&owner) else {
            continue;
        };
        if owner_meta.file_type().is_symlink() {
            continue;
        }
        let Some(owner_text) = fs::read_to_string(&owner).ok() else {
            continue;
        };
        if !scope_owner_alive(&owner_text) {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(target_os = "linux")]
fn scope_owner_alive(owner: &str) -> bool {
    let Some((pid, start)) = owner.trim().split_once(':') else {
        // Older or malformed records cannot be proven stale, so retain them.
        return true;
    };
    let (Ok(pid), Ok(start)) = (pid.parse::<u32>(), start.parse::<u64>()) else {
        return true;
    };
    match process_start_time(pid) {
        // A missing PID can belong to another PID namespace sharing this temp directory.
        // Keep its scope; only a visible, different process start proves PID reuse.
        None => true,
        Some(current_start) => current_start == start,
    }
}

#[cfg(not(target_os = "linux"))]
fn scope_owner_alive(owner: &str) -> bool {
    owner.trim().parse::<u32>().map_or(true, process_alive)
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_command = stat.rsplit_once(')')?.1;
    after_command
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()
}

#[cfg(unix)]
fn owned_by_current_user(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    meta.uid() == crate::platform::unix_ffi::effective_uid()
}

#[cfg(not(unix))]
fn owned_by_current_user(_meta: &fs::Metadata) -> bool {
    // The Windows temporary directory is per user.
    true
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM means the process exists but belongs to someone else.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: OpenProcess with a numeric pid has no pointer arguments; a returned
    // handle is closed exactly once below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // SAFETY: GetLastError reads this thread's last-error value, which OpenProcess set when it
        // returned NULL. The is_null check is plain Rust and makes no Win32 call, so no later call
        // has overwritten that value before GetLastError runs.
        // Access denied means the process exists.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    // SAFETY: handle is non-null, so OpenProcess succeeded and returned a handle that this
    // function owns. It is closed exactly once here and not used after this call.
    unsafe { CloseHandle(handle) };
    true
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_scope_removes_its_path_and_an_outer_close_removes_inner_scopes() {
        let mut stack = ScopeStack::default();
        let outer = stack.open(0, TempKind::Directory).unwrap();
        let inner = stack.open(3, TempKind::File).unwrap();
        assert!(outer.is_dir());
        assert!(inner.is_file());
        stack.close(3);
        assert!(!inner.exists());
        assert!(outer.is_dir());
        stack.close(0);
        assert!(!outer.exists());
    }

    #[test]
    fn a_file_scope_starts_empty_and_is_private() {
        let mut stack = ScopeStack::default();
        let path = stack.open(0, TempKind::File).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        stack.close(0);
    }

    #[test]
    fn a_sweep_removes_only_scopes_whose_owner_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let dead = temp.path().join(format!("{SCOPE_PREFIX}dead"));
        let live = temp.path().join(format!("{SCOPE_PREFIX}live"));
        let unknown = temp.path().join(format!("{SCOPE_PREFIX}unknown"));
        let other = temp.path().join("unrelated");
        for dir in [&dead, &live, &unknown, &other] {
            fs::create_dir(dir).unwrap();
        }
        #[cfg(target_os = "linux")]
        {
            let start = process_start_time(std::process::id()).unwrap();
            fs::write(
                dead.join(OWNER_FILE),
                format!("{}:{}", std::process::id(), start.saturating_add(1)),
            )
            .unwrap();
            fs::write(
                live.join(OWNER_FILE),
                format!("{}:{start}", std::process::id()),
            )
            .unwrap();
            fs::write(unknown.join(OWNER_FILE), "4000000000:1").unwrap();
        }
        #[cfg(not(target_os = "linux"))]
        {
            // A pid this large is not a live process on any supported system.
            fs::write(dead.join(OWNER_FILE), "4000000000").unwrap();
            fs::write(live.join(OWNER_FILE), std::process::id().to_string()).unwrap();
        }
        sweep_stale(temp.path());
        assert!(!dead.exists());
        assert!(live.exists());
        #[cfg(target_os = "linux")]
        assert!(unknown.exists());
        assert!(other.exists());
    }
}
