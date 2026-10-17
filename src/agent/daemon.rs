//! `execute daemon` (#77, first slice): a per-user process behind a Unix socket.
//!
//! The daemon answers `ping`, `status` and `stop`. It accepts only connections from its
//! own user, and it keeps its socket in a private per-user directory. A client probes
//! for it before any manifest or registry load, so `run --daemon` can tell at once
//! whether a daemon is up. Running programs through the daemon is the next slice; until
//! then a run always executes cold.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The wire protocol every request and reply names.
pub const PROTOCOL: &str = "execute.daemon/1";

/// How a run was asked to execute. `--daemon` and `EXECUTE_DAEMON=1` select `Daemon`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceleration {
    Cold,
    Daemon,
}

/// What the probe found. A run only asks for it when `Acceleration::Daemon` was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// A daemon of this user answered on the default socket.
    Running,
    /// No daemon answered, or the socket directory is not private to this user.
    Absent,
}

const USAGE: &str = "usage: execute daemon serve|status|stop [--socket PATH]";

/// `execute daemon ...`.
pub fn cli(arguments: &[String]) -> Result<i32> {
    let (command, rest) = arguments.split_first().ok_or_else(|| anyhow!(USAGE))?;
    let socket = socket_option(rest)?;
    match command.as_str() {
        "serve" => {
            imp::serve(&socket)?;
            Ok(0)
        }
        "status" => {
            println!("{}", serde_json::to_string(&imp::status(&socket))?);
            Ok(0)
        }
        "stop" => {
            imp::stop(&socket)?;
            Ok(0)
        }
        other => bail!("unknown daemon command {other:?}; {USAGE}"),
    }
}

/// The socket a daemon listens on: `$XDG_RUNTIME_DIR` (or the temporary directory)
/// under a per-user directory named for the effective uid.
pub fn default_socket() -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    };
    base.join(format!("execute-{}", imp::euid()))
        .join("daemon.sock")
}

/// Probe for a running daemon at the default socket. It reads no program and loads no
/// manifest, so when no daemon is up it costs one failed connection.
pub fn probe() -> Probe {
    imp::probe(&default_socket())
}

fn socket_option(arguments: &[String]) -> Result<PathBuf> {
    let mut socket = None;
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--socket" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow!("--socket requires a path"))?;
                if socket.replace(PathBuf::from(value)).is_some() {
                    bail!("--socket may be specified only once");
                }
            }
            other => bail!("unknown daemon option {other:?}; {USAGE}"),
        }
    }
    Ok(socket.unwrap_or_else(default_socket))
}

/// The status reply for a daemon that is not running.
fn not_running(socket: &Path) -> Value {
    json!({"ok": true, "protocol": PROTOCOL, "running": false, "socket": socket.to_string_lossy()})
}

#[cfg(unix)]
mod imp {
    use super::{json, not_running, Context, Path, PathBuf, Probe, Result, Value, PROTOCOL};
    use crate::live::peer_is_owner;
    use crate::live::transport::{read_message, write_message};
    use anyhow::{anyhow, bail};
    use std::fs::{self, DirBuilder, Permissions};
    use std::io::ErrorKind;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, Instant};

    const IO_TIMEOUT: Duration = Duration::from_secs(2);
    const PROBE_TIMEOUT: Duration = Duration::from_millis(50);
    const MAX_MESSAGE_BYTES: usize = 64 * 1024;

    pub(super) fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Create the per-user directory with mode 0700 when asked, then check that it is a
    /// real directory owned by this user and private to it.
    fn private_directory(path: &Path, create: bool) -> Result<()> {
        if create {
            match DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        let metadata =
            fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
        if !metadata.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        if metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
            bail!(
                "{} must be owned by you and private (mode 0700)",
                path.display()
            );
        }
        Ok(())
    }

    /// Whether `path` is a socket this user owns, the only kind a client connects to.
    fn owned_socket(path: &Path) -> bool {
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.uid() == euid())
    }

    pub(super) fn probe(socket: &Path) -> Probe {
        let Some(directory) = socket.parent() else {
            return Probe::Absent;
        };
        if private_directory(directory, false).is_err() || !owned_socket(socket) {
            return Probe::Absent;
        }
        match request(socket, "ping", PROBE_TIMEOUT) {
            Ok(reply) if reply["ok"] == true => Probe::Running,
            _ => Probe::Absent,
        }
    }

    pub(super) fn status(socket: &Path) -> Value {
        request(socket, "status", IO_TIMEOUT).unwrap_or_else(|_| not_running(socket))
    }

    pub(super) fn stop(socket: &Path) -> Result<()> {
        let reply = request(socket, "stop", IO_TIMEOUT)
            .with_context(|| format!("no daemon is running at {}", socket.display()))?;
        if reply["ok"] != true {
            bail!("the daemon refused to stop: {reply}");
        }
        Ok(())
    }

    /// Send one request and read its reply, each step within `timeout`.
    fn request(socket: &Path, operation: &str, timeout: Duration) -> Result<Value> {
        let mut stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        let message = format!("{}\n", json!({"protocol": PROTOCOL, "op": operation}));
        write_message(&mut stream, message.as_bytes(), timeout)?;
        let reply = read_message(&mut stream, MAX_MESSAGE_BYTES, timeout)?;
        serde_json::from_slice(&reply).context("daemon reply is not JSON")
    }

    /// Run the daemon in the foreground until a `stop` request arrives.
    pub(super) fn serve(socket: &Path) -> Result<()> {
        let directory = socket
            .parent()
            .ok_or_else(|| anyhow!("socket path {} has no directory", socket.display()))?;
        private_directory(directory, true)?;
        if request(socket, "ping", PROBE_TIMEOUT).is_ok() {
            bail!("a daemon is already running at {}", socket.display());
        }
        if fs::symlink_metadata(socket).is_ok() {
            // Only a stale socket of ours is replaced; anything else is left alone.
            if !owned_socket(socket) {
                bail!("{} exists and is not a socket of yours", socket.display());
            }
            fs::remove_file(socket).with_context(|| format!("removing {}", socket.display()))?;
        }
        let listener =
            UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
        fs::set_permissions(socket, Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", socket.display()))?;
        let _guard = SocketGuard(socket.to_owned());
        let mut state = State {
            started: Instant::now(),
            served: 0,
        };
        loop {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            if handle(&mut stream, &mut state) == Flow::Stop {
                return Ok(());
            }
        }
    }

    /// Removes the socket file when the daemon exits.
    struct SocketGuard(PathBuf);

    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    struct State {
        started: Instant,
        served: u64,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Flow {
        Continue,
        Stop,
    }

    /// Answer one connection. A connection from another user, or one that does not send
    /// a valid request, is refused and does not stop the daemon.
    fn handle(stream: &mut UnixStream, state: &mut State) -> Flow {
        if !peer_is_owner(stream) {
            let _ = reply(
                stream,
                json!({"ok": false, "error": "peer is not the daemon's owner"}),
            );
            return Flow::Continue;
        }
        let Ok(bytes) = read_message(stream, MAX_MESSAGE_BYTES, IO_TIMEOUT) else {
            return Flow::Continue;
        };
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(request) => request,
            Err(_) => {
                let _ = reply(stream, json!({"ok": false, "error": "request is not JSON"}));
                return Flow::Continue;
            }
        };
        if request["protocol"] != PROTOCOL {
            let _ = reply(
                stream,
                json!({"ok": false, "error": "unsupported protocol"}),
            );
            return Flow::Continue;
        }
        state.served += 1;
        match request["op"].as_str() {
            Some("ping") => {
                let _ = reply(
                    stream,
                    json!({"ok": true, "protocol": PROTOCOL, "pid": std::process::id()}),
                );
                Flow::Continue
            }
            Some("status") => {
                let _ = reply(
                    stream,
                    json!({
                        "ok": true,
                        "protocol": PROTOCOL,
                        "running": true,
                        "pid": std::process::id(),
                        "uptime_ms": state.started.elapsed().as_millis() as u64,
                        "requests_served": state.served,
                    }),
                );
                Flow::Continue
            }
            Some("stop") => {
                let _ = reply(
                    stream,
                    json!({"ok": true, "protocol": PROTOCOL, "stopping": true}),
                );
                Flow::Stop
            }
            _ => {
                let _ = reply(stream, json!({"ok": false, "error": "unknown operation"}));
                Flow::Continue
            }
        }
    }

    fn reply(stream: &mut UnixStream, body: Value) -> Result<()> {
        let message = format!("{body}\n");
        write_message(stream, message.as_bytes(), IO_TIMEOUT)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{not_running, Path, Probe, Result, Value};
    use anyhow::bail;

    pub(super) fn euid() -> u32 {
        0
    }

    pub(super) fn probe(_socket: &Path) -> Probe {
        Probe::Absent
    }

    pub(super) fn status(socket: &Path) -> Value {
        not_running(socket)
    }

    pub(super) fn stop(_socket: &Path) -> Result<()> {
        bail!("the daemon needs Unix sockets; this build has none")
    }

    pub(super) fn serve(_socket: &Path) -> Result<()> {
        bail!("the daemon needs Unix sockets; this build has none")
    }
}
