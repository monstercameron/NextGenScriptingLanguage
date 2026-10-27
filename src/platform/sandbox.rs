//! OS-backed native launch confinement for untrusted mode and required grants.
//!
//! The supported paths are intentionally narrow: macOS uses the system
//! `sandbox-exec` profile runner, Linux uses bubblewrap, and other platforms
//! fail before the applet is started. No path falls back to an unsandboxed
//! command when a required sandbox cannot be established.

use crate::capabilities::AppletGrant;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::anyhow;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::Context;
use anyhow::{bail, Result};
#[cfg(target_os = "linux")]
use std::cell::RefCell;
#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::fmt;
#[cfg(target_os = "macos")]
use std::io::Read;
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;
use std::process::Command;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Stdio;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Child, ExitStatus};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
const MAC_SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Classified only when a basic, permissive no-op probe proves that the OS
/// facility itself cannot be used. Invalid applet profiles remain hard errors.
#[derive(Debug)]
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct SandboxUnavailable(String);

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl fmt::Display for SandboxUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl std::error::Error for SandboxUnavailable {}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(crate) fn is_unavailable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SandboxUnavailable>().is_some()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn unavailable(reason: impl Into<String>) -> anyhow::Error {
    SandboxUnavailable(reason.into()).into()
}

/// Verify the selected OS facility and every configured path grant before any
/// workflow applet or host file operation can have effects.
pub(crate) fn preflight(grant: &AppletGrant, binary: &Path, cwd: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        preflight_with_probe(grant, binary, cwd, || {
            probe_macos_sandbox()?;
            probe_macos_grant(grant, cwd)
        })
    }
    #[cfg(target_os = "linux")]
    {
        let bwrap = find_bwrap(cwd)?;
        validate_linux_paths(grant, binary, cwd)?;
        cache_successful_probe(&bwrap, grant, binary, cwd, || {
            probe_bwrap(&bwrap, grant, binary, cwd)
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (grant, binary, cwd);
        bail!("required applet sandbox is unsupported on this platform");
    }
}

#[cfg(target_os = "linux")]
type ProbeKey = (PathBuf, String, PathBuf, PathBuf, String);

#[cfg(target_os = "linux")]
type ProbeCache = HashSet<ProbeKey>;

#[cfg(target_os = "linux")]
thread_local! {
    static ACTIVE_PROBE_CACHE: RefCell<Option<ProbeCache>> = const { RefCell::new(None) };
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static TEST_BWRAP_PROBE_CHILDREN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Successful probes are reused only in the thread that owns the runtime run.
/// Worker threads without an explicit run context bypass the cache, preventing
/// reuse across threads with different per-thread credentials or namespaces.
#[cfg(target_os = "linux")]
pub(crate) struct ProbeRunGuard(Option<ProbeCache>);

#[cfg(target_os = "linux")]
pub(crate) fn begin_probe_run() -> ProbeRunGuard {
    let previous = ACTIVE_PROBE_CACHE.with(|active| active.replace(Some(HashSet::new())));
    ProbeRunGuard(previous)
}

#[cfg(target_os = "linux")]
pub(crate) fn begin_probe_run_if_missing() -> Option<ProbeRunGuard> {
    let active = ACTIVE_PROBE_CACHE.with(|active| active.borrow().is_some());
    (!active).then(begin_probe_run)
}

#[cfg(target_os = "linux")]
impl Drop for ProbeRunGuard {
    fn drop(&mut self) {
        ACTIVE_PROBE_CACHE.with(|active| {
            active.replace(self.0.take());
        });
    }
}

#[cfg(target_os = "linux")]
fn cache_successful_probe(
    bwrap: &Path,
    grant: &AppletGrant,
    binary: &Path,
    cwd: &Path,
    probe: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if !ACTIVE_PROBE_CACHE.with(|active| active.borrow().is_some()) {
        return probe();
    }
    // The applet-specific mount probe depends on all four inputs. Keep the
    // validated path grant in the key so a successful probe is never reused
    // for a different executable or working directory.
    let grant =
        serde_json::to_string(grant).context("serializing sandbox grant for probe cache")?;
    let Some(identity) = execution_identity(bwrap) else {
        return probe();
    };
    let key = (
        bwrap.to_path_buf(),
        grant,
        binary.to_path_buf(),
        cwd.to_path_buf(),
        identity.clone(),
    );
    ACTIVE_PROBE_CACHE.with(|active| {
        let mut active = active.borrow_mut();
        let cache = active
            .as_mut()
            .expect("cache presence was checked immediately before borrowing");
        remember_successful_probe(cache, key, probe)
    })
}

#[cfg(target_os = "linux")]
fn remember_successful_probe(
    cache: &mut ProbeCache,
    key: ProbeKey,
    probe: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if cache.contains(&key) {
        return Ok(());
    }
    probe()?;
    cache.insert(key);
    Ok(())
}

#[cfg(target_os = "linux")]
fn execution_identity(bwrap: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;

    let mut identity = String::new();
    for namespace in ["user", "pid", "mnt", "net", "ipc", "uts"] {
        let path = Path::new("/proc/thread-self/ns").join(namespace);
        let namespace_id = std::fs::read_link(path).ok()?;
        identity.push_str(&format!(" {namespace}={}", namespace_id.display()));
    }
    {
        let metadata = std::fs::metadata(bwrap).ok()?;
        use std::fmt::Write as _;
        let _ = write!(
            identity,
            " bwrap={}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.mode()
        );
    }
    let status = std::fs::read_to_string("/proc/thread-self/status").ok()?;
    let mut identity_fields = 0;
    for line in status.lines().filter(|line| {
        line.starts_with("Uid:")
            || line.starts_with("Gid:")
            || line.starts_with("Groups:")
            || line.starts_with("CapEff:")
            || line.starts_with("CapPrm:")
            || line.starts_with("CapBnd:")
            || line.starts_with("CapInh:")
            || line.starts_with("CapAmb:")
            || line.starts_with("NoNewPrivs:")
            || line.starts_with("Seccomp:")
    }) {
        identity.push(' ');
        identity.push_str(line);
        identity_fields += 1;
    }
    (identity_fields == 10).then_some(identity)
}

#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
fn preflight_with_probe(
    grant: &AppletGrant,
    binary: &Path,
    cwd: &Path,
    probe: impl FnOnce() -> Result<()>,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        validate_macos_paths(grant, binary, cwd)?;
        probe()
    }
    #[cfg(target_os = "linux")]
    {
        validate_linux_paths(grant, binary, cwd)?;
        probe()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (grant, binary, cwd);
        let _ = probe;
        bail!("required applet sandbox is unsupported on this platform");
    }
}

/// Construct the sandbox runner command with the original executable and argv
/// passed as literal arguments. The caller applies environment filtering and
/// configures standard streams after receiving the returned command.
pub(crate) fn command<I, S>(
    binary: &Path,
    arguments: I,
    grant: &AppletGrant,
    cwd: &Path,
) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    #[cfg(target_os = "macos")]
    {
        let profile = macos_profile(grant, binary, cwd)?;
        let mut command = Command::new(MAC_SANDBOX_EXEC);
        command.arg("-p").arg(profile).arg(binary).args(arguments);
        Ok(command)
    }
    #[cfg(target_os = "linux")]
    {
        let bwrap = find_bwrap(cwd)?;
        let mut command = Command::new(bwrap);
        command.args(linux_prefix(grant, binary, cwd)?);
        command.arg(binary).args(arguments);
        Ok(command)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (binary, arguments, grant, cwd);
        bail!("required applet sandbox is unsupported on this platform");
    }
}

#[cfg(target_os = "macos")]
fn wait_bounded(mut child: Child, description: &str) -> Result<ExitStatus> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{description} timed out while probing sandbox availability");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).with_context(|| format!("waiting for {description} probe"));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_bounded(mut child: Child, description: &str) -> Result<ExitStatus> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    // SAFETY: `pid` is the live child ID returned by `Command::spawn`, and
    // `pidfd_open` does not access caller-owned memory.
    let pidfd = unsafe {
        libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0u32) as libc::c_int
    };
    if pidfd < 0 {
        // Older kernels may lack pidfd_open. Keep the bounded fallback for
        // those hosts; current Linux CI and supported distributions use the
        // blocking pidfd path below.
        return wait_bounded_fallback(&mut child, description);
    }
    // SAFETY: a nonnegative pidfd_open result is a newly owned file descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` points to one initialized pollfd that remains
        // alive for the duration of the blocking call; its fd is owned above.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result > 0 {
            return child
                .wait()
                .with_context(|| format!("waiting for {description} probe"));
        }
        if result == 0 {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{description} timed out while probing sandbox availability");
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted && Instant::now() < deadline {
            continue;
        }
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).with_context(|| format!("waiting for {description} probe"));
    }
}

#[cfg(target_os = "linux")]
fn wait_bounded_fallback(child: &mut Child, description: &str) -> Result<ExitStatus> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let pid = child.id() as libc::pid_t;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let status = child.wait();
            let _ = sender.send(status);
        });
        match receiver.recv_timeout(PROBE_TIMEOUT) {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => {
                Err(error).with_context(|| format!("waiting for {description} probe"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // SAFETY: `pid` came from the child, which remains unreaped
                // until the scoped waiter finishes; the PID cannot be reused
                // while that child is still waitable.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                let _ = receiver.recv();
                bail!("{description} timed out while probing sandbox availability");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("{description} probe waiter disconnected")
            }
        }
    })
}

#[cfg(target_os = "macos")]
fn probe_macos_sandbox() -> Result<()> {
    let metadata = std::fs::metadata(MAC_SANDBOX_EXEC)
        .map_err(|_| unavailable("macOS sandbox-exec is unavailable"))?;
    if !metadata.is_file() {
        return Err(unavailable("macOS sandbox-exec is not an executable file"));
    }
    // First prove the facility can install any profile. Only this failure is
    // classified as unavailable; a failure of the restrictive runtime profile
    // below remains a real setup failure and must not be skipped in tests.
    let (availability, availability_stderr) =
        run_macos_probe("(version 1) (allow default)", Path::new("/usr/bin/true"))
            .map_err(|_| unavailable("macOS sandbox-exec could not start"))?;
    if !availability.success() {
        return Err(unavailable(format!(
            "macOS sandbox-exec is unavailable: {}{}",
            describe_status(availability),
            sanitize_probe_stderr(&availability_stderr)
        )));
    }
    // This probes the host's SBPL implementation with a fixed no-op system
    // executable in a deny-default profile. It is not an applet launch.
    let mut profile = String::from("(version 1)\n(deny default)\n");
    append_path_rule(&mut profile, "process-exec", Path::new("/usr/bin/true"))?;
    append_macos_runtime_rules(&mut profile);
    let (status, stderr) = run_macos_probe(&profile, Path::new("/usr/bin/true"))
        .context("probing the deny-default macOS runtime profile")?;
    if !status.success() {
        bail!(
            "macOS deny-default sandbox runtime profile failed: {}{}",
            describe_status(status),
            sanitize_probe_stderr(&stderr)
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_macos_probe(profile: &str, binary: &Path) -> Result<(ExitStatus, Vec<u8>)> {
    let child = Command::new(MAC_SANDBOX_EXEC)
        .arg("-p")
        .arg(profile)
        .arg(binary)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting macOS sandbox probe")?;
    wait_bounded_capture_stderr(child, "macOS sandbox")
}

#[cfg(target_os = "macos")]
fn wait_bounded_capture_stderr(
    mut child: Child,
    description: &str,
) -> Result<(ExitStatus, Vec<u8>)> {
    const MAX_CAPTURED_STDERR: usize = 2048;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("sandbox probe stderr pipe was not created"))?;
    let reader = std::thread::spawn(move || {
        let mut captured = Vec::with_capacity(MAX_CAPTURED_STDERR);
        let mut buffer = [0u8; 512];
        loop {
            let count = match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => count,
            };
            let remaining = MAX_CAPTURED_STDERR.saturating_sub(captured.len());
            captured.extend_from_slice(&buffer[..count.min(remaining)]);
        }
        captured
    });
    let status = wait_bounded(child, description);
    let stderr = reader
        .join()
        .map_err(|_| anyhow!("sandbox probe stderr collector failed"))?;
    Ok((status?, stderr))
}

#[cfg(target_os = "macos")]
fn describe_status(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => status.signal().map_or_else(
            || "terminated by an unknown signal".to_owned(),
            |signal| format!("terminated by signal {signal}"),
        ),
    }
}

#[cfg(target_os = "macos")]
fn sanitize_probe_stderr(stderr: &[u8]) -> String {
    if stderr.is_empty() {
        return String::new();
    }
    let diagnostic = String::from_utf8_lossy(stderr)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    format!("; probe stderr: {}", diagnostic.trim())
}

#[cfg(target_os = "linux")]
fn find_bwrap(cwd: &Path) -> Result<PathBuf> {
    let _ = cwd;
    for candidate in [Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")] {
        let Ok(canonical) = std::fs::canonicalize(candidate) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&canonical) else {
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != 0
                || metadata.permissions().mode() & 0o022 != 0
                || metadata.permissions().mode() & 0o111 == 0
                || !metadata.is_file()
                || !trusted_root_owned_parents(&canonical)
            {
                continue;
            }
        }
        return Ok(canonical);
    }
    Err(unavailable(
        "a trusted root-owned bubblewrap executable is required for sandbox_required applets",
    ))
}

#[cfg(target_os = "linux")]
fn trusted_root_owned_parents(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mut parent = path.parent();
    while let Some(directory) = parent {
        let Ok(metadata) = std::fs::metadata(directory) else {
            return false;
        };
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
            return false;
        }
        parent = directory.parent();
    }
    true
}

#[cfg(target_os = "linux")]
fn probe_bwrap(bwrap: &Path, grant: &AppletGrant, binary: &Path, cwd: &Path) -> Result<()> {
    let true_binary = [Path::new("/usr/bin/true"), Path::new("/bin/true")]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow!("sandbox probe could not locate a no-op executable"))?;
    let mut command = Command::new(bwrap);
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-net",
        "--cap-drop",
        "ALL",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--dir",
        "/tmp",
        "--chmod",
        "0555",
        "/tmp",
    ]);
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        let source = Path::new(path);
        if source.exists() {
            command.args(["--dir", path, "--ro-bind", path, path]);
        }
    }
    if Path::new("/etc/ld.so.cache").exists() {
        command.args([
            "--dir",
            "/etc",
            "--ro-bind",
            "/etc/ld.so.cache",
            "/etc/ld.so.cache",
        ]);
    }
    command
        .arg("--")
        .arg(true_binary)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(test)]
    TEST_BWRAP_PROBE_CHILDREN.with(|count| count.set(count.get() + 1));
    let child = command.spawn().context("probing bubblewrap support")?;
    let status = wait_bounded(child, "bubblewrap")?;
    if !status.success() {
        return Err(unavailable(
            "bubblewrap could not establish the required namespaces",
        ));
    }
    // Also exercise this applet's exact mount/profile arguments with a fixed
    // no-op executable. Never launch the target applet during preflight.
    let mut command = Command::new(bwrap);
    command
        .args(linux_prefix(grant, binary, cwd)?)
        .arg(true_binary)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(test)]
    TEST_BWRAP_PROBE_CHILDREN.with(|count| count.set(count.get() + 1));
    let child = command
        .spawn()
        .context("probing applet-specific bubblewrap mounts")?;
    let status = wait_bounded(child, "applet-specific bubblewrap")?;
    if !status.success() {
        bail!("bubblewrap could not establish the applet-specific sandbox");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn probe_macos_grant(grant: &AppletGrant, cwd: &Path) -> Result<()> {
    let no_op = Path::new("/usr/bin/true");
    let profile = macos_profile(grant, no_op, cwd)?;
    let child = Command::new(MAC_SANDBOX_EXEC)
        .arg("-p")
        .arg(profile)
        .arg(no_op)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("probing applet-specific macOS sandbox profile")?;
    let status = wait_bounded(child, "applet-specific macOS sandbox")?;
    if !status.success() {
        bail!("macOS could not establish the applet-specific sandbox profile");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_macos_paths(grant: &AppletGrant, binary: &Path, cwd: &Path) -> Result<()> {
    if !binary.is_absolute() || !std::fs::metadata(binary).is_ok_and(|metadata| metadata.is_file())
    {
        bail!("macOS sandbox requires an existing absolute executable path");
    }
    if !cwd.is_absolute() || !cwd.is_dir() {
        bail!("macOS sandbox requires an existing absolute working directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(binary)?.permissions().mode() & 0o111 == 0 {
            bail!("macOS sandbox requires an executable file");
        }
    }
    for path in grant
        .filesystem
        .read
        .iter()
        .chain(grant.filesystem.write.iter())
    {
        let resolved = resolved_path(path, cwd)?;
        if !resolved.exists() {
            // SBPL can safely grant a future path. Existing symlink targets
            // are still canonicalized when they exist.
            if resolved
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                bail!("sandbox path grant cannot contain unresolved parent traversal");
            }
        }
        sbpl_path(&resolved)?;
    }
    sbpl_path(binary)?;
    sbpl_path(cwd)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_linux_paths(grant: &AppletGrant, binary: &Path, cwd: &Path) -> Result<()> {
    for path in grant
        .filesystem
        .read
        .iter()
        .chain(grant.filesystem.write.iter())
    {
        let resolved = resolved_path(path, cwd)?;
        if !resolved.exists() {
            bail!(
                "bubblewrap path grants must name existing files or directories; grant the parent directory to create a new file"
            );
        }
        if !resolved.is_file() && !resolved.is_dir() {
            bail!("bubblewrap path grants must name regular files or directories");
        }
    }
    if !binary.is_file() || !binary.is_absolute() {
        bail!("bubblewrap requires a resolved absolute executable path");
    }
    if !cwd.is_dir() || !cwd.is_absolute() {
        bail!("bubblewrap requires an existing absolute working directory");
    }
    if !grant
        .filesystem
        .read
        .iter()
        .chain(grant.filesystem.write.iter())
        .any(|path| resolved_path(path, cwd).is_ok_and(|root| cwd == root || cwd.starts_with(root)))
        && !is_system_runtime_path(cwd)
    {
        bail!("sandboxed working directory must be within an explicit filesystem grant");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_profile(grant: &AppletGrant, binary: &Path, cwd: &Path) -> Result<String> {
    let binary_real = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_path_buf());
    let mut profile = String::from("(version 1)\n(deny default)\n");
    profile.push_str(
        "(allow sysctl-read)\n(allow process-info* (target self))\n(allow signal (target self))\n",
    );
    let process_exec = grant.capabilities.contains("process.exec");
    append_macos_runtime_rules(&mut profile);
    if process_exec {
        profile.push_str(
            "(allow file-read* (subpath \"/usr/bin\") (subpath \"/bin\") (subpath \"/sbin\"))\n",
        );
        profile.push_str("(allow file-read-metadata (subpath \"/usr/bin\") (subpath \"/bin\") (subpath \"/sbin\"))\n");
        profile.push_str("(allow process-fork)\n(allow process-exec)\n");
    } else {
        append_path_rule(&mut profile, "process-exec", binary)?;
        append_path_rule(&mut profile, "process-exec", &binary_real)?;
        if let Some(interpreter) = shebang_interpreter(binary)? {
            append_path_rule(&mut profile, "process-exec", &interpreter)?;
            append_path_rule(&mut profile, "file-read*", &interpreter)?;
            if let Ok(canonical) = std::fs::canonicalize(&interpreter) {
                append_path_rule(&mut profile, "process-exec", &canonical)?;
                append_path_rule(&mut profile, "file-read*", &canonical)?;
            }
        }
    }
    append_path_rule(&mut profile, "file-read*", binary)?;
    append_path_rule(&mut profile, "file-read*", &binary_real)?;
    for path in &grant.filesystem.read {
        for resolved in path_aliases(path, cwd)? {
            append_fs_rule(&mut profile, "file-read*", &resolved)?;
            append_ancestor_metadata_rules(&mut profile, &resolved)?;
        }
    }
    for path in &grant.filesystem.write {
        for resolved in path_aliases(path, cwd)? {
            append_fs_rule(&mut profile, "file-write*", &resolved)?;
            append_ancestor_metadata_rules(&mut profile, &resolved)?;
        }
    }
    for cwd_alias in path_aliases(cwd, cwd)? {
        append_ancestor_metadata_rules(&mut profile, &cwd_alias)?;
    }
    // macOS resolves the system /bin/sh variant through this root-owned
    // selector before it reaches the shell image. Permit that single fixed
    // system dispatch path for scripts with a /bin/sh shebang; other child
    // executable paths remain denied unless `process.exec` was granted.
    if shebang_interpreter(binary)?.as_deref() == Some(Path::new("/bin/sh")) {
        profile.push_str(
            "(allow file-read* (literal \"/private/var/select/sh\"))\n(allow file-read-metadata (literal \"/private/var/select\"))\n",
        );
        append_path_rule(&mut profile, "process-exec", Path::new("/bin/bash"))?;
        append_path_rule(&mut profile, "file-read*", Path::new("/bin/bash"))?;
        if let Ok(canonical) = std::fs::canonicalize("/bin/bash") {
            append_path_rule(&mut profile, "process-exec", &canonical)?;
            append_path_rule(&mut profile, "file-read*", &canonical)?;
        }
    }
    if grant.network {
        profile.push_str("(allow network*)\n");
    }
    Ok(profile)
}

#[cfg(target_os = "macos")]
fn append_macos_runtime_rules(profile: &mut String) {
    // These are the narrow runtime paths used by Apple's current system.sb,
    // plus standard special files needed by libc/dyld. A deny-default profile
    // that only allows the executable itself prevents dyld from starting.
    profile.push_str("(allow file-read* file-test-existence (subpath \"/Library/Apple\") (subpath \"/Library/Filesystems/NetFSPlugins\") (subpath \"/Library/Preferences/Logging\") (subpath \"/System\") (subpath \"/private/var/db/timezone\") (subpath \"/usr/lib\") (subpath \"/usr/share\"))\n");
    profile.push_str("(allow file-map-executable (subpath \"/Library/Apple/System/Library/Frameworks\") (subpath \"/Library/Apple/System/Library/PrivateFrameworks\") (subpath \"/Library/Apple/usr/lib\") (subpath \"/System/Library/Extensions\") (subpath \"/System/Library/Frameworks\") (subpath \"/System/Library/PrivateFrameworks\") (subpath \"/System/Library/SubFrameworks\") (subpath \"/System/iOSSupport/System/Library/Frameworks\") (subpath \"/System/iOSSupport/System/Library/PrivateFrameworks\") (subpath \"/System/iOSSupport/System/Library/SubFrameworks\") (subpath \"/System/iOSSupport/usr/lib\") (subpath \"/usr/lib\"))\n");
    profile.push_str("(allow file-read-metadata file-test-existence (literal \"/etc\") (literal \"/tmp\") (literal \"/var\") (literal \"/private/etc/localtime\"))\n");
    profile.push_str("(allow file-read* file-test-existence (literal \"/\"))\n");
    profile.push_str("(allow file-read* file-test-existence (literal \"/dev/autofs_nowait\") (literal \"/dev/random\") (literal \"/dev/urandom\") (literal \"/private/etc/master.passwd\") (literal \"/private/etc/passwd\") (literal \"/private/etc/protocols\") (literal \"/private/etc/services\"))\n");
    profile.push_str("(allow file-read* file-test-existence file-write-data (literal \"/dev/null\") (literal \"/dev/zero\"))\n");
    profile.push_str(
        "(allow file-read-data file-test-existence file-write-data (subpath \"/dev/fd\"))\n",
    );
    if Path::new("/private/var/db/dyld").exists() {
        profile.push_str("(allow file-read* (subpath \"/private/var/db/dyld\"))\n");
    }
}

#[cfg(target_os = "macos")]
fn append_path_rule(profile: &mut String, operation: &str, path: &Path) -> Result<()> {
    profile.push_str(&format!(
        "(allow {operation} (literal {}))\n",
        sbpl_quote(path)?
    ));
    Ok(())
}

#[cfg(target_os = "macos")]
fn append_fs_rule(profile: &mut String, operation: &str, path: &Path) -> Result<()> {
    let quoted = sbpl_quote(path)?;
    let filter = if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        format!("(subpath {quoted})")
    } else {
        format!("(literal {quoted})")
    };
    profile.push_str(&format!("(allow {operation} {filter})\n"));
    Ok(())
}

#[cfg(target_os = "macos")]
fn append_ancestor_metadata_rules(profile: &mut String, path: &Path) -> Result<()> {
    let mut directory = Some(path);
    while let Some(current) = directory {
        profile.push_str(&format!(
            "(allow file-read-metadata (literal {}))\n",
            sbpl_quote(current)?
        ));
        if current == Path::new("/") {
            break;
        }
        directory = current.parent();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn sbpl_path(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("macOS sandbox requires UTF-8 filesystem paths"))
}

#[cfg(target_os = "macos")]
fn sbpl_quote(path: &Path) -> Result<String> {
    let value = sbpl_path(path)?;
    if value.contains('\n') || value.contains('\r') || value.contains('\0') {
        bail!("macOS sandbox paths cannot contain control characters");
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(target_os = "macos")]
fn path_aliases(path: &Path, cwd: &Path) -> Result<Vec<PathBuf>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut values = vec![absolute.clone()];
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        if !values.contains(&canonical) {
            values.push(canonical);
        }
    }
    for value in &values {
        sbpl_path(value)?;
    }
    Ok(values)
}

#[cfg(target_os = "macos")]
fn shebang_interpreter(binary: &Path) -> Result<Option<PathBuf>> {
    let bytes =
        std::fs::read(binary).with_context(|| "reading executable header for sandbox setup")?;
    if !bytes.starts_with(b"#!") {
        return Ok(None);
    }
    let line_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(bytes.len());
    let line = std::str::from_utf8(&bytes[2..line_end])
        .context("sandboxed script has a non-UTF-8 interpreter line")?
        .trim();
    let interpreter = line.split_ascii_whitespace().next().unwrap_or_default();
    if interpreter.is_empty() || !Path::new(interpreter).is_absolute() {
        bail!("sandboxed scripts require an absolute interpreter path");
    }
    Ok(Some(PathBuf::from(interpreter)))
}

#[cfg(target_os = "linux")]
fn linux_prefix(grant: &AppletGrant, binary: &Path, cwd: &Path) -> Result<Vec<OsString>> {
    let mut args = vec![
        "--die-with-parent".into(),
        "--new-session".into(),
        "--unshare-user".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--cap-drop".into(),
        "ALL".into(),
    ];
    if !grant.network {
        args.push("--unshare-net".into());
    }
    let mut created = BTreeSet::from([PathBuf::from("/")]);
    let mut covered_roots = Vec::new();
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        let source = Path::new(path);
        if source.exists() {
            ensure_parent_directories(&mut args, source, &mut created, &covered_roots);
            args.extend([
                "--ro-bind".into(),
                source.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
            ]);
            created.insert(source.to_path_buf());
            if source.is_dir() {
                covered_roots.push(source.to_path_buf());
            }
        }
    }
    args.extend(["--dev".into(), "/dev".into()]);
    created.insert(PathBuf::from("/dev"));
    args.extend(["--proc".into(), "/proc".into()]);
    created.insert(PathBuf::from("/proc"));
    args.extend(["--dir".into(), "/tmp".into()]);
    args.extend(["--chmod".into(), "0555".into(), "/tmp".into()]);
    created.insert(PathBuf::from("/tmp"));
    args.extend(["--dir".into(), "/run".into()]);
    args.extend(["--chmod".into(), "0555".into(), "/run".into()]);
    created.insert(PathBuf::from("/run"));
    args.push("--dir".into());
    args.push("/etc".into());
    created.insert(PathBuf::from("/etc"));
    for path in [
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/passwd",
        "/etc/group",
        "/etc/hosts",
        "/etc/resolv.conf",
        "/etc/nsswitch.conf",
    ] {
        let source = Path::new(path);
        if source.exists() {
            ensure_parent_directories(&mut args, source, &mut created, &covered_roots);
            args.extend([
                "--ro-bind".into(),
                source.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
            ]);
            created.insert(source.to_path_buf());
            if source.is_dir() {
                covered_roots.push(source.to_path_buf());
            }
        }
    }
    if grant.network && Path::new("/etc/ssl/certs").exists() {
        let certs = Path::new("/etc/ssl/certs");
        ensure_parent_directories(&mut args, certs, &mut created, &covered_roots);
        args.extend([
            "--ro-bind".into(),
            "/etc/ssl/certs".into(),
            "/etc/ssl/certs".into(),
        ]);
        created.insert(certs.to_path_buf());
        covered_roots.push(certs.to_path_buf());
    }
    args.extend(["--chmod".into(), "0555".into(), "/etc".into()]);

    let binary_real = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_path_buf());
    if !is_system_runtime_path(binary) && !is_system_runtime_path(&binary_real) {
        add_bind(
            &mut args,
            &binary_real,
            binary,
            true,
            &mut created,
            &mut covered_roots,
        )?;
    }

    let mut mounts = BTreeSet::new();
    for path in &grant.filesystem.read {
        for alias in path_aliases_linux(path, cwd)? {
            mounts.insert((alias, false));
        }
    }
    for path in &grant.filesystem.write {
        for alias in path_aliases_linux(path, cwd)? {
            mounts.insert((alias, true));
        }
    }
    let mut mounts = mounts.into_iter().collect::<Vec<_>>();
    mounts.sort_by_key(|(path, _)| path.components().count());
    for (path, write) in mounts {
        add_bind(
            &mut args,
            &path,
            &path,
            !write,
            &mut created,
            &mut covered_roots,
        )?;
    }
    if !is_system_runtime_path(cwd)
        && !grant
            .filesystem
            .read
            .iter()
            .chain(grant.filesystem.write.iter())
            .any(|path| {
                path_aliases_linux(path, cwd).is_ok_and(|aliases| {
                    aliases
                        .iter()
                        .any(|root| cwd == root || cwd.starts_with(root))
                })
            })
    {
        bail!("sandboxed working directory must be explicitly granted");
    }
    args.extend(["--chdir".into(), cwd.as_os_str().to_owned()]);
    Ok(args)
}

#[cfg(target_os = "linux")]
fn path_aliases_linux(path: &Path, cwd: &Path) -> Result<Vec<PathBuf>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let canonical = std::fs::canonicalize(&absolute)
        .with_context(|| "required sandbox filesystem grant must exist before execution")?;
    Ok(if canonical == absolute {
        vec![canonical]
    } else {
        vec![absolute, canonical]
    })
}

#[cfg(target_os = "linux")]
fn add_bind(
    args: &mut Vec<OsString>,
    source: &Path,
    target: &Path,
    read_only: bool,
    created: &mut BTreeSet<PathBuf>,
    covered_roots: &mut Vec<PathBuf>,
) -> Result<()> {
    if !source.is_absolute() || !target.is_absolute() {
        bail!("bubblewrap grants require absolute paths");
    }
    ensure_parent_directories(args, target, created, covered_roots);
    args.push(if read_only { "--ro-bind" } else { "--bind" }.into());
    args.push(source.as_os_str().to_owned());
    args.push(target.as_os_str().to_owned());
    created.insert(target.to_path_buf());
    if source.is_dir() {
        covered_roots.push(target.to_path_buf());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_parent_directories(
    args: &mut Vec<OsString>,
    path: &Path,
    created: &mut BTreeSet<PathBuf>,
    covered_roots: &[PathBuf],
) {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| *ancestor != Path::new("/"))
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        if created.contains(ancestor)
            || (ancestor.exists()
                && covered_roots
                    .iter()
                    .any(|root| ancestor == root || ancestor.starts_with(root)))
        {
            created.insert(ancestor.to_path_buf());
            continue;
        }
        args.push("--dir".into());
        args.push(ancestor.as_os_str().to_owned());
        created.insert(ancestor.to_path_buf());
    }
}

#[cfg(target_os = "linux")]
fn is_system_runtime_path(path: &Path) -> bool {
    ["/usr", "/bin", "/sbin", "/lib", "/lib64"]
        .iter()
        .any(|root| path == Path::new(root) || path.starts_with(root))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolved_path(path: &Path, cwd: &Path) -> Result<PathBuf> {
    crate::capabilities::canonicalize_for_grant(path, cwd)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn grant_for(root: &Path) -> AppletGrant {
        AppletGrant {
            capabilities: BTreeSet::from(["filesystem.read".into()]),
            binary_sha256: None,
            environment: BTreeSet::new(),
            filesystem: crate::capabilities::FilesystemGrant {
                read: vec![root.to_path_buf()],
                write: vec![],
            },
            network: false,
            network_origins: BTreeSet::new(),
            sandbox_required: true,
        }
    }

    fn require_sandbox(grant: &AppletGrant, binary: &Path, cwd: &Path, label: &str) -> bool {
        match preflight(grant, binary, cwd) {
            Ok(()) => true,
            Err(error) if is_unavailable(&error) => {
                eprintln!("skipping {label}: OS sandbox facility unavailable: {error:#}");
                false
            }
            Err(error) => panic!("{label} sandbox profile failed: {error:#}"),
        }
    }

    #[test]
    fn unavailable_sandbox_probe_is_propagated_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let grant = grant_for(root.path());
        let error = preflight_with_probe(&grant, Path::new("/usr/bin/true"), root.path(), || {
            bail!("owned test sandbox facility is unavailable")
        })
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("owned test sandbox facility is unavailable"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn successful_probe_cache_is_scoped_to_one_runtime_run() {
        let root = tempfile::tempdir().unwrap();
        let grant = grant_for(root.path());
        let bwrap = PathBuf::from("/usr/bin/true");
        let binary = PathBuf::from("/usr/bin/true");
        let cwd = root.path().to_path_buf();
        let probes = std::sync::atomic::AtomicUsize::new(0);

        let first_run = begin_probe_run();
        for _ in 0..2 {
            cache_successful_probe(&bwrap, &grant, &binary, &cwd, || {
                probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(first_run);

        let second_run = begin_probe_run();
        cache_successful_probe(&bwrap, &grant, &binary, &cwd, || {
            probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(second_run);

        let outer = begin_probe_run();
        cache_successful_probe(&bwrap, &grant, &binary, &cwd, || {
            probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        let nested = begin_probe_run();
        cache_successful_probe(&bwrap, &grant, &binary, &cwd, || {
            probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        drop(nested);
        cache_successful_probe(&bwrap, &grant, &binary, &cwd, || {
            probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        drop(outer);
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    /// Linux CI acceptance measurement for issue #572. This deliberately runs
    /// in release mode and is ignored in ordinary test suites because it is a
    /// timing gate. The CI workflow invokes it explicitly after installing
    /// Bubblewrap. The sandbox and baseline each launch the same executable
    /// 100 times; sandbox preflight remains inside the timed launch loop.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "Linux CI performance acceptance; run explicitly in release mode"]
    fn required_sandbox_100_spawn_overhead_and_probe_frequency() {
        const SPAWNS: usize = 100;
        const MAX_OVERHEAD_PER_SPAWN: u128 = 1_500_000;

        let cwd = tempfile::tempdir().expect("temporary working directory");
        let binary = [Path::new("/usr/bin/true"), Path::new("/bin/true")]
            .into_iter()
            .find(|path| path.is_file())
            .expect("Linux CI must provide /usr/bin/true or /bin/true");
        let grant = grant_for(cwd.path());
        find_bwrap(cwd.path()).expect("Linux CI must install trusted Bubblewrap");

        fn launch_plain(binary: &Path, cwd: &Path) {
            let status = Command::new(binary)
                .current_dir(cwd)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("launch baseline child");
            assert!(status.success(), "baseline child failed: {status}");
        }

        fn launch_sandboxed(binary: &Path, cwd: &Path, grant: &AppletGrant) {
            preflight(grant, binary, cwd).expect("required sandbox preflight");
            let status = command(binary, std::iter::empty::<&OsStr>(), grant, cwd)
                .expect("construct required sandbox command")
                .current_dir(cwd)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("launch sandboxed child");
            assert!(status.success(), "sandboxed child failed: {status}");
        }

        // Warm up the executable and filesystem before taking paired samples.
        launch_plain(binary, cwd.path());
        let warmup_guard = begin_probe_run_if_missing();
        launch_sandboxed(binary, cwd.path(), &grant);
        drop(warmup_guard);

        let mut per_spawn_overheads = Vec::with_capacity(3);
        for sample in 0..3 {
            // Always collect the unsandboxed baseline first, as required by
            // the acceptance contract.
            let baseline_start = Instant::now();
            for _ in 0..SPAWNS {
                launch_plain(binary, cwd.path());
            }
            let baseline = baseline_start.elapsed();

            TEST_BWRAP_PROBE_CHILDREN.with(|count| count.set(0));
            let _probe_run = begin_probe_run();
            let sandbox_start = Instant::now();
            for _ in 0..SPAWNS {
                launch_sandboxed(binary, cwd.path(), &grant);
            }
            let sandboxed = sandbox_start.elapsed();
            let children = TEST_BWRAP_PROBE_CHILDREN.with(std::cell::Cell::get);
            assert_eq!(
                children, 2,
                "sample {sample}: the two Bubblewrap validation probes must run exactly once per runtime run"
            );
            let overhead =
                sandboxed.as_nanos().saturating_sub(baseline.as_nanos()) / SPAWNS as u128;
            eprintln!(
                "sample {sample}: baseline={:.3}ms sandboxed={:.3}ms overhead={:.3}us/spawn probes={children}",
                baseline.as_secs_f64() * 1_000.0,
                sandboxed.as_secs_f64() * 1_000.0,
                overhead as f64 / 1_000.0,
            );
            assert!(
                overhead <= MAX_OVERHEAD_PER_SPAWN,
                "sample {sample} required-sandbox overhead exceeded 1.5ms per spawn: {overhead}ns"
            );
            per_spawn_overheads.push(overhead);
        }

        per_spawn_overheads.sort_unstable();
        let median = per_spawn_overheads[per_spawn_overheads.len() / 2];
        eprintln!(
            "median sandbox overhead: {:.3}us/spawn across {SPAWNS} launches (each sample max {:.3}us)",
            median as f64 / 1_000.0,
            MAX_OVERHEAD_PER_SPAWN as f64 / 1_000.0,
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unowned_worker_thread_does_not_reuse_another_threads_probe() {
        let root = tempfile::tempdir().unwrap();
        let grant = grant_for(root.path());
        let bwrap = PathBuf::from("/usr/bin/true");
        let binary = PathBuf::from("/usr/bin/true");
        let cwd = root.path().to_path_buf();
        let probes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _run = begin_probe_run();
        let worker_probes = std::sync::Arc::clone(&probes);

        cache_successful_probe(&bwrap, &grant, &binary, &cwd, || {
            probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        let worker_bwrap = bwrap.clone();
        let worker_binary = binary.clone();
        let worker_cwd = cwd.clone();
        let worker_grant = grant.clone();
        std::thread::spawn(move || {
            cache_successful_probe(
                &worker_bwrap,
                &worker_grant,
                &worker_binary,
                &worker_cwd,
                || {
                    worker_probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .unwrap();
        })
        .join()
        .unwrap();
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);

        let worker_probes = std::sync::Arc::clone(&probes);
        let worker_bwrap = bwrap.clone();
        let worker_binary = binary.clone();
        let worker_cwd = cwd.clone();
        let worker_grant = grant.clone();
        std::thread::spawn(move || {
            let _run = begin_probe_run_if_missing().expect("worker owns an independent run cache");
            for _ in 0..2 {
                cache_successful_probe(
                    &worker_bwrap,
                    &worker_grant,
                    &worker_binary,
                    &worker_cwd,
                    || {
                        worker_probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    },
                )
                .unwrap();
            }
        })
        .join()
        .unwrap();
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn executable_mount_is_read_only_without_a_write_grant() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("fixture");
        std::fs::write(&binary, b"fixture").unwrap();
        let grant = grant_for(root.path());
        let args = linux_prefix(&grant, &binary, root.path()).unwrap();
        let mount = args
            .windows(3)
            .find(|args| args[1] == binary.as_os_str() && args[2] == binary.as_os_str())
            .unwrap();
        assert_eq!(mount[0], "--ro-bind");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_bubblewrap_denies_ungranted_file_read_and_write_on_owned_fixtures() {
        use std::os::unix::fs::PermissionsExt;

        let cwd = tempfile::tempdir().unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let script = cwd.path().join("sandbox-fixture.sh");
        let secret = secret_dir.path().join("ungranted-secret");
        let output = output_dir.path().join("ungranted-output");
        std::fs::write(&secret, b"hidden").unwrap();
        std::fs::write(
            &script,
            b"#!/bin/sh\nif IFS= read -r value < \"$1\"; then exit 40; fi\nif printf x > \"$2\"; then exit 41; fi\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        let grant = grant_for(cwd.path());
        if !require_sandbox(
            &grant,
            &script,
            cwd.path(),
            "bubblewrap file-access fixture",
        ) {
            return;
        }

        let status = command(
            &script,
            [secret.as_os_str(), output.as_os_str()],
            &grant,
            cwd.path(),
        )
        .unwrap()
        .status()
        .unwrap();
        assert!(status.success(), "sandbox fixture exit status: {status}");
        assert_eq!(std::fs::read(&secret).unwrap(), b"hidden");
        assert!(!output.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_bubblewrap_denies_network_without_a_grant() {
        use std::net::TcpListener;

        let python = [Path::new("/usr/bin/python3"), Path::new("/bin/python3")]
            .into_iter()
            .find(|path| path.is_file());
        let Some(python) = python else {
            eprintln!("skipping bubblewrap network integration: Python 3 is unavailable");
            return;
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let grant = grant_for(cwd.path());
        if !require_sandbox(&grant, python, cwd.path(), "bubblewrap network fixture") {
            return;
        }

        let script = format!(
            "import socket; s=socket.socket(); s.settimeout(1); s.connect(('{}', {}))",
            address.ip(),
            address.port()
        );
        let status = command(
            python,
            [OsStr::new("-c"), OsStr::new(&script)],
            &grant,
            cwd.path(),
        )
        .unwrap()
        .status()
        .unwrap();
        assert!(!status.success(), "sandboxed socket unexpectedly connected");
        assert!(
            listener.accept().is_err(),
            "sandboxed process reached the owned loopback listener"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn real_macos_sandbox_denies_ungranted_file_access_and_child_launch() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("sandbox-fixture.sh");
        let secret = root.path().join("ungranted-secret");
        let output = root.path().join("ungranted-output");
        std::fs::write(&secret, b"hidden").unwrap();
        std::fs::write(
            &script,
            b"#!/bin/sh\nif IFS= read -r value < \"$1\"; then exit 40; fi\nif printf x > \"$2\"; then exit 41; fi\nexec /bin/true\nexit 42\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        let grant = AppletGrant {
            capabilities: BTreeSet::new(),
            binary_sha256: None,
            environment: BTreeSet::new(),
            filesystem: crate::capabilities::FilesystemGrant::default(),
            network: false,
            network_origins: BTreeSet::new(),
            sandbox_required: true,
        };

        if !require_sandbox(&grant, &script, root.path(), "macOS file-access fixture") {
            return;
        }
        let output_result = command(
            &script,
            [secret.as_os_str(), output.as_os_str()],
            &grant,
            root.path(),
        )
        .unwrap()
        .current_dir(root.path())
        .output()
        .unwrap();
        assert!(
            !output_result.status.success(),
            "sandbox fixture exit status: {}; stderr: {}",
            output_result.status,
            String::from_utf8_lossy(&output_result.stderr)
        );
        assert_ne!(
            output_result.status.code(),
            Some(40),
            "ungranted read succeeded"
        );
        assert_ne!(
            output_result.status.code(),
            Some(41),
            "ungranted write succeeded"
        );
        assert_ne!(
            output_result.status.code(),
            Some(42),
            "ungranted child launch succeeded"
        );
        assert_eq!(std::fs::read(&secret).unwrap(), b"hidden");
        assert!(!output.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn real_macos_sandbox_denies_network_when_not_granted() {
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let nc = Path::new("/usr/bin/nc");
        assert!(nc.is_file(), "the macOS network fixture requires nc");
        TcpStream::connect(address).expect("owned loopback listener should accept a baseline");
        listener
            .accept()
            .expect("baseline TCP connection should arrive");
        let grant = AppletGrant {
            capabilities: BTreeSet::from(["process.exec".into()]),
            binary_sha256: None,
            environment: BTreeSet::new(),
            filesystem: crate::capabilities::FilesystemGrant::default(),
            network: false,
            network_origins: BTreeSet::new(),
            sandbox_required: true,
        };

        if !require_sandbox(&grant, nc, root.path(), "macOS network fixture") {
            return;
        }
        let output = command(
            nc,
            [
                OsStr::new("-z"),
                OsStr::new("-v"),
                OsStr::new("-w"),
                OsStr::new("1"),
                OsStr::new(&address.ip().to_string()),
                OsStr::new(&address.port().to_string()),
            ],
            &grant,
            root.path(),
        )
        .unwrap()
        .current_dir(root.path())
        .output()
        .unwrap();
        assert!(
            !output.status.success(),
            "network was reachable without a grant"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Operation not permitted"),
            "expected an OS sandbox network denial, got: {stderr}"
        );
        assert!(
            listener.accept().is_err(),
            "sandboxed applet reached the local listener"
        );
    }
}
