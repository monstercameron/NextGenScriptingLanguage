//! Same-user peer checks for local IPC sockets. The live server, the attach client,
//! and the daemon all refuse a connection whose peer is not the current user.
use super::UnixStream;

/// Whether the peer of `stream` runs as the current user.
pub(crate) fn peer_is_owner(stream: &UnixStream) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail. It only reads the
        // process's effective uid.
        peer_has_uid(stream, crate::platform::unix_ffi::effective_uid())
    }
    #[cfg(windows)]
    {
        stream.peer_is_owner()
    }
}

/// Whether the peer shares this process's OS confinement context.
///
/// Same-user peers can run under stricter confinement (a container, a sandbox,
/// a differently-scoped service). The daemon must not take a client's word for
/// this, so on Linux the peer's confinement is compared kernel-side: the pid
/// comes from `SO_PEERCRED` and the confinement identities from `/proc`.
/// Namespaces, `NoNewPrivs`, seccomp mode and filter count, capabilities,
/// supplementary groups, cgroup membership and the LSM label must all match,
/// and anything that cannot be read fails closed. Seccomp filters are opaque to
/// user space: when either side has filtering active, equivalence cannot be
/// proven from `/proc`, so the comparison reports a foreign context and the
/// daemon declines unless its operator also opted in. On other systems there
/// is no equivalent attestation, so this returns true and the daemon relies on
/// explicit operator acceptance.
pub(crate) fn peer_confinement_matches(stream: &UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        peer_shares_confinement(stream)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        true
    }
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Option<libc::pid_t> {
    use std::os::fd::AsRawFd;
    // SAFETY: credentials is a zeroed ucred, which is plain data for which zero is valid.
    // length is set to its size, the capacity getsockopt expects. The pointer covers
    // that many bytes. The pid is read only when getsockopt returned 0.
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        ) != 0
        {
            return None;
        }
        if credentials.pid <= 0 {
            return None;
        }
        Some(credentials.pid)
    }
}

/// Namespace entries whose identity must match for the daemon's OS context to
/// be the client's OS context.
#[cfg(target_os = "linux")]
const CONFINEMENT_NAMESPACES: &[&str] = &["user", "mnt", "net", "pid", "uts", "ipc", "cgroup"];

/// `/proc/pid/status` fields that describe OS confinement. `Uid`/`Gid` are
/// covered by the `SO_PEERCRED` owner check; these are the least-privilege
/// attributes a sandbox can tighten without changing namespaces.
#[cfg(target_os = "linux")]
const CONFINEMENT_STATUS_FIELDS: &[&str] = &[
    "NoNewPrivs:",
    "Seccomp:",
    "Seccomp_filters:",
    "CapInh:",
    "CapPrm:",
    "CapEff:",
    "CapBnd:",
    "CapAmb:",
    "Groups:",
];

/// Compare confinement for this process and the peer's pid, both read from this
/// process's `/proc`. Any mismatch fails closed, as does anything unreadable.
/// A namespace type absent from this kernel on both sides is skipped; any other
/// read failure (permissions, a vanished peer, an unobservable `/proc`) is a
/// foreign context. An LSM without a `current` interface on both sides is
/// likewise skipped. Active seccomp filtering on either side cannot be proven
/// equivalent from user space, so it also reports a foreign context.
#[cfg(target_os = "linux")]
fn peer_shares_confinement(stream: &UnixStream) -> bool {
    let pid = match peer_pid(stream) {
        Some(pid) => pid,
        None => return false,
    };
    // The loopback socketpair reports this process as its own peer; it shares
    // every confinement attribute by definition. Short-circuiting here keeps
    // that identity check independent of seccomp opacity below.
    // SAFETY: getpid has no preconditions and cannot fail.
    let own_pid = unsafe { libc::getpid() };
    if pid == own_pid {
        return true;
    }
    if !namespaces_match(pid) {
        return false;
    }
    match status_confinement_match(pid) {
        StatusMatch::Shared => {}
        StatusMatch::Foreign | StatusMatch::Unproven => return false,
    }
    if !cgroup_matches(pid) {
        return false;
    }
    if !lsm_label_matches(pid) {
        return false;
    }
    true
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum StatusMatch {
    Shared,
    Foreign,
    /// The observable fields match, but at least one side filters syscalls, so
    /// filter-program equivalence cannot be proven from `/proc`.
    Unproven,
}

#[cfg(target_os = "linux")]
fn is_not_found(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

#[cfg(target_os = "linux")]
fn namespaces_match(peer: libc::pid_t) -> bool {
    for namespace in CONFINEMENT_NAMESPACES {
        let own = std::fs::read_link(format!("/proc/self/ns/{namespace}"));
        let other = std::fs::read_link(format!("/proc/{peer}/ns/{namespace}"));
        match (own, other) {
            (Ok(own), Ok(other)) => {
                if own != other {
                    return false;
                }
            }
            // A namespace type the running kernel does not provide is absent
            // on both sides. Anything else unreadable fails closed instead of
            // counting an unobservable peer as shared.
            (Err(own_error), Err(other_error))
                if is_not_found(&own_error) && is_not_found(&other_error) =>
            {
                continue;
            }
            _ => return false,
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn confinement_status_lines(pid: libc::pid_t, use_self: bool) -> Option<Vec<String>> {
    let path = if use_self {
        "/proc/self/status".to_owned()
    } else {
        format!("/proc/{pid}/status")
    };
    let contents = std::fs::read_to_string(path).ok()?;
    status_fields_from_text(&contents)
}

#[cfg(target_os = "linux")]
fn status_fields_from_text(contents: &str) -> Option<Vec<String>> {
    let mut fields = Vec::with_capacity(CONFINEMENT_STATUS_FIELDS.len());
    let mut seen = vec![false; CONFINEMENT_STATUS_FIELDS.len()];
    for line in contents.lines() {
        let Some(index) = CONFINEMENT_STATUS_FIELDS
            .iter()
            .position(|prefix| line.starts_with(prefix))
        else {
            continue;
        };
        if seen[index] {
            return None;
        }
        seen[index] = true;
        fields.push(line.to_owned());
    }
    if seen.into_iter().all(|field| field) {
        fields.sort();
        Some(fields)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn seccomp_active(fields: &[String]) -> Option<bool> {
    let mut mode_active = None;
    let mut filters_active = None;
    for field in fields {
        if let Some(value) = field.strip_prefix("Seccomp:") {
            mode_active = Some(value.trim() != "0");
        } else if let Some(value) = field.strip_prefix("Seccomp_filters:") {
            filters_active = Some(value.trim() != "0");
        }
    }
    match (mode_active, filters_active) {
        (Some(mode), Some(filters)) => Some(mode || filters),
        // Older kernels predate Seccomp_filters; the mode alone decides.
        (Some(mode), None) => Some(mode),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn status_confinement_match(peer: libc::pid_t) -> StatusMatch {
    let own = match confinement_status_lines(peer, true) {
        Some(lines) => lines,
        None => return StatusMatch::Foreign,
    };
    let other = match confinement_status_lines(peer, false) {
        Some(lines) => lines,
        None => return StatusMatch::Foreign,
    };
    if own != other {
        return StatusMatch::Foreign;
    }
    match (seccomp_active(&own), seccomp_active(&other)) {
        (Some(true), _) | (_, Some(true)) => StatusMatch::Unproven,
        (Some(false), Some(false)) => StatusMatch::Shared,
        _ => StatusMatch::Foreign,
    }
}

#[cfg(target_os = "linux")]
fn cgroup_matches(peer: libc::pid_t) -> bool {
    let own = std::fs::read(format!("/proc/self/cgroup"));
    let other = std::fs::read(format!("/proc/{peer}/cgroup"));
    match (own, other) {
        (Ok(own), Ok(other)) => own == other,
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn lsm_label_matches(peer: libc::pid_t) -> bool {
    let own = std::fs::read("/proc/self/attr/current");
    let other = std::fs::read(format!("/proc/{peer}/attr/current"));
    match (own, other) {
        (Ok(own), Ok(other)) => own == other,
        // Kernels without an LSM expose no `current` label on either side.
        (Err(own_error), Err(other_error))
            if is_not_found(&own_error) && is_not_found(&other_error) =>
        {
            true
        }
        _ => false,
    }
}

#[cfg(unix)]
pub(crate) fn peer_has_uid(stream: &UnixStream, owner: libc::uid_t) -> bool {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    // SAFETY: stream.as_raw_fd() is the descriptor of a connected AF_UNIX socket,
    // borrowed for this call. uid and gid are live locals of type uid_t and gid_t.
    // uid is read only when getpeereid returned 0, because && short-circuits.
    unsafe {
        let mut uid = 0;
        let mut gid = 0;
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == owner
    }
    #[cfg(target_os = "linux")]
    // SAFETY: credentials is a zeroed ucred, which is plain data for which zero is valid.
    // length is set to its size, the capacity getsockopt expects. The pointer covers
    // that many bytes. credentials.uid is read only when getsockopt returned 0.
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        ) == 0
            && credentials.uid == owner
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (stream, owner);
        false
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A loopback socket pair reports the calling process as its own peer, so
    /// the confinement comparison must agree with itself. This exercises the
    /// read path, not the cross-namespace decline (see `tests/daemon_cli.rs`).
    #[test]
    fn loopback_peer_shares_confinement() {
        let (first, second) = UnixStream::pair().expect("socket pair");
        assert!(peer_confinement_matches(&first));
        assert!(peer_confinement_matches(&second));
    }

    #[cfg(target_os = "linux")]
    const REALISTIC_STATUS_BASE: &str = concat!(
        "Name:\texecute\n",
        "Umask:\t0022\n",
        "State:\tR (running)\n",
        "Tgid:\t100\n",
        "Ngid:\t0\n",
        "Pid:\t100\n",
        "PPid:\t1\n",
        "TracerPid:\t0\n",
        "Uid:\t1000\t1000\t1000\t1000\n",
        "Gid:\t1000\t1000\t1000\t1000\n",
        "FDSize:\t64\n",
        "Groups:\t1000 \n",
        "NStgid:\t100\n",
        "NSpid:\t100\n",
        "NSpgid:\t100\n",
        "NSsid:\t100\n",
        "Kthread:\t0\n",
        "VmPeak:\t1000 kB\n",
        "NoNewPrivs:\t0\n",
        "Seccomp:\t0\n",
        "Seccomp_filters:\t0\n",
        "Speculation_Store_Bypass:\tthread vulnerable\n",
        "Cpus_allowed:\tff\n",
        "CapInh:\t0000000000000000\n",
        "CapPrm:\t0000000000000000\n",
        "CapEff:\t0000000000000000\n",
        "CapBnd:\t000001ffffffffff\n",
        "CapAmb:\t0000000000000000\n",
    );

    #[cfg(target_os = "linux")]
    fn status_with(replacements: &[(&str, &str)]) -> String {
        let mut text = REALISTIC_STATUS_BASE.to_owned();
        for (prefix, replacement) in replacements {
            let start = text
                .find(prefix)
                .unwrap_or_else(|| panic!("missing prefix {prefix}"));
            let end = text[start..]
                .find('\n')
                .map(|offset| start + offset)
                .unwrap();
            text.replace_range(start..end, replacement);
        }
        text
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identical_unfiltered_status_shares_confinement() {
        let own = status_fields_from_text(REALISTIC_STATUS_BASE).expect("parse base");
        let peer = status_fields_from_text(REALISTIC_STATUS_BASE).expect("parse peer");
        assert_eq!(own, peer);
        assert_eq!(seccomp_active(&own), Some(false));
        assert_eq!(
            status_text_match(REALISTIC_STATUS_BASE, REALISTIC_STATUS_BASE),
            StatusMatch::Shared
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn differing_seccomp_filters_are_foreign_with_same_namespaces() {
        // Same namespaces, but the peer installed one allow-all filter.
        let filtered = status_with(&[
            ("Seccomp:", "Seccomp:\t2"),
            ("Seccomp_filters:", "Seccomp_filters:\t1"),
        ]);
        assert_eq!(
            status_text_match(REALISTIC_STATUS_BASE, &filtered),
            StatusMatch::Foreign
        );
        assert_eq!(
            status_text_match(&filtered, REALISTIC_STATUS_BASE),
            StatusMatch::Foreign
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identical_filtered_status_is_unproven_not_shared() {
        // Filter programs are opaque in /proc, so even byte-identical
        // observables cannot prove equivalence once filtering is active.
        let filtered = status_with(&[
            ("Seccomp:", "Seccomp:\t2"),
            ("Seccomp_filters:", "Seccomp_filters:\t1"),
        ]);
        assert_eq!(
            status_text_match(&filtered, &filtered),
            StatusMatch::Unproven
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn differing_no_new_privs_capabilities_and_groups_are_foreign() {
        let no_new_privs = status_with(&[("NoNewPrivs:", "NoNewPrivs:\t1")]);
        assert_eq!(
            status_text_match(REALISTIC_STATUS_BASE, &no_new_privs),
            StatusMatch::Foreign
        );
        let capabilities = status_with(&[("CapEff:", "CapEff:\t0000000000000400")]);
        assert_eq!(
            status_text_match(REALISTIC_STATUS_BASE, &capabilities),
            StatusMatch::Foreign
        );
        let groups = status_with(&[("Groups:", "Groups:\t1000 1001 ")]);
        assert_eq!(
            status_text_match(REALISTIC_STATUS_BASE, &groups),
            StatusMatch::Foreign
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn incomplete_status_fails_closed() {
        assert_eq!(status_fields_from_text("Seccomp:\t0\n"), None);
        assert_eq!(
            status_text_match(REALISTIC_STATUS_BASE, "Seccomp:\t0\n"),
            StatusMatch::Foreign
        );
    }

    #[cfg(target_os = "linux")]
    fn status_text_match(own: &str, peer: &str) -> StatusMatch {
        let own = match status_fields_from_text(own) {
            Some(lines) => lines,
            None => return StatusMatch::Foreign,
        };
        let peer = match status_fields_from_text(peer) {
            Some(lines) => lines,
            None => return StatusMatch::Foreign,
        };
        if own != peer {
            return StatusMatch::Foreign;
        }
        match (seccomp_active(&own), seccomp_active(&peer)) {
            (Some(true), _) | (_, Some(true)) => StatusMatch::Unproven,
            (Some(false), Some(false)) => StatusMatch::Shared,
            _ => StatusMatch::Foreign,
        }
    }
}
