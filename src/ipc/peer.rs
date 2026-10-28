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
/// this, so on Linux the peer's namespaces are compared kernel-side: the pid
/// comes from `SO_PEERCRED` and the namespace identities from `/proc`. Every
/// compared namespace must match, and any failure to compare fails closed. On
/// other systems there is no equivalent attestation, so this returns true and
/// the daemon relies on explicit operator acceptance.
pub(crate) fn peer_confinement_matches(stream: &UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        peer_shares_namespaces(stream)
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

/// Compare namespace identities for this process and the peer's pid, both read
/// from this process's `/proc`. A namespace type missing on both sides (a
/// kernel predating it) counts as shared; anything else that cannot be read or
/// that differs fails closed.
#[cfg(target_os = "linux")]
fn peer_shares_namespaces(stream: &UnixStream) -> bool {
    let pid = match peer_pid(stream) {
        Some(pid) => pid,
        None => return false,
    };
    CONFINEMENT_NAMESPACES.iter().all(|namespace| {
        let own = std::fs::read_link(format!("/proc/self/ns/{namespace}"));
        let peer = std::fs::read_link(format!("/proc/{pid}/ns/{namespace}"));
        match (own, peer) {
            (Ok(own), Ok(peer)) => own == peer,
            (Err(_), Err(_)) => true,
            _ => false,
        }
    })
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
}
