//! Local IPC shared by `execute live`, `execute attach`, `run --control`, and the
//! daemon: framed message I/O under absolute budgets, same-user peer checks, and the
//! socket types. It lives in core so that the live server can be gated behind the
//! `full` feature while the daemon, which only needs this layer, stays in every build.
#[cfg(any(unix, feature = "live"))]
pub(crate) mod peer;
#[cfg(any(unix, feature = "live"))]
pub(crate) mod transport;
#[cfg(all(windows, feature = "live"))]
pub(crate) mod windows;

#[cfg(all(unix, feature = "live"))]
pub(crate) use std::os::unix::net::UnixListener;
#[cfg(unix)]
pub(crate) use std::os::unix::net::UnixStream;
#[cfg(all(windows, feature = "live"))]
pub(crate) use windows::Listener as UnixListener;
#[cfg(all(windows, feature = "live"))]
pub(crate) use windows::Stream as UnixStream;
