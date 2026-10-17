//! Same-user peer checks for local IPC sockets. The live server, the attach client,
//! and the daemon all refuse a connection whose peer is not the current user.
use super::UnixStream;

/// Whether the peer of `stream` runs as the current user.
pub(crate) fn peer_is_owner(stream: &UnixStream) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail. It only reads the
        // process's effective uid.
        peer_has_uid(stream, unsafe { libc::geteuid() })
    }
    #[cfg(windows)]
    {
        stream.peer_is_owner()
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
