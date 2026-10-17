//! `execute daemon` (#77): a per-user zygote behind a Unix socket.
//!
//! The daemon parent is single-threaded. It accepts connections and answers `ping`,
//! `status` and `stop` itself. For a `run` request it forks: the child takes the client's
//! stdin, stdout and stderr (sent over the socket as descriptors), changes to the client's
//! working directory, replaces its environment, and calls the run callback that `main`
//! passes in. The child exits with the run's status. The parent keeps the client's
//! connection, reports the child's pid and exit status to the client, and kills the run
//! if the client disconnects. A program that takes the terminal is declined before it
//! produces output, and the client runs it itself.
//!
//! Every client path checks the socket directory and the socket before it connects, and
//! checks the peer's uid after it connects.
//!
//! The accept stall: the parent reads each request on its accepting thread, with a
//! 2 s timeout (`IO_TIMEOUT`). A client that connects and sends nothing therefore holds
//! the accept loop for up to 2 s before its connection is dropped, and other clients wait
//! in the listen queue meanwhile. The parent must stay single-threaded because it forks,
//! and fork in a multithreaded process is unsafe, so the read cannot move to a thread.
//! Only processes of the same user can connect, so the stall is a delay a user can cause
//! to themselves, not a denial of service across users.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The wire protocol every request and reply names.
pub const PROTOCOL: &str = "execute.daemon/1";

/// Runs one forwarded request in a forked child and returns the program's status. The
/// callback runs the normal execute path; it may call `std::process::exit` itself.
pub type RunCallback = fn(&[String], &mut RunContext) -> i32;

/// The client's connection, held by a forked run.
pub struct RunContext {
    #[cfg(unix)]
    connection: std::os::unix::net::UnixStream,
}

impl RunContext {
    /// Tell the client this daemon will not run the program. Call it before any output;
    /// the client then runs the program itself.
    #[cfg(unix)]
    pub fn decline(&mut self, reason: &str) {
        let _ = imp::write_line(
            &mut self.connection,
            &json!({"event": "fallback", "reason": reason}),
        );
    }

    #[cfg(not(unix))]
    pub fn decline(&mut self, _reason: &str) {}
}

/// What a client does with a `run --daemon` request.
#[derive(Debug, PartialEq, Eq)]
pub enum Forwarding {
    /// The daemon ran the program, and this is its exit status.
    Ran(i32),
    /// Run the program here. The note, when present, is for stderr first.
    Cold { note: Option<String> },
}

const USAGE: &str = "usage: execute daemon serve|status|stop [--socket PATH]";

/// `execute daemon ...`. `run` is the callback a forked child calls for each request.
pub fn cli(arguments: &[String], run: RunCallback) -> Result<i32> {
    let (command, rest) = arguments.split_first().ok_or_else(|| anyhow!(USAGE))?;
    let socket = socket_option(rest)?;
    match command.as_str() {
        "serve" => {
            imp::serve(&socket, run)?;
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

/// Forward a `run` request (arguments without `--daemon`) to the daemon at the default
/// socket. Without a trusted daemon the program runs here, with no output of its own.
pub fn forward_run(arguments: &[String]) -> Forwarding {
    imp::forward(arguments, &default_socket())
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

/// The status reply for a daemon that is not running, or whose socket cannot be trusted.
fn not_running(socket: &Path, reason: Option<String>) -> Value {
    json!({
        "ok": true,
        "protocol": PROTOCOL,
        "running": false,
        "socket": socket.to_string_lossy(),
        "reason": reason,
    })
}

#[cfg(unix)]
mod imp {
    use super::{
        not_running, Forwarding, Path, PathBuf, Result, RunCallback, RunContext, Value, PROTOCOL,
    };
    use crate::live::peer_is_owner;
    use crate::live::transport::{read_message, write_message};
    use anyhow::{anyhow, bail, Context};
    use serde_json::json;
    use std::ffi::OsString;
    use std::fmt;
    use std::fs::{self, DirBuilder, Permissions};
    use std::io::{self, ErrorKind, Read, Write};
    use std::mem;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const IO_TIMEOUT: Duration = Duration::from_secs(2);
    const PROBE_TIMEOUT: Duration = Duration::from_millis(50);
    const MAX_HEADER_BYTES: usize = 256 * 1024;
    /// Room in the control buffer for one descriptor-passing message.
    const CONTROL_WORDS: usize = 32;
    /// How often the parent checks its children and the client connections.
    const POLL_MILLIS: i32 = 50;
    /// How long a run has after its client disconnects before it is killed.
    const KILL_GRACE: Duration = Duration::from_secs(15);

    /// Why a request to the daemon did not complete. Nothing is sent to a path that fails
    /// the trust checks, so `Untrusted` means the request never left this process.
    #[derive(Debug)]
    pub(super) enum DaemonError {
        /// The directory holding the socket, the socket itself, or the peer that answered
        /// is not this user's. Nothing was sent.
        Untrusted { path: PathBuf, reason: String },
        /// The path is trusted but nothing is listening on it.
        NotRunning { path: PathBuf },
        /// A trusted daemon was reached but the exchange failed.
        Protocol { path: PathBuf, detail: String },
    }

    impl fmt::Display for DaemonError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Untrusted { path, reason } => write!(
                    formatter,
                    "refusing to use the daemon socket {}: {reason}",
                    path.display()
                ),
                Self::NotRunning { path } => {
                    write!(formatter, "no daemon is running at {}", path.display())
                }
                Self::Protocol { path, detail } => {
                    write!(
                        formatter,
                        "the daemon at {} failed: {detail}",
                        path.display()
                    )
                }
            }
        }
    }

    impl std::error::Error for DaemonError {}

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

    /// The checks a path must pass before anything is sent to it: its directory is
    /// private to this user, and the path is a socket this user owns.
    fn check_trusted(socket: &Path) -> std::result::Result<(), DaemonError> {
        let untrusted = |reason: String| DaemonError::Untrusted {
            path: socket.to_owned(),
            reason,
        };
        let directory = socket
            .parent()
            .ok_or_else(|| untrusted("the path has no directory".to_owned()))?;
        // A directory that does not exist means no daemon has ever run here: not running,
        // which is not an alarm. A directory that exists but is not private is untrusted.
        if matches!(fs::symlink_metadata(directory), Err(ref error) if error.kind() == ErrorKind::NotFound)
        {
            return Err(DaemonError::NotRunning {
                path: socket.to_owned(),
            });
        }
        private_directory(directory, false).map_err(|error| untrusted(format!("{error:#}")))?;
        if !owned_socket(socket) {
            return Err(untrusted(
                "it is not a socket owned by you (it may be absent)".to_owned(),
            ));
        }
        Ok(())
    }

    /// Connect to the daemon at `socket` after the path checks, and confirm that the
    /// process listening there belongs to this user.
    fn connect_trusted(socket: &Path) -> std::result::Result<UnixStream, DaemonError> {
        check_trusted(socket)?;
        let stream = UnixStream::connect(socket).map_err(|error| match error.kind() {
            ErrorKind::ConnectionRefused | ErrorKind::NotFound => DaemonError::NotRunning {
                path: socket.to_owned(),
            },
            _ => DaemonError::Protocol {
                path: socket.to_owned(),
                detail: format!("connecting failed: {error}"),
            },
        })?;
        if !peer_is_owner(&stream) {
            return Err(DaemonError::Untrusted {
                path: socket.to_owned(),
                reason: "the process listening there is not owned by you".to_owned(),
            });
        }
        Ok(stream)
    }

    /// Send one request line and read its one-line reply, each step within `timeout`.
    fn request(
        socket: &Path,
        operation: &str,
        timeout: Duration,
    ) -> std::result::Result<Value, DaemonError> {
        let mut stream = connect_trusted(socket)?;
        let protocol_error = |detail: String| DaemonError::Protocol {
            path: socket.to_owned(),
            detail,
        };
        let message = format!("{}\n", json!({"protocol": PROTOCOL, "op": operation}));
        write_message(&mut stream, message.as_bytes(), timeout)
            .map_err(|error| protocol_error(format!("sending failed: {error:#}")))?;
        let reply = read_message(&mut stream, MAX_HEADER_BYTES, timeout)
            .map_err(|error| protocol_error(format!("reading failed: {error:#}")))?;
        serde_json::from_slice(&reply).map_err(|_| protocol_error("reply is not JSON".to_owned()))
    }

    pub(super) fn status(socket: &Path) -> Value {
        match request(socket, "status", IO_TIMEOUT) {
            Ok(reply) => reply,
            Err(DaemonError::Untrusted { reason, .. }) => not_running(socket, Some(reason)),
            Err(_) => not_running(socket, None),
        }
    }

    pub(super) fn stop(socket: &Path) -> Result<()> {
        let reply = request(socket, "stop", IO_TIMEOUT)?;
        if reply["ok"] != true {
            bail!("the daemon refused to stop: {reply}");
        }
        Ok(())
    }

    /// Write one JSON value and a newline.
    pub(super) fn write_line(stream: &mut UnixStream, value: &Value) -> io::Result<()> {
        let mut bytes = value.to_string().into_bytes();
        bytes.push(b'\n');
        stream.write_all(&bytes)
    }

    /// Run the daemon in the foreground until a `stop` request arrives.
    pub(super) fn serve(socket: &Path, run: RunCallback) -> Result<()> {
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
        listener
            .set_nonblocking(true)
            .context("making the listener nonblocking")?;
        let _guard = SocketGuard(socket.to_owned());
        let mut server = Server {
            listener,
            run,
            children: Vec::new(),
            started: Instant::now(),
            served: 0,
        };
        loop {
            server.reap();
            server.wait_for_activity();
            if server.accept_pending() == Flow::Stop {
                break;
            }
        }
        server.shutdown();
        Ok(())
    }

    /// Removes the socket file when the daemon exits.
    struct SocketGuard(PathBuf);

    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Flow {
        Continue,
        Stop,
    }

    /// A forked run and the client connection that receives its events.
    struct Child {
        pid: libc::pid_t,
        connection: UnixStream,
        kill_at: Option<Instant>,
    }

    enum Waited {
        Running,
        Exited(i32),
        Gone,
    }

    struct Server {
        listener: UnixListener,
        run: RunCallback,
        children: Vec<Child>,
        started: Instant,
        served: u64,
    }

    impl Server {
        /// Block for at most one poll interval, unless a connection or a child's client
        /// has something to say.
        fn wait_for_activity(&self) {
            let mut descriptors = Vec::with_capacity(self.children.len() + 1);
            descriptors.push(libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            // A client that hung up stays readable at end of file, so it is left out of the
            // wait; its run is being terminated.
            for child in self.children.iter().filter(|child| child.kill_at.is_none()) {
                descriptors.push(libc::pollfd {
                    fd: child.connection.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            // SAFETY: descriptors is a valid array of pollfd for the whole call.
            unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    POLL_MILLIS,
                );
            }
        }

        fn accept_pending(&mut self) -> Flow {
            loop {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        if self.handle(stream) == Flow::Stop {
                            return Flow::Stop;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return Flow::Continue,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(10));
                        return Flow::Continue;
                    }
                }
            }
        }

        /// Answer one connection. A connection from another user, or one that does not send
        /// a valid request, is refused and does not stop the daemon.
        fn handle(&mut self, mut stream: UnixStream) -> Flow {
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
                || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
            {
                return Flow::Continue;
            }
            if !peer_is_owner(&stream) {
                let _ = write_line(
                    &mut stream,
                    &json!({"ok": false, "error": "peer is not the daemon's owner"}),
                );
                return Flow::Continue;
            }
            let (request, descriptors) = match receive_request(&stream) {
                Ok(received) => received,
                Err(detail) => {
                    let _ = write_line(&mut stream, &json!({"ok": false, "error": detail}));
                    return Flow::Continue;
                }
            };
            if request["protocol"] != PROTOCOL {
                let _ = write_line(
                    &mut stream,
                    &json!({"ok": false, "error": "unsupported protocol"}),
                );
                return Flow::Continue;
            }
            self.served += 1;
            match request["op"].as_str() {
                Some("ping") => {
                    let _ = write_line(
                        &mut stream,
                        &json!({"ok": true, "protocol": PROTOCOL, "pid": std::process::id()}),
                    );
                    Flow::Continue
                }
                Some("status") => {
                    let _ = write_line(&mut stream, &self.status_reply());
                    Flow::Continue
                }
                Some("stop") => {
                    let _ = write_line(
                        &mut stream,
                        &json!({"ok": true, "protocol": PROTOCOL, "stopping": true}),
                    );
                    Flow::Stop
                }
                Some("run") => {
                    self.start_run(stream, &request, descriptors);
                    Flow::Continue
                }
                _ => {
                    let _ = write_line(
                        &mut stream,
                        &json!({"ok": false, "error": "unknown operation"}),
                    );
                    Flow::Continue
                }
            }
        }

        fn status_reply(&self) -> Value {
            json!({
                "ok": true,
                "protocol": PROTOCOL,
                "running": true,
                "pid": std::process::id(),
                "uptime_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "requests_served": self.served,
                "runs": self.children.len(),
            })
        }

        /// Fork a child for a run request. The child takes over the client's stdio and
        /// environment and never returns. The parent closes its copies of the descriptors,
        /// keeps the connection, and reports the child's pid.
        fn start_run(
            &mut self,
            mut stream: UnixStream,
            request: &Value,
            descriptors: Vec<OwnedFd>,
        ) {
            let run = match RunRequest::from_json(request) {
                Ok(run) => run,
                Err(detail) => {
                    let _ = write_line(&mut stream, &json!({"ok": false, "error": detail}));
                    return;
                }
            };
            if descriptors.len() != 3 {
                let _ = write_line(
                    &mut stream,
                    &json!({"ok": false, "error": "a run needs exactly three descriptors"}),
                );
                return;
            }
            let held: Vec<RawFd> = self
                .children
                .iter()
                .map(|child| child.connection.as_raw_fd())
                .collect();
            let setup = ChildSetup {
                listener_fd: self.listener.as_raw_fd(),
                held,
                descriptors,
                connection: stream,
                run: self.run,
                request: run,
            };
            // SAFETY: the parent is single-threaded, so the child is a copy of one thread
            // and inherits no lock held elsewhere. The child never returns from run_child.
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                let mut connection = setup.connection;
                let _ = write_line(
                    &mut connection,
                    &json!({"ok": false, "error": io::Error::last_os_error().to_string()}),
                );
                return;
            }
            if pid == 0 {
                run_child(setup);
            }
            // Parent: the descriptors belong to the child now, so close the parent's copies.
            drop(setup.descriptors);
            let mut connection = setup.connection;
            let _ = write_line(&mut connection, &json!({"event": "started", "pid": pid}));
            self.children.push(Child {
                pid,
                connection,
                kill_at: None,
            });
        }

        /// Collect finished runs and report their status. A run whose client has hung up
        /// is terminated, and killed if it does not end within the grace period.
        fn reap(&mut self) {
            let now = Instant::now();
            let mut index = 0;
            while index < self.children.len() {
                let child = &mut self.children[index];
                if child.kill_at.is_none() && client_hung_up(&child.connection) {
                    signal_child(child.pid, libc::SIGTERM);
                    child.kill_at = Some(now + KILL_GRACE);
                }
                if child.kill_at.is_some_and(|deadline| now >= deadline) {
                    signal_child(child.pid, libc::SIGKILL);
                }
                match wait_nohang(child.pid) {
                    Waited::Running => index += 1,
                    Waited::Exited(status) => {
                        let mut finished = self.children.swap_remove(index);
                        let _ = write_line(
                            &mut finished.connection,
                            &json!({"event": "exit", "status": status}),
                        );
                    }
                    Waited::Gone => {
                        self.children.swap_remove(index);
                    }
                }
            }
        }

        /// Stop every run and wait for them: terminate, then kill what remains.
        fn shutdown(&mut self) {
            for child in &self.children {
                signal_child(child.pid, libc::SIGTERM);
            }
            let deadline = Instant::now() + KILL_GRACE;
            while !self.children.is_empty() && Instant::now() < deadline {
                self.reap();
                std::thread::sleep(Duration::from_millis(20));
            }
            for child in self.children.drain(..) {
                signal_child(child.pid, libc::SIGKILL);
                let _ = wait_blocking(child.pid);
            }
        }
    }

    /// The resource limits a run inherits from its client. A run must have all of them.
    const LIMIT_NAMES: [&str; 6] = ["nofile", "nproc", "as", "cpu", "fsize", "core"];

    /// The `setrlimit` resource for one of `LIMIT_NAMES`.
    fn limit_resource(name: &str) -> Option<libc::c_int> {
        Some(match name {
            "nofile" => libc::RLIMIT_NOFILE as _,
            "nproc" => libc::RLIMIT_NPROC as _,
            "as" => libc::RLIMIT_AS as _,
            "cpu" => libc::RLIMIT_CPU as _,
            "fsize" => libc::RLIMIT_FSIZE as _,
            "core" => libc::RLIMIT_CORE as _,
            _ => return None,
        })
    }

    /// One client limit: its soft and hard values, as `getrlimit` reports them.
    struct Limit {
        name: &'static str,
        soft: u64,
        hard: u64,
    }

    /// The run request, after validation.
    struct RunRequest {
        argv: Vec<String>,
        cwd: String,
        env: Vec<(String, String)>,
        umask: u32,
        limits: Vec<Limit>,
    }

    impl RunRequest {
        fn from_json(request: &Value) -> std::result::Result<Self, String> {
            let argv = strings(&request["argv"]).ok_or("argv must be an array of strings")?;
            if argv.first().map(String::as_str) != Some("run") {
                return Err("only run requests are forwarded".to_owned());
            }
            if argv.iter().any(|argument| argument.contains('\0')) {
                return Err("arguments may not contain NUL".to_owned());
            }
            let cwd = request["cwd"]
                .as_str()
                .filter(|cwd| !cwd.contains('\0'))
                .ok_or("cwd must be a string without NUL")?
                .to_owned();
            let mut env = Vec::new();
            for pair in request["env"].as_array().ok_or("env must be an array")? {
                match (
                    pair.get(0).and_then(Value::as_str),
                    pair.get(1).and_then(Value::as_str),
                ) {
                    (Some(name), Some(value))
                        if !name.is_empty()
                            && !name.contains(['=', '\0'])
                            && !value.contains('\0') =>
                    {
                        env.push((name.to_owned(), value.to_owned()));
                    }
                    _ => return Err("env entries must be name and value strings".to_owned()),
                }
            }
            let umask = request["umask"]
                .as_u64()
                .and_then(|mask| u32::try_from(mask).ok())
                .filter(|mask| *mask <= 0o777)
                .ok_or("umask must be a number from 0 to 0777")?;
            let object = request["limits"]
                .as_object()
                .ok_or("limits must be an object")?;
            let mut limits = Vec::with_capacity(LIMIT_NAMES.len());
            for name in LIMIT_NAMES {
                let pair = object
                    .get(name)
                    .and_then(Value::as_array)
                    .filter(|pair| pair.len() == 2)
                    .ok_or("each limit must be a [soft, hard] pair")?;
                let (Some(soft), Some(hard)) = (pair[0].as_u64(), pair[1].as_u64()) else {
                    return Err(format!("the {name} limit must hold two numbers"));
                };
                limits.push(Limit { name, soft, hard });
            }
            Ok(Self {
                argv,
                cwd,
                env,
                umask,
                limits,
            })
        }
    }

    /// Apply the client's umask and limits in the child. Any failure is returned as the
    /// reason to decline, because a run under different limits is not the same run.
    fn apply_inherited_settings(request: &RunRequest) -> std::result::Result<(), String> {
        // SAFETY: umask has no failure mode, and the child is single-threaded.
        unsafe {
            libc::umask(request.umask as _);
        }
        for limit in &request.limits {
            let resource = limit_resource(limit.name)
                .ok_or_else(|| format!("unknown limit {}", limit.name))?;
            let value = libc::rlimit {
                rlim_cur: limit.soft as _,
                rlim_max: limit.hard as _,
            };
            // SAFETY: setrlimit reads value, which lives for the call.
            if unsafe { libc::setrlimit(resource as _, &value) } != 0 {
                return Err(format!(
                    "cannot apply the client's {} limit: {}",
                    limit.name,
                    io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    fn strings(value: &Value) -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect()
    }

    /// Everything the child needs, moved into it by the fork.
    struct ChildSetup {
        listener_fd: RawFd,
        held: Vec<RawFd>,
        descriptors: Vec<OwnedFd>,
        connection: UnixStream,
        run: RunCallback,
        request: RunRequest,
    }

    /// The child of a fork. It becomes the run and exits with the run's status.
    fn run_child(setup: ChildSetup) -> ! {
        let ChildSetup {
            listener_fd,
            held,
            descriptors,
            connection: inherited,
            run,
            request,
        } = setup;
        // Move every descriptor the child keeps above 2 first, so that dup2 onto 0, 1 and 2
        // cannot overwrite one of them.
        let connection = match duplicate_high(inherited.as_raw_fd()) {
            Ok(fd) => UnixStream::from(fd),
            Err(error) => fail(&format!("could not keep the client connection: {error}")),
        };
        drop(inherited);
        let mut stdio = Vec::with_capacity(3);
        for descriptor in &descriptors {
            match duplicate_high(descriptor.as_raw_fd()) {
                Ok(fd) => stdio.push(fd),
                Err(error) => fail(&format!("could not take the client's stdio: {error}")),
            }
        }
        drop(descriptors);
        // SAFETY: these descriptors were inherited from the parent and the child does not
        // use them; closing them keeps other clients' connections from staying open here.
        unsafe {
            libc::close(listener_fd);
            for fd in held {
                libc::close(fd);
            }
        }
        for (target, source) in stdio.iter().enumerate() {
            // SAFETY: dup2 replaces descriptor 0, 1 or 2 with a copy of one we own.
            if unsafe { libc::dup2(source.as_raw_fd(), target as RawFd) } < 0 {
                fail(&format!(
                    "could not take the client's stdio: {}",
                    io::Error::last_os_error()
                ));
            }
        }
        drop(stdio);
        reset_signals();
        if let Err(error) = std::env::set_current_dir(&request.cwd) {
            fail(&format!("cannot enter {}: {error}", request.cwd));
        }
        let names: Vec<OsString> = std::env::vars_os().map(|(name, _)| name).collect();
        for name in names {
            std::env::remove_var(name);
        }
        for (name, value) in &request.env {
            std::env::set_var(name, value);
        }
        let mut context = RunContext { connection };
        if let Err(reason) = apply_inherited_settings(&request) {
            // Declined before any output: the client runs the program under its own settings.
            context.decline(&reason);
            std::process::exit(0);
        }
        let status = (run)(&request.argv, &mut context);
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        std::process::exit(status)
    }

    /// Report a child-side failure on stderr and exit as a failed run.
    fn fail(message: &str) -> ! {
        let _ = writeln!(io::stderr(), "execute: {message}");
        std::process::exit(1)
    }

    /// Default dispositions for the signals a daemon may have changed, and an empty mask.
    fn reset_signals() {
        for signal in [
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGQUIT,
            libc::SIGCHLD,
        ] {
            // SAFETY: restoring SIG_DFL is always valid in a single-threaded child.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
            }
        }
        // SAFETY: the set is initialized by sigemptyset before use.
        unsafe {
            let mut set: libc::sigset_t = mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, ptr::null_mut());
        }
    }

    /// A new descriptor, at least 3, that refers to the same file as `fd`.
    fn duplicate_high(fd: RawFd) -> io::Result<OwnedFd> {
        // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor for the caller, or -1.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: copy is a fresh descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(copy) })
    }

    /// Whether the client has closed its end. Data it left unread is not a hangup.
    fn client_hung_up(connection: &UnixStream) -> bool {
        let mut byte = 0u8;
        // SAFETY: byte is a valid one-byte buffer; MSG_PEEK leaves queued data in place.
        let read = unsafe {
            libc::recv(
                connection.as_raw_fd(),
                ptr::addr_of_mut!(byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        read == 0
    }

    fn signal_child(pid: libc::pid_t, signal: libc::c_int) {
        // SAFETY: kill is safe to call with any pid; a stale pid fails harmlessly.
        unsafe {
            libc::kill(pid, signal);
        }
    }

    fn wait_nohang(pid: libc::pid_t) -> Waited {
        let mut raw: libc::c_int = 0;
        // SAFETY: raw is a valid out-pointer for waitpid.
        let result = unsafe { libc::waitpid(pid, &mut raw, libc::WNOHANG) };
        if result == 0 {
            Waited::Running
        } else if result == pid {
            Waited::Exited(exit_status(raw))
        } else {
            Waited::Gone
        }
    }

    fn wait_blocking(pid: libc::pid_t) -> Option<i32> {
        let mut raw: libc::c_int = 0;
        // SAFETY: raw is a valid out-pointer for waitpid.
        let result = unsafe { libc::waitpid(pid, &mut raw, 0) };
        (result == pid).then(|| exit_status(raw))
    }

    /// The shell convention for a wait status: the exit code, or 128 plus the signal.
    fn exit_status(raw: libc::c_int) -> i32 {
        let signal = raw & 0x7f;
        if signal == 0 {
            (raw >> 8) & 0xff
        } else {
            128 + signal
        }
    }

    /// Receive one request line and any descriptors sent with it. The descriptors are
    /// owned from the moment they arrive, so an error here closes them.
    fn receive_request(stream: &UnixStream) -> std::result::Result<(Value, Vec<OwnedFd>), String> {
        let mut buffer = Vec::new();
        let mut descriptors = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut control = [0u64; CONTROL_WORDS];
            let mut iov = libc::iovec {
                iov_base: chunk.as_mut_ptr().cast(),
                iov_len: chunk.len(),
            };
            // SAFETY: msghdr is plain data, and all zero bytes are a valid starting value.
            let mut message: libc::msghdr = unsafe { mem::zeroed() };
            message.msg_iov = &mut iov as *mut libc::iovec;
            message.msg_iovlen = 1 as _;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = mem::size_of_val(&control) as _;
            // SAFETY: every pointer in message refers to a live local buffer for the call.
            let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
            if received < 0 {
                return Err(format!(
                    "reading the request failed: {}",
                    io::Error::last_os_error()
                ));
            }
            if received == 0 {
                return Err("the connection closed before the request ended".to_owned());
            }
            collect_descriptors(&message, &mut descriptors);
            buffer.extend_from_slice(&chunk[..received as usize]);
            if buffer.contains(&b'\n') {
                break;
            }
            if buffer.len() > MAX_HEADER_BYTES {
                return Err("the request is too large".to_owned());
            }
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(buffer.len());
        let request = serde_json::from_slice(&buffer[..end])
            .map_err(|_| "the request is not JSON".to_owned())?;
        Ok((request, descriptors))
    }

    /// Take the descriptors from the first control message, which is the only kind a
    /// client sends with a request.
    fn collect_descriptors(message: &libc::msghdr, out: &mut Vec<OwnedFd>) {
        // SAFETY: CMSG_FIRSTHDR reads only the msghdr it is given and returns a pointer
        // into the control buffer that recvmsg filled, or null.
        let header = unsafe { libc::CMSG_FIRSTHDR(message) };
        if header.is_null() {
            return;
        }
        // SAFETY: header points at a complete control message inside the buffer.
        let (level, kind, length): (libc::c_int, libc::c_int, usize) = unsafe {
            (
                (*header).cmsg_level,
                (*header).cmsg_type,
                (*header).cmsg_len as _,
            )
        };
        if level != libc::SOL_SOCKET || kind != libc::SCM_RIGHTS {
            return;
        }
        // SAFETY: CMSG_LEN(0) is arithmetic on its argument only.
        let header_length = unsafe { libc::CMSG_LEN(0) } as usize;
        let count = length.saturating_sub(header_length) / mem::size_of::<RawFd>();
        // SAFETY: the kernel placed `count` descriptors at the data offset of this header.
        let data = unsafe { libc::CMSG_DATA(header) } as *const RawFd;
        for index in 0..count {
            // SAFETY: index is within the descriptors the kernel delivered.
            let fd = unsafe { data.add(index).read_unaligned() };
            // SAFETY: each delivered descriptor is ours from here on, and is wrapped at once
            // so that an error path closes it.
            out.push(unsafe { OwnedFd::from_raw_fd(fd) });
            // SAFETY: fcntl on a descriptor just received.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
    }

    /// Write a request and send this process's stdin, stdout and stderr with it.
    fn send_with_stdio(stream: &UnixStream, bytes: &[u8]) -> io::Result<()> {
        let descriptors: [RawFd; 3] =
            [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO];
        let payload = mem::size_of_val(&descriptors) as libc::c_uint;
        // SAFETY: CMSG_SPACE and CMSG_LEN are arithmetic on their argument.
        let space = unsafe { libc::CMSG_SPACE(payload) } as usize;
        if space > mem::size_of::<[u64; CONTROL_WORDS]>() {
            return Err(io::Error::other("control message too large"));
        }
        let mut control = [0u64; CONTROL_WORDS];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        // SAFETY: msghdr is plain data, and all zero bytes are a valid starting value.
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut iov as *mut libc::iovec;
        message.msg_iovlen = 1 as _;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        // SAFETY: the control buffer holds `space` bytes: one header and three descriptors.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            if header.is_null() {
                return Err(io::Error::other("no control header"));
            }
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(payload) as _;
            let data = libc::CMSG_DATA(header) as *mut RawFd;
            for (index, fd) in descriptors.iter().enumerate() {
                data.add(index).write_unaligned(*fd);
            }
        }
        // SAFETY: message and the buffers it names are live for the call.
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let sent = sent as usize;
        if sent < bytes.len() {
            // The descriptors went with the first bytes; the rest is plain data.
            let mut writer: &UnixStream = stream;
            writer.write_all(&bytes[sent..])?;
        }
        Ok(())
    }

    /// The environment to send, without the selector that asked for the daemon. A
    /// variable that is not UTF-8 cannot be sent, so the run stays here.
    fn environment_for_daemon() -> std::result::Result<Vec<(String, String)>, String> {
        let mut pairs = Vec::new();
        for (name, value) in std::env::vars_os() {
            if name.to_str() == Some("EXECUTE_DAEMON") {
                continue;
            }
            match (name.into_string(), value.into_string()) {
                (Ok(name), Ok(value)) => pairs.push((name, value)),
                _ => return Err("the environment is not valid UTF-8; running here".to_owned()),
            }
        }
        Ok(pairs)
    }

    /// The signals a client forwards to its run.
    struct Forwarded {
        signal: libc::c_int,
        flag: Arc<AtomicBool>,
        id: signal_hook::SigId,
    }

    fn register_forwarded_signals() -> io::Result<Vec<Forwarded>> {
        let mut forwarded: Vec<Forwarded> = Vec::new();
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            let flag = Arc::new(AtomicBool::new(false));
            match signal_hook::flag::register(signal, Arc::clone(&flag)) {
                Ok(id) => forwarded.push(Forwarded { signal, flag, id }),
                Err(error) => {
                    unregister_forwarded(&forwarded);
                    return Err(error);
                }
            }
        }
        Ok(forwarded)
    }

    fn unregister_forwarded(forwarded: &[Forwarded]) {
        for item in forwarded {
            let _ = signal_hook::low_level::unregister(item.id);
        }
    }

    /// This process's umask. Reading it sets a temporary mask and restores the old one, which
    /// is safe because the client is single-threaded here.
    fn client_umask() -> u32 {
        // SAFETY: umask has no failure mode; the client is single-threaded.
        unsafe {
            let previous = libc::umask(0);
            libc::umask(previous);
            previous as _
        }
    }

    /// This process's soft and hard limits for each of `LIMIT_NAMES`.
    fn client_limits() -> std::result::Result<serde_json::Map<String, Value>, String> {
        let mut limits = serde_json::Map::new();
        for name in LIMIT_NAMES {
            let resource = limit_resource(name).ok_or_else(|| format!("unknown limit {name}"))?;
            let mut value = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: getrlimit writes into value, which lives for the call.
            if unsafe { libc::getrlimit(resource as _, &mut value) } != 0 {
                return Err(format!(
                    "could not read the {name} limit ({}); running here",
                    io::Error::last_os_error()
                ));
            }
            limits.insert(name.to_owned(), json!([value.rlim_cur, value.rlim_max]));
        }
        Ok(limits)
    }

    /// Send a `run` request and relay the daemon's events until the run ends.
    pub(super) fn forward(arguments: &[String], socket: &Path) -> Forwarding {
        let environment = match environment_for_daemon() {
            Ok(environment) => environment,
            Err(note) => return Forwarding::Cold { note: Some(note) },
        };
        let cwd = match std::env::current_dir()
            .ok()
            .and_then(|dir| dir.into_os_string().into_string().ok())
        {
            Some(cwd) => cwd,
            None => {
                return Forwarding::Cold {
                    note: Some("the working directory is not valid UTF-8; running here".to_owned()),
                }
            }
        };
        let umask = client_umask();
        let limits = match client_limits() {
            Ok(limits) => limits,
            Err(note) => return Forwarding::Cold { note: Some(note) },
        };
        let mut stream = match connect_trusted(socket) {
            Ok(stream) => stream,
            Err(DaemonError::NotRunning { .. }) => return Forwarding::Cold { note: None },
            Err(error) => {
                return Forwarding::Cold {
                    note: Some(format!("{error}; running here")),
                }
            }
        };
        let forwarded = match register_forwarded_signals() {
            Ok(forwarded) => forwarded,
            Err(error) => {
                return Forwarding::Cold {
                    note: Some(format!("could not forward signals ({error}); running here")),
                }
            }
        };
        if stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .is_err()
        {
            unregister_forwarded(&forwarded);
            return Forwarding::Cold {
                note: Some("could not set up the daemon connection; running here".to_owned()),
            };
        }
        let header = json!({
            "protocol": PROTOCOL,
            "op": "run",
            "argv": arguments,
            "cwd": cwd,
            "env": environment,
            "umask": umask,
            "limits": limits,
        });
        let mut line = header.to_string().into_bytes();
        line.push(b'\n');
        if let Err(error) = send_with_stdio(&stream, &line) {
            unregister_forwarded(&forwarded);
            return Forwarding::Cold {
                note: Some(format!(
                    "could not send the run to the daemon ({error}); running here"
                )),
            };
        }
        let outcome = relay(&mut stream, &forwarded);
        unregister_forwarded(&forwarded);
        outcome
    }

    enum Event {
        Started(libc::pid_t),
        Declined(String),
        Exited(i32),
    }

    fn parse_event(line: &[u8]) -> Option<Event> {
        let value: Value = serde_json::from_slice(line).ok()?;
        match value["event"].as_str()? {
            "started" => Some(Event::Started(
                libc::pid_t::try_from(value["pid"].as_i64()?).ok()?,
            )),
            "fallback" => Some(Event::Declined(
                value["reason"].as_str().unwrap_or("unspecified").to_owned(),
            )),
            "exit" => Some(Event::Exited(
                i32::try_from(value["status"].as_i64()?).ok()?,
            )),
            _ => None,
        }
    }

    fn take_line(pending: &mut Vec<u8>) -> Option<Vec<u8>> {
        let end = pending.iter().position(|byte| *byte == b'\n')?;
        let mut line: Vec<u8> = pending.drain(..=end).collect();
        line.pop();
        Some(line)
    }

    /// Read events until the run ends. Signals are forwarded to the child's pid once the
    /// daemon has reported it.
    fn relay(stream: &mut UnixStream, forwarded: &[Forwarded]) -> Forwarding {
        let mut child: Option<libc::pid_t> = None;
        let mut pending = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(pid) = child {
                for item in forwarded {
                    if item.flag.swap(false, Ordering::SeqCst) {
                        signal_child(pid, item.signal);
                    }
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) => return ended_early(child),
                Ok(count) => pending.extend_from_slice(&chunk[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(_) => return ended_early(child),
            }
            while let Some(line) = take_line(&mut pending) {
                match parse_event(&line) {
                    Some(Event::Started(pid)) => child = Some(pid),
                    Some(Event::Declined(reason)) => {
                        return Forwarding::Cold {
                            note: Some(format!(
                                "the daemon will not run this program ({reason}); running it here"
                            )),
                        }
                    }
                    Some(Event::Exited(status)) => return Forwarding::Ran(status),
                    None => {}
                }
            }
        }
    }

    /// The connection ended. Before the run started the program can still run here; after
    /// it started, running it again could repeat its effects, so the run is reported failed.
    fn ended_early(child: Option<libc::pid_t>) -> Forwarding {
        match child {
            None => Forwarding::Cold {
                note: Some(
                    "the daemon closed the connection before it started the run; running here"
                        .to_owned(),
                ),
            },
            Some(_) => {
                let _ = writeln!(
                    io::stderr(),
                    "execute: the daemon connection ended before the run finished"
                );
                Forwarding::Ran(1)
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{not_running, Forwarding, Path, Result, RunCallback, Value};
    use anyhow::bail;

    pub(super) fn euid() -> u32 {
        0
    }

    pub(super) fn status(socket: &Path) -> Value {
        not_running(socket, None)
    }

    pub(super) fn stop(_socket: &Path) -> Result<()> {
        bail!("the daemon needs Unix sockets; this build has none")
    }

    pub(super) fn serve(_socket: &Path, _run: RunCallback) -> Result<()> {
        bail!("the daemon needs Unix sockets; this build has none")
    }

    pub(super) fn forward(_arguments: &[String], _socket: &Path) -> Forwarding {
        Forwarding::Cold { note: None }
    }
}
