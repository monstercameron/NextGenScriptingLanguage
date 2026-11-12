//! Privileged steps: a step that can run a command with elevated rights, or that can
//! run another program chosen by its arguments.
//!
//! Classification is by executable name, after its directory and any `.exe` suffix
//! are removed, compared case-insensitively. Both the name written in the program and
//! the name of the executable that would actually run (the canonical resolved path,
//! see `capabilities::check_privileged_launch`) are checked, so an absolute path
//! or a `PATH` alias that keeps the listed file name, and a symlink whose
//! canonical target keeps the listed file name, are still classified the same
//! way. A trailing version (`python3.12`, `lua5.4`) is ignored the same
//! way. A renamed copy or hardlink (same bytes, unlisted file name) is not
//! recognised: classification is by name, not content. A name that is not on a list is not
//! recognised: this is a review aid and a policy hook, not a sandbox and not a
//! complete confinement boundary. Fail-closed enforcement is the strict capability
//! policy itself with OS confinement in untrusted mode; `deny_privileged` only
//! refuses the known names below.
//!
//! The lists are conservative. `deny_privileged` in a capability policy refuses every
//! listed elevator, and it also refuses every wrapper and shell listed here whatever
//! their arguments, because their arguments can name a program the program text
//! never mentions (including host-escape, scheduling, D-Bus, and desktop-launch
//! arguments such as `flatpak-spawn --host`, `at` job text, a `systemd1` D-Bus call,
//! or `gio open`). A wrapper can be allowed by exact normalized name with
//! `allow_wrappers`. Elevators cannot be allowed.

/// Executables that run a command with elevated rights.
pub const ELEVATORS: &[&str] = &[
    "sudo",
    "sudo-rs",
    "pkexec",
    "doas",
    "run0",
    "su",
    "runuser",
    "systemd-run",
    "machinectl",
    "gsudo",
    "please",
    "runas",
    "sg",
    "newgrp",
];

/// Launchers whose argument list can name another program to run.
pub const WRAPPERS: &[&str] = &[
    "env",
    "nice",
    "nohup",
    "timeout",
    "stdbuf",
    "setsid",
    "busybox",
    "flock",
    "chroot",
    "unshare",
    "nsenter",
    "ionice",
    "chrt",
    "taskset",
    "time",
    "watch",
    "script",
    "start-stop-daemon",
    "setpriv",
    // Command runners and sandbox or session launchers that start another program.
    "find",
    "fd",
    "xargs",
    "ssh",
    "parallel",
    "strace",
    "gdb",
    "fakeroot",
    "capsh",
    "bwrap",
    "firejail",
    "tmux",
    "screen",
    "expect",
    // Container-host escape and container CLIs: the arguments name the program
    // that runs outside Execute's view (for example `flatpak-spawn --host`,
    // `toolbox run`, `docker exec`, `kubectl exec`). Denied whatever the
    // subcommand, so a benign-looking invocation cannot pass as harmless.
    "flatpak-spawn",
    "host-spawn",
    "distrobox",
    "distrobox-host-exec",
    "toolbox",
    "docker",
    "podman",
    "kubectl",
    // Scheduling and deferred execution: the command runs later, outside the run.
    "at",
    "batch",
    "crontab",
    // D-Bus launchers: a message can start a transient unit, activate a name,
    // or reach host services such as `org.freedesktop.systemd1` and
    // `org.freedesktop.Flatpak`. Denied whatever the destination.
    "dbus-send",
    "gdbus",
    "busctl",
    // Desktop launchers: the arguments name a URI or file handed to a desktop
    // handler outside Execute's view (for example `gio open`).
    "xdg-open",
    "gio",
];

/// Interpreters, text processors and tools with command hooks (`awk 'BEGIN{system()}'`,
/// `sed e`, `git -c core.pager=`, `tar --to-command`, `less !`). They are ordinary glue
/// in most scripts, and their program text is usually visible in the step's arguments,
/// so `explain` and the effects gate do not mark them privileged. A capability policy
/// with `deny_privileged` treats them as wrappers: refused unless `allow_wrappers` names
/// them.
pub const INTERPRETERS: &[&str] = &[
    "python", "python2", "python3", "pypy", "pypy3", "perl", "ruby", "node", "nodejs", "lua",
    "php", "awk", "gawk", "mawk", "nawk", "sed", "git", "make", "gmake", "tar", "rsync", "vim",
    "vi", "less", "man",
];

/// Shells. A shell runs a command string, a script, or standard input, so its
/// arguments can name any program. They are denied whatever their arguments are.
pub const SHELLS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "fish",
    "ksh",
    "mksh",
    "ash",
    "csh",
    "tcsh",
    "yash",
    "posh",
    "cmd",
    "powershell",
    "pwsh",
];

/// How an executable relates to privilege. Higher variants are more restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Privilege {
    Standard,
    /// A launcher whose arguments can name another program.
    Wrapper,
    /// A known privilege elevator.
    Elevator,
}

impl Privilege {
    /// The stable label used in machine-readable output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Wrapper => "wrapper",
            Self::Elevator => "elevator",
        }
    }
}

/// The normalized name of an executable: its file name, without a `.exe` suffix in
/// any case, in lowercase. Returns `None` when there is no file name.
pub fn normalized_name(binary: &str) -> Option<String> {
    // Both separators are honoured on every platform, so a Windows path is classified
    // the same way when this runs elsewhere.
    let file = binary.rsplit(['/', '\\']).next()?;
    if file.is_empty() {
        return None;
    }
    let stem = match file.get(file.len().saturating_sub(4)..) {
        Some(suffix) if file.len() > 4 && suffix.eq_ignore_ascii_case(".exe") => {
            &file[..file.len() - 4]
        }
        _ => file,
    };
    Some(stem.to_ascii_lowercase())
}

/// `name` without a trailing version (`python3.12` -> `python`, `lua5.4` -> `lua`,
/// `node-18` -> `node`), so versioned installs are classified like the plain name.
fn unversioned(name: &str) -> &str {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let stem = stem.strip_suffix('-').unwrap_or(stem);
    if stem.is_empty() {
        name
    } else {
        stem
    }
}

fn classify_one(name: &str) -> Privilege {
    let base = classify_exact(name);
    let unversioned = unversioned(name);
    if unversioned == name {
        base
    } else {
        base.max(classify_exact(unversioned))
    }
}

fn classify_exact(name: &str) -> Privilege {
    if ELEVATORS.contains(&name) {
        Privilege::Elevator
    } else if WRAPPERS.contains(&name) || INTERPRETERS.contains(&name) || SHELLS.contains(&name) {
        Privilege::Wrapper
    } else {
        Privilege::Standard
    }
}

/// The most restrictive classification of any of the given executable names.
pub fn classify_names<'a>(binaries: impl IntoIterator<Item = &'a str>) -> Privilege {
    binaries
        .into_iter()
        .filter_map(normalized_name)
        .map(|name| classify_one(&name))
        .max()
        .unwrap_or(Privilege::Standard)
}

/// Classify one executable by name, as written in the program.
pub fn classify(binary: &str) -> Privilege {
    classify_names([binary])
}

/// Whether a binary name or path is a known privilege elevator.
pub fn is_elevator(binary: &str) -> bool {
    classify(binary) == Privilege::Elevator
}

/// Whether a step is privileged or is a wrapper that can run another program. This is
/// the name-only answer; `explain` combines it with the resolved path below so an
/// alias, symlink, or absolute path to a listed program is still privileged.
pub fn is_privileged_invocation(binary: &str) -> bool {
    match classify(binary) {
        Privilege::Standard => false,
        Privilege::Wrapper => !is_interpreter(binary),
        Privilege::Elevator => true,
    }
}

/// Whether an explain row is a privileged step, given the binary as written and the
/// canonical resolved path when it is known. An invocation whose binary is unknown
/// is privileged, so an unclassified step cannot pass as harmless. A known binary
/// that resolves to a listed launcher or elevator is privileged even when the
/// written name is an unlisted alias, which keeps `explain` consistent with
/// `capabilities::check_privileged_launch`. A symlink whose canonical target
/// keeps the listed file name is likewise caught. A renamed copy or hardlink
/// whose file name is not listed is not recognised, and an alias that fails to
/// resolve falls back to the written name, so runtime enforcement remains
/// authoritative.
pub fn is_privileged_step(binary: Option<&str>, resolved_path: Option<&str>) -> bool {
    if let Some(path) = resolved_path {
        let privileged = match classify(path) {
            Privilege::Standard => false,
            Privilege::Wrapper => !is_interpreter(path),
            Privilege::Elevator => true,
        };
        if privileged {
            return true;
        }
    }
    binary.is_none_or(is_privileged_invocation)
}

/// Whether the name is on [`INTERPRETERS`]: a wrapper for `deny_privileged`, but not
/// marked privileged by `explain`.
pub fn is_interpreter(binary: &str) -> bool {
    normalized_name(binary).is_some_and(|name| {
        INTERPRETERS.contains(&name.as_str()) || INTERPRETERS.contains(&unversioned(&name))
    })
}

/// Validated drop-in install (issue #321): stage a copy beside the target, validate the
/// staged copy with the owning tool, install it atomically, run the matching reload, and
/// put the previous bytes back when the reload fails.
///
/// The sequence is written over injected steps so it can be tested without root and
/// without the real tools. The caller supplies:
/// - `validate(staged)`: runs the owning tool's checker on the staged path;
/// - `install(replacement, target)`: atomically replaces the target with the staged bytes
///   (owner, group and mode are this step's job), or removes the target;
/// - `reload()`: runs the matching reload.
///
/// Staged copy naming. The staged file is `execute-stage-<pid>-<n>.tmp` in the target's
/// own directory, so the final rename stays on one filesystem and is atomic. The name
/// contains a `.` and does not end in `~`: sudo ignores files in `sudoers.d` whose names
/// contain a `.` or end in `~`, so a staged copy that is left behind by a crash is never
/// read as a live sudoers file. The `.tmp` suffix is also not a suffix any drop-in
/// directory loads (`.conf`, `.rules`, ...). The name is created with `create_new`, so it
/// never reuses an existing file. On Unix the staged copy is created with mode 0600.
///
/// Placement. This module is self-contained and uses no helper from `applets`. The
/// `applets::transaction` helpers are `pub(super)` to `applets`, so they cannot be reached
/// from here. The applet that finally calls `install_dropin` (`system.install_dropin`)
/// should live in `applets`, and it must run the whole sequence under the elevated helper,
/// because staging writes into `/etc/...`.
pub mod dropin {
    use std::fmt::Write as _;
    use std::fs::{self, OpenOptions};
    use std::io::{ErrorKind, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use sha2::{Digest, Sha256};

    static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);
    const STAGE_ATTEMPTS: u32 = 100;

    /// What the install step should leave at the target.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Replacement<'a> {
        /// Atomically replace the target with the bytes of this staged file.
        Staged(&'a Path),
        /// Remove the target. Used only to undo an install of a file that did not exist.
        Remove,
    }

    /// What happened when the reload failed and the previous state was restored.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Rollback {
        /// The previous bytes are back (or the target is gone again) and the second reload
        /// succeeded.
        Restored,
        /// The previous state is back, but the reload that followed the restore also failed.
        RestoredReloadFailed { message: String },
        /// The restore itself failed. The new bytes are still at `left_in_place`.
        RestoreFailed {
            message: String,
            left_in_place: PathBuf,
        },
    }

    /// The first failure in the sequence. Nothing after it ran.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Failure {
        /// The target could not be read, so the previous state is unknown.
        Read { message: String },
        /// The staged copy could not be created. Nothing was installed.
        Stage { message: String },
        /// The validator refused the staged copy. The target was not touched.
        Validation { message: String },
        /// The install step failed. No reload ran.
        Install { message: String },
        /// The install succeeded and the reload failed. `rollback` says what was done.
        Reload { message: String, rollback: Rollback },
    }

    /// The result of `install_dropin`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Outcome {
        /// The target now holds the new bytes (an install ran and succeeded).
        pub changed: bool,
        /// The validator accepted the staged copy.
        pub validated: bool,
        /// The reload for the new bytes succeeded.
        pub reloaded: bool,
        /// SHA-256 of the bytes the target held before the call, if it existed.
        pub previous_sha256: Option<String>,
        /// The first failure, if any.
        pub failure: Option<Failure>,
        /// Staged copies that could not be removed. Empty when cleanup succeeded.
        pub staged_leftovers: Vec<PathBuf>,
    }

    /// Stage, validate, install and reload `new_bytes` at `target`. See the module
    /// documentation for the steps and their contract.
    ///
    /// If the target already holds exactly `new_bytes`, no step runs and `changed` is false.
    pub fn install_dropin<V, I, R>(
        target: &Path,
        new_bytes: &[u8],
        mut validate: V,
        mut install: I,
        mut reload: R,
    ) -> Outcome
    where
        V: FnMut(&Path) -> Result<(), String>,
        I: FnMut(Replacement<'_>, &Path) -> Result<(), String>,
        R: FnMut() -> Result<(), String>,
    {
        let previous = match fs::read(target) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Outcome {
                    failure: Some(Failure::Read {
                        message: format!("cannot read {}: {error}", target.display()),
                    }),
                    ..Outcome::empty(None)
                };
            }
        };
        let previous_sha256 = previous.as_deref().map(sha256_hex);
        if previous.as_deref() == Some(new_bytes) {
            return Outcome::empty(previous_sha256);
        }

        let mut out = Outcome::empty(previous_sha256);
        let stage = match create_stage(target, new_bytes) {
            Ok(stage) => stage,
            Err(message) => {
                out.failure = Some(Failure::Stage { message });
                return out;
            }
        };
        if let Err(message) = validate(&stage.path) {
            out.failure = Some(Failure::Validation { message });
            out.staged_leftovers.extend(stage.finish());
            return out;
        }
        out.validated = true;
        let installed = install(Replacement::Staged(&stage.path), target);
        out.staged_leftovers.extend(stage.finish());
        if let Err(message) = installed {
            out.failure = Some(Failure::Install { message });
            return out;
        }
        out.changed = true;

        if let Err(reload_message) = reload() {
            let restored = match previous.as_deref() {
                Some(bytes) => match create_stage(target, bytes) {
                    Ok(stage) => {
                        let result = install(Replacement::Staged(&stage.path), target);
                        out.staged_leftovers.extend(stage.finish());
                        result
                    }
                    Err(message) => Err(message),
                },
                None => install(Replacement::Remove, target),
            };
            let rollback = match restored {
                Err(message) => Rollback::RestoreFailed {
                    message,
                    left_in_place: target.to_path_buf(),
                },
                Ok(()) => match reload() {
                    Ok(()) => Rollback::Restored,
                    Err(message) => Rollback::RestoredReloadFailed { message },
                },
            };
            out.changed = true;
            out.failure = Some(Failure::Reload {
                message: reload_message,
                rollback,
            });
            return out;
        }
        out.reloaded = true;
        out
    }

    impl Outcome {
        fn empty(previous_sha256: Option<String>) -> Self {
            Self {
                changed: false,
                validated: false,
                reloaded: false,
                previous_sha256,
                failure: None,
                staged_leftovers: Vec::new(),
            }
        }
    }

    /// A staged copy. It is removed when `finish` is called, and on drop if it was not.
    struct Stage {
        path: PathBuf,
        done: bool,
    }

    impl Stage {
        /// Remove the staged copy. Returns its path if it could not be removed.
        fn finish(mut self) -> Option<PathBuf> {
            self.done = true;
            match fs::remove_file(&self.path) {
                Ok(()) => None,
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(_) => Some(self.path.clone()),
            }
        }
    }

    impl Drop for Stage {
        fn drop(&mut self) {
            if !self.done {
                fs::remove_file(&self.path).ok();
            }
        }
    }

    fn create_stage(target: &Path, bytes: &[u8]) -> Result<Stage, String> {
        let dir = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let pid = std::process::id();
        for _ in 0..STAGE_ATTEMPTS {
            let sequence = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("execute-stage-{pid}-{sequence}.tmp"));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            match options.open(&path) {
                Ok(mut file) => {
                    // The guard exists before the write, so a failed write still removes it.
                    let stage = Stage { path, done: false };
                    file.write_all(bytes)
                        .and_then(|()| file.sync_all())
                        .map_err(|error| {
                            format!("cannot write staged copy {}: {error}", stage.path.display())
                        })?;
                    return Ok(stage);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "cannot create staged copy in {}: {error}",
                        dir.display()
                    ));
                }
            }
        }
        Err(format!(
            "no free staged-copy name in {} after {STAGE_ATTEMPTS} attempts",
            dir.display()
        ))
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hex = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// The command for an argument vector. The target or staged path, when given, is
    /// appended as its own argument. Nothing goes through a shell. An empty vector is an
    /// error, not a panic.
    pub fn command_for(argv: &[String], path: Option<&Path>) -> Result<Command, String> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| "empty command: the manifest gives no program".to_string())?;
        let mut command = Command::new(program);
        command.args(args);
        if let Some(path) = path {
            command.arg(path);
        }
        Ok(command)
    }

    /// Run an argument vector (see [`command_for`]). The error is the command's stderr, or
    /// its stdout, or its exit status.
    pub fn run_argv(argv: &[String], path: Option<&Path>) -> Result<(), String> {
        let mut command = command_for(argv, path)?;
        let program = command.get_program().to_string_lossy().into_owned();
        let output = command
            .output()
            .map_err(|error| format!("cannot run {program}: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("{program} exited with {}", output.status)
        })
    }
}

/// The runtime-owned sudo session (issue #320).
///
/// Omarchy's update script asks for the password once with `sudo true` (not `sudo -v`:
/// under the default `verifypw=all`, `-v` prompts even with passwordless sudo enabled),
/// refreshes the timestamp every 60 seconds while the run lives, revokes it with `sudo -k`
/// on every exit path, and withdraws it before third-party code runs. Those later calls
/// authenticate with the non-updating form (`sudo -N`).
///
/// This module holds the argument vectors and the state machine. It runs nothing itself:
/// the injected runner executes each `SudoCall` exactly as given (no shell), and it is the
/// only place sudo runs. The caller passes `now` to `authorize` and `tick`, so there is no
/// clock, thread or timer here. No password, prompt text or other user input is in an
/// argument vector, an environment entry or an event. sudo reads the password itself from
/// the terminal the runner gives it.
///
/// Lifetime limit. `Drop` runs only when the process unwinds. A process killed with
/// SIGKILL runs no code, so its timestamp survives until `timestamp_timeout` expires. The
/// runtime must stop the keepalive with the run and must revoke through a helper that
/// outlives a killed run (issue #320). This module cannot do that alone.
pub mod sudo_session {
    use std::ffi::OsString;
    use std::io;
    use std::time::{Duration, Instant};

    /// How long after a successful authorize or refresh `tick` waits before refreshing.
    /// Omarchy refreshes every 60 seconds.
    pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

    /// The PATH every sudo call runs with. It is fixed and holds no user or package
    /// directory, so a `sudo` or `true` placed earlier in the user's PATH (or in a
    /// PKGBUILD's) is never the one that runs. It is the Ubuntu and Debian `secure_path`
    /// list, which sudo uses by default there.
    pub const SANITIZED_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

    /// The environment every sudo call runs with. `LC_ALL=C` keeps sudo's messages in one
    /// language, so nothing that reads them depends on the user's locale.
    pub const SUDO_ENV: &[(&str, &str)] = &[("PATH", SANITIZED_PATH), ("LC_ALL", "C")];

    /// One sudo invocation as data: `argv[0]` is `sudo`, and `env` is the complete
    /// environment it runs with. The runner executes this exactly, without a shell.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SudoCall {
        pub argv: Vec<OsString>,
        pub env: &'static [(&'static str, &'static str)],
    }

    fn call(args: &[&str]) -> SudoCall {
        SudoCall {
            argv: std::iter::once("sudo")
                .chain(args.iter().copied())
                .map(OsString::from)
                .collect(),
            env: SUDO_ENV,
        }
    }

    /// `sudo true`: the prompting authorization. It is not `sudo -v`, which prompts under
    /// the default `verifypw=all` even when passwordless sudo is enabled.
    pub fn authorize_call() -> SudoCall {
        call(&["true"])
    }

    /// `sudo -n true`: refresh the timestamp. `-n` makes sudo fail instead of prompting.
    pub fn refresh_call() -> SudoCall {
        call(&["-n", "true"])
    }

    /// `sudo -k`: drop the cached credential.
    pub fn revoke_call() -> SudoCall {
        call(&["-k"])
    }

    /// `sudo -N <command...>`: the non-updating form, for a command run in an untrusted
    /// phase. The command is a vector of arguments, never a shell string.
    pub fn no_update_call(command: &[OsString]) -> SudoCall {
        let mut argv = vec![OsString::from("sudo"), OsString::from("-N")];
        argv.extend(command.iter().cloned());
        SudoCall {
            argv,
            env: SUDO_ENV,
        }
    }

    /// The part of an exit status the session needs. A process the runner could not start
    /// is reported as an `Err` from the runner instead.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Exit {
        pub success: bool,
        pub code: Option<i32>,
    }

    /// What a transition did. It carries no password and no user input.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SudoEvent {
        Authorized,
        Refreshed,
        RefreshFailed,
        Withheld,
        Revoked,
        RevokeFailed,
    }

    /// A refused or failed transition.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SudoError {
        /// `sudo true` failed (wrong password, no permission, or cancelled). The session
        /// stays Unauthorized.
        AuthorizeFailed { code: Option<i32> },
        /// `authorize` was called on a session that is not Unauthorized. No sudo ran.
        NotUnauthorized,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum State {
        Unauthorized,
        Active {
            refreshed_at: Instant,
        },
        /// A refresh failed. `tick` does nothing more, and the failure was reported once.
        Lost,
        /// Withheld for an untrusted phase. `tick` does nothing, and sudo is never refreshed.
        Withheld,
        Ended,
    }

    /// A run-level sudo session over an injected runner.
    pub struct SudoSession<R>
    where
        R: FnMut(&SudoCall) -> io::Result<Exit>,
    {
        runner: R,
        interval: Duration,
        state: State,
        /// True from a successful authorize until a revoke succeeds. A failed revoke leaves
        /// it true, so a later revoke (including the one in `Drop`) tries again.
        credential_may_be_live: bool,
    }

    impl<R> SudoSession<R>
    where
        R: FnMut(&SudoCall) -> io::Result<Exit>,
    {
        pub fn new(runner: R, interval: Duration) -> Self {
            Self {
                runner,
                interval,
                state: State::Unauthorized,
                credential_may_be_live: false,
            }
        }

        /// True while `tick` will refresh the credential.
        pub fn is_active(&self) -> bool {
            matches!(self.state, State::Active { .. })
        }

        /// Ask for the password with `sudo true`. Moves Unauthorized to Active.
        pub fn authorize(&mut self, now: Instant) -> Result<SudoEvent, SudoError> {
            if self.state != State::Unauthorized {
                return Err(SudoError::NotUnauthorized);
            }
            self.run(&authorize_call())
                .map_err(|code| SudoError::AuthorizeFailed { code })?;
            self.credential_may_be_live = true;
            self.state = State::Active { refreshed_at: now };
            Ok(SudoEvent::Authorized)
        }

        /// Refresh with `sudo -n true` when the interval has elapsed since the last
        /// refresh, and only while Active. A failed refresh moves the session to Lost and
        /// is returned once. The refresh never prompts.
        pub fn tick(&mut self, now: Instant) -> Option<SudoEvent> {
            let State::Active { refreshed_at } = self.state else {
                return None;
            };
            if now.saturating_duration_since(refreshed_at) < self.interval {
                return None;
            }
            match self.run(&refresh_call()) {
                Ok(()) => {
                    self.state = State::Active { refreshed_at: now };
                    Some(SudoEvent::Refreshed)
                }
                Err(_) => {
                    self.state = State::Lost;
                    Some(SudoEvent::RefreshFailed)
                }
            }
        }

        /// Revoke, then withhold the session so later phases cannot ride a cached
        /// credential. Returns the revoke outcome (if a revoke was needed) and then
        /// `Withheld`. Once withheld, `tick` does nothing.
        pub fn withhold(&mut self) -> Vec<SudoEvent> {
            let mut events: Vec<SudoEvent> = self.revoke().into_iter().collect();
            if matches!(
                self.state,
                State::Unauthorized | State::Active { .. } | State::Lost
            ) {
                self.state = State::Withheld;
                events.push(SudoEvent::Withheld);
            }
            events
        }

        /// Revoke on the way out of the run, and end the session. Idempotent: once a
        /// revoke has succeeded, a second call runs no sudo. A failed revoke is returned
        /// as `RevokeFailed`, and the next revoke tries again.
        pub fn end(&mut self) -> Option<SudoEvent> {
            let event = self.revoke();
            self.state = State::Ended;
            event
        }

        /// `None` when no credential may be live. Otherwise runs `sudo -k` and reports it.
        fn revoke(&mut self) -> Option<SudoEvent> {
            if !self.credential_may_be_live {
                return None;
            }
            match self.run(&revoke_call()) {
                Ok(()) => {
                    self.credential_may_be_live = false;
                    Some(SudoEvent::Revoked)
                }
                Err(_) => Some(SudoEvent::RevokeFailed),
            }
        }

        fn run(&mut self, call: &SudoCall) -> Result<(), Option<i32>> {
            match (self.runner)(call) {
                Ok(exit) if exit.success => Ok(()),
                Ok(exit) => Err(exit.code),
                Err(_) => Err(None),
            }
        }
    }

    impl<R> Drop for SudoSession<R>
    where
        R: FnMut(&SudoCall) -> io::Result<Exit>,
    {
        /// Revokes a credential this session may still hold. The runner's answer is ignored,
        /// because nothing can be reported from `drop`. A run that is killed with SIGKILL
        /// never gets here, which is why the runtime also ends the keepalive with the run.
        fn drop(&mut self) {
            let _ = self.revoke();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_elevator_is_recognised_by_name_and_path() {
        for name in ELEVATORS {
            assert!(is_elevator(name), "{name} should be an elevator");
            assert!(is_elevator(&format!("/usr/bin/{name}")), "{name} by path");
            assert_eq!(classify(name), Privilege::Elevator);
        }
    }

    #[test]
    fn the_exe_suffix_is_stripped_in_any_case() {
        for binary in [
            "sudo.exe",
            "SUDO.EXE",
            "Sudo.Exe",
            "C:\\Tools\\sudo.exe",
            "/bin/su.exe",
        ] {
            assert!(is_elevator(binary), "{binary} should be an elevator");
        }
        assert_eq!(normalized_name("PKEXEC.EXE").as_deref(), Some("pkexec"));
    }

    #[test]
    fn similar_names_are_not_elevators() {
        for binary in [
            "printf",
            "/usr/bin/sudoers-helper",
            "pseudo",
            "sudo-rs-tool",
            "subu",
            "",
        ] {
            assert_eq!(classify(binary), Privilege::Standard, "{binary:?}");
        }
    }

    #[test]
    fn wrappers_and_shells_are_wrappers_not_elevators() {
        for name in WRAPPERS.iter().chain(SHELLS) {
            assert_eq!(classify(name), Privilege::Wrapper, "{name}");
            assert!(is_privileged_invocation(name));
        }
        assert_eq!(classify("env.exe"), Privilege::Wrapper);
    }

    #[test]
    fn common_code_and_command_launchers_are_privileged_wrappers() {
        for name in [
            "python", "python3", "perl", "ruby", "node", "awk", "find", "fd", "git", "ssh", "make",
            "sed", "tar", "rsync", "parallel", "strace", "gdb", "fakeroot", "capsh", "bwrap",
            "firejail", "tmux", "screen", "expect", "vim", "less", "man",
        ] {
            assert_eq!(classify(name), Privilege::Wrapper, "{name}");
        }
    }

    #[test]
    fn versioned_names_are_classified_like_the_plain_name() {
        for name in [
            "python3.12",
            "perl5.36",
            "lua5.4",
            "php8.2",
            "node-18",
            "pypy3.10",
        ] {
            assert_eq!(classify(name), Privilege::Wrapper, "{name}");
        }
        assert_eq!(classify("/usr/bin/python3.12"), Privilege::Wrapper);
        assert_eq!(classify("bash5"), Privilege::Wrapper);
        assert_eq!(classify("sha256sum"), Privilege::Standard);
        assert_eq!(classify("7z"), Privilege::Standard);
    }

    #[test]
    fn interpreters_are_wrappers_for_policy_but_not_privileged_in_explain() {
        for name in INTERPRETERS {
            assert_eq!(classify(name), Privilege::Wrapper, "{name}");
            assert!(!is_privileged_invocation(name), "{name}");
        }
        for name in ["xargs", "ssh", "find", "env", "bash", "sudo"] {
            assert!(is_privileged_invocation(name), "{name}");
        }
    }

    #[test]
    fn the_most_restrictive_name_wins() {
        assert_eq!(
            classify_names(["env", "sudo-link-name"]),
            Privilege::Wrapper,
            "an unlisted name does not hide a listed one"
        );
        assert_eq!(classify_names(["env", "sudo"]), Privilege::Elevator);
        assert_eq!(classify_names(["printf"]), Privilege::Standard);
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(Privilege::Elevator.label(), "elevator");
        assert_eq!(Privilege::Wrapper.label(), "wrapper");
        assert_eq!(Privilege::Standard.label(), "standard");
    }

    #[test]
    fn host_escape_launchers_are_privileged_wrappers() {
        for name in [
            "flatpak-spawn",
            "host-spawn",
            "distrobox",
            "distrobox-host-exec",
            "toolbox",
            "docker",
            "podman",
            "kubectl",
        ] {
            assert_eq!(classify(name), Privilege::Wrapper, "{name}");
            assert!(is_privileged_invocation(name), "{name}");
            assert!(
                is_privileged_invocation(&format!("/usr/bin/{name}")),
                "{name} by absolute path"
            );
        }
    }

    #[test]
    fn schedulers_are_privileged_wrappers_whatever_their_arguments() {
        for name in ["at", "batch", "crontab"] {
            assert_eq!(classify(name), Privilege::Wrapper, "{name}");
            assert!(is_privileged_invocation(name), "{name}");
            assert!(
                is_privileged_invocation(&format!("/usr/bin/{name}")),
                "{name} by absolute path"
            );
        }
    }

    #[test]
    fn dbus_and_desktop_launchers_are_privileged_wrappers() {
        for name in ["dbus-send", "gdbus", "busctl", "xdg-open", "gio"] {
            assert_eq!(classify(name), Privilege::Wrapper, "{name}");
            assert!(is_privileged_invocation(name), "{name}");
        }
        // Case, directory, and Windows suffix do not hide a launcher.
        assert!(is_privileged_invocation("/usr/bin/DBUS-SEND"));
        assert!(is_privileged_invocation("C:\\Windows\\System32\\gio.exe"));
        assert!(is_privileged_invocation("/usr/bin/xdg-open"));
    }

    #[test]
    fn resolved_path_keeps_an_alias_privileged() {
        // The written name is unlisted, but the executable that would run is a
        // listed launcher: still privileged, matching `check_privileged_launch`.
        assert!(is_privileged_step(Some("tool"), Some("/usr/bin/docker")));
        assert!(is_privileged_step(Some("tool"), Some("/usr/bin/at")));
        assert!(is_privileged_step(Some("tool"), Some("/usr/bin/dbus-send")));
        assert!(is_privileged_step(Some("tool"), Some("/usr/bin/gio")));
        assert!(is_privileged_step(
            Some("tool"),
            Some("/usr/bin/flatpak-spawn")
        ));
        assert!(!is_privileged_step(Some("printf"), Some("/usr/bin/printf")));
        assert!(!is_privileged_step(Some("printf"), None));
        // An unknown binary stays privileged (fail closed).
        assert!(is_privileged_step(None, None));
    }

    // ---- dropin::install_dropin (issue #321): fake steps, no root, no real tools ----

    use dropin::{Failure, Replacement, Rollback};
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};

    /// The real-file install used by the fake steps: copy the staged bytes over the
    /// target, or remove the target.
    fn copy_or_remove(replacement: Replacement<'_>, target: &Path) -> Result<(), String> {
        match replacement {
            Replacement::Staged(staged) => std::fs::copy(staged, target)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Replacement::Remove => std::fs::remove_file(target).map_err(|error| error.to_string()),
        }
    }

    fn entries(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn dropin_identical_bytes_run_no_step_and_report_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dev-link");
        std::fs::write(&target, b"alpha\n").unwrap();
        let log = RefCell::new(Vec::<String>::new());
        let out = dropin::install_dropin(
            &target,
            b"alpha\n",
            |_| {
                log.borrow_mut().push("validate".into());
                Ok(())
            },
            |_, _| {
                log.borrow_mut().push("install".into());
                Ok(())
            },
            || {
                log.borrow_mut().push("reload".into());
                Ok(())
            },
        );
        assert!(
            log.borrow().is_empty(),
            "no step may run: {:?}",
            log.borrow()
        );
        assert!(!out.changed);
        assert!(!out.validated);
        assert!(!out.reloaded);
        assert!(out.failure.is_none());
        assert_eq!(out.previous_sha256.as_deref().map(str::len), Some(64));
        assert_eq!(entries(dir.path()), 1, "nothing may be staged");
    }

    #[test]
    fn dropin_stages_beside_the_target_under_a_name_the_owning_tool_ignores() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dev-link");
        let seen = RefCell::new(None::<PathBuf>);
        let out = dropin::install_dropin(
            &target,
            b"Defaults:dev !requiretty\n",
            |staged| {
                assert_eq!(
                    std::fs::read(staged).unwrap(),
                    b"Defaults:dev !requiretty\n"
                );
                #[cfg(unix)]
                assert_eq!(
                    std::os::unix::fs::PermissionsExt::mode(
                        &std::fs::metadata(staged).unwrap().permissions()
                    ) & 0o777,
                    0o600
                );
                *seen.borrow_mut() = Some(staged.to_path_buf());
                Ok(())
            },
            copy_or_remove,
            || Ok(()),
        );
        assert!(out.failure.is_none(), "{:?}", out.failure);
        assert!(out.changed && out.validated && out.reloaded);
        assert!(out.staged_leftovers.is_empty());
        let staged = seen.borrow().clone().unwrap();
        assert_eq!(
            staged.parent(),
            Some(dir.path()),
            "same directory as the target"
        );
        let name = staged.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.contains('.'),
            "sudo ignores names containing '.': {name}"
        );
        assert!(
            !name.ends_with('~'),
            "sudo ignores names ending in '~': {name}"
        );
        assert!(!staged.exists(), "staged copy is removed after install");
        assert_eq!(entries(dir.path()), 1, "only the target remains");
    }

    #[test]
    fn dropin_failed_validation_leaves_the_target_byte_for_byte_and_never_installs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dev-link");
        std::fs::write(&target, b"old\n").unwrap();
        let log = RefCell::new(Vec::<String>::new());
        let out = dropin::install_dropin(
            &target,
            b"bad line\n",
            |_| {
                log.borrow_mut().push("validate".into());
                Err("parse error in staged file, line 1".to_string())
            },
            |r, t| {
                log.borrow_mut().push("install".into());
                copy_or_remove(r, t)
            },
            || {
                log.borrow_mut().push("reload".into());
                Ok(())
            },
        );
        assert_eq!(*log.borrow(), ["validate"]);
        assert_eq!(
            out.failure,
            Some(Failure::Validation {
                message: "parse error in staged file, line 1".to_string()
            })
        );
        assert!(!out.changed && !out.validated && !out.reloaded);
        assert_eq!(std::fs::read(&target).unwrap(), b"old\n");
        assert_eq!(entries(dir.path()), 1, "staged copy removed on failure");
        assert!(out.staged_leftovers.is_empty());
    }

    #[test]
    fn dropin_failed_install_runs_no_reload_and_removes_the_staged_copy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dev-link");
        std::fs::write(&target, b"old\n").unwrap();
        let log = RefCell::new(Vec::<String>::new());
        let out = dropin::install_dropin(
            &target,
            b"new\n",
            |_| {
                log.borrow_mut().push("validate".into());
                Ok(())
            },
            |_, _| {
                log.borrow_mut().push("install".into());
                Err("permission denied".to_string())
            },
            || {
                log.borrow_mut().push("reload".into());
                Ok(())
            },
        );
        assert_eq!(*log.borrow(), ["validate", "install"]);
        assert_eq!(
            out.failure,
            Some(Failure::Install {
                message: "permission denied".to_string()
            })
        );
        assert!(!out.changed && out.validated && !out.reloaded);
        assert_eq!(std::fs::read(&target).unwrap(), b"old\n");
        assert_eq!(entries(dir.path()), 1);
    }

    #[test]
    fn dropin_reload_failure_restores_the_previous_bytes_and_reloads_again() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("50-ssh.conf");
        std::fs::write(&target, b"old\n").unwrap();
        let log = RefCell::new(Vec::<String>::new());
        let reloads = Cell::new(0u32);
        let out = dropin::install_dropin(
            &target,
            b"new\n",
            |_| {
                log.borrow_mut().push("validate".into());
                Ok(())
            },
            |r, t| {
                log.borrow_mut().push(
                    match r {
                        Replacement::Staged(_) => "install-staged",
                        Replacement::Remove => "install-remove",
                    }
                    .into(),
                );
                copy_or_remove(r, t)
            },
            || {
                log.borrow_mut().push("reload".into());
                let n = reloads.get() + 1;
                reloads.set(n);
                if n == 1 {
                    Err("unit failed to start".to_string())
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(
            *log.borrow(),
            [
                "validate",
                "install-staged",
                "reload",
                "install-staged",
                "reload"
            ]
        );
        assert_eq!(
            out.failure,
            Some(Failure::Reload {
                message: "unit failed to start".to_string(),
                rollback: Rollback::Restored,
            })
        );
        assert!(out.changed && out.validated && !out.reloaded);
        assert_eq!(std::fs::read(&target).unwrap(), b"old\n");
        assert_eq!(entries(dir.path()), 1, "no staged copy left behind");
        assert!(out.staged_leftovers.is_empty());
    }

    #[test]
    fn dropin_reload_failure_on_a_new_file_removes_the_target_again() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("60-new.conf");
        let log = RefCell::new(Vec::<String>::new());
        let out = dropin::install_dropin(
            &target,
            b"new\n",
            |_| Ok(()),
            |r, t| {
                log.borrow_mut().push(
                    match r {
                        Replacement::Staged(_) => "install-staged",
                        Replacement::Remove => "install-remove",
                    }
                    .into(),
                );
                copy_or_remove(r, t)
            },
            || {
                log.borrow_mut().push("reload".into());
                Err("nope".to_string())
            },
        );
        assert_eq!(
            *log.borrow(),
            ["install-staged", "reload", "install-remove", "reload"]
        );
        assert_eq!(
            out.failure,
            Some(Failure::Reload {
                message: "nope".to_string(),
                rollback: Rollback::RestoredReloadFailed {
                    message: "nope".to_string()
                },
            })
        );
        assert!(
            !target.exists(),
            "a file that did not exist before is removed"
        );
        assert_eq!(entries(dir.path()), 0);
    }

    #[test]
    fn dropin_failed_restore_says_so_and_names_the_file_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("50-ssh.conf");
        std::fs::write(&target, b"old\n").unwrap();
        let installs = Cell::new(0u32);
        let out = dropin::install_dropin(
            &target,
            b"new\n",
            |_| Ok(()),
            |r, t| {
                let n = installs.get() + 1;
                installs.set(n);
                if n == 2 {
                    Err("disk full".to_string())
                } else {
                    copy_or_remove(r, t)
                }
            },
            || Err("unit failed".to_string()),
        );
        assert_eq!(
            out.failure,
            Some(Failure::Reload {
                message: "unit failed".to_string(),
                rollback: Rollback::RestoreFailed {
                    message: "disk full".to_string(),
                    left_in_place: target.clone(),
                },
            })
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"new\n",
            "new bytes remain"
        );
        assert_eq!(
            entries(dir.path()),
            1,
            "the failed restore's staged copy is removed"
        );
    }

    #[test]
    fn dropin_command_vectors_pass_the_path_as_its_own_argument() {
        use std::ffi::OsStr;
        let argv = vec!["visudo".to_string(), "-cf".to_string()];
        let path = Path::new("/etc/sudoers.d/dir with spaces; rm -rf /");
        let command = dropin::command_for(&argv, Some(path)).unwrap();
        assert_eq!(command.get_program(), OsStr::new("visudo"));
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, [OsStr::new("-cf"), path.as_os_str()]);
    }

    #[test]
    fn dropin_empty_command_vector_is_an_error_not_a_panic() {
        assert!(dropin::command_for(&[], None).is_err());
        assert!(dropin::run_argv(&[], Some(Path::new("/x"))).is_err());
    }

    use std::ffi::OsString;
    use std::time::{Duration, Instant};
    use sudo_session::{
        no_update_call, Exit, SudoCall, SudoError, SudoEvent, SudoSession, REFRESH_INTERVAL,
        SUDO_ENV,
    };

    type CallLog = std::rc::Rc<std::cell::RefCell<Vec<SudoCall>>>;

    /// A runner that records every call. `script` gives the success of each call in order;
    /// calls past the end of `script` succeed.
    fn sudo_fake(log: &CallLog, script: &[bool]) -> impl FnMut(&SudoCall) -> std::io::Result<Exit> {
        let log = log.clone();
        let mut script: std::collections::VecDeque<bool> = script.iter().copied().collect();
        move |call: &SudoCall| {
            log.borrow_mut().push(call.clone());
            let success = script.pop_front().unwrap_or(true);
            Ok(Exit {
                success,
                code: Some(if success { 0 } else { 1 }),
            })
        }
    }

    fn argvs(log: &CallLog) -> Vec<Vec<String>> {
        log.borrow()
            .iter()
            .map(|call| {
                call.argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn sudo_happy_path_authorizes_refreshes_and_revokes_with_fixed_env() {
        let log = CallLog::default();
        let t0 = Instant::now();
        let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
        assert_eq!(session.authorize(t0), Ok(SudoEvent::Authorized));
        assert!(session.is_active());
        assert_eq!(
            session.tick(t0 + REFRESH_INTERVAL),
            Some(SudoEvent::Refreshed)
        );
        assert_eq!(session.end(), Some(SudoEvent::Revoked));
        assert_eq!(
            argvs(&log),
            vec![
                words(&["sudo", "true"]),
                words(&["sudo", "-n", "true"]),
                words(&["sudo", "-k"]),
            ]
        );
        assert!(log.borrow().iter().all(|call| call.env == SUDO_ENV));
        assert!(SUDO_ENV.contains(&("PATH", sudo_session::SANITIZED_PATH)));
        assert!(SUDO_ENV.contains(&("LC_ALL", "C")));
    }

    #[test]
    fn sudo_refresh_waits_for_the_interval_then_fires_once_per_interval() {
        let log = CallLog::default();
        let t0 = Instant::now();
        let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
        session.authorize(t0).unwrap();
        assert_eq!(session.tick(t0), None);
        assert_eq!(session.tick(t0 + Duration::from_secs(59)), None);
        assert_eq!(
            session.tick(t0 + Duration::from_secs(60)),
            Some(SudoEvent::Refreshed)
        );
        assert_eq!(session.tick(t0 + Duration::from_secs(61)), None);
        assert_eq!(session.tick(t0 + Duration::from_secs(119)), None);
        assert_eq!(
            session.tick(t0 + Duration::from_secs(120)),
            Some(SudoEvent::Refreshed)
        );
        let refreshes = argvs(&log)
            .into_iter()
            .filter(|argv| *argv == words(&["sudo", "-n", "true"]))
            .count();
        assert_eq!(refreshes, 2);
    }

    #[test]
    fn sudo_failed_refresh_is_reported_once_and_never_prompts() {
        let log = CallLog::default();
        let t0 = Instant::now();
        // Call 1 (authorize) succeeds, call 2 (refresh) fails, call 3 (revoke) succeeds.
        let mut session = SudoSession::new(sudo_fake(&log, &[true, false]), REFRESH_INTERVAL);
        session.authorize(t0).unwrap();
        assert_eq!(
            session.tick(t0 + REFRESH_INTERVAL),
            Some(SudoEvent::RefreshFailed)
        );
        assert!(!session.is_active());
        assert_eq!(session.tick(t0 + Duration::from_secs(300)), None);
        assert_eq!(session.tick(t0 + Duration::from_secs(600)), None);
        assert_eq!(session.end(), Some(SudoEvent::Revoked));
        assert_eq!(
            argvs(&log),
            vec![
                words(&["sudo", "true"]),
                words(&["sudo", "-n", "true"]),
                words(&["sudo", "-k"]),
            ]
        );
    }

    #[test]
    fn sudo_withhold_revokes_and_tick_then_does_nothing() {
        let log = CallLog::default();
        let t0 = Instant::now();
        let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
        session.authorize(t0).unwrap();
        assert_eq!(
            session.withhold(),
            vec![SudoEvent::Revoked, SudoEvent::Withheld]
        );
        assert_eq!(session.tick(t0 + Duration::from_secs(600)), None);
        assert_eq!(session.end(), None);
        assert_eq!(
            argvs(&log),
            vec![words(&["sudo", "true"]), words(&["sudo", "-k"])]
        );
    }

    #[test]
    fn sudo_withhold_cannot_be_authorized_again() {
        let log = CallLog::default();
        let t0 = Instant::now();
        let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
        session.authorize(t0).unwrap();
        session.withhold();
        assert_eq!(session.authorize(t0), Err(SudoError::NotUnauthorized));
        assert_eq!(argvs(&log).len(), 2);
    }

    #[test]
    fn sudo_end_twice_revokes_once() {
        let log = CallLog::default();
        let t0 = Instant::now();
        let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
        session.authorize(t0).unwrap();
        assert_eq!(session.end(), Some(SudoEvent::Revoked));
        assert_eq!(session.end(), None);
        assert_eq!(
            argvs(&log),
            vec![words(&["sudo", "true"]), words(&["sudo", "-k"])]
        );
    }

    #[test]
    fn sudo_drop_while_active_revokes() {
        let log = CallLog::default();
        let t0 = Instant::now();
        {
            let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
            session.authorize(t0).unwrap();
        }
        assert_eq!(
            argvs(&log),
            vec![words(&["sudo", "true"]), words(&["sudo", "-k"])]
        );
    }

    #[test]
    fn sudo_drop_after_end_does_not_revoke_again() {
        let log = CallLog::default();
        let t0 = Instant::now();
        {
            let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
            session.authorize(t0).unwrap();
            session.end();
        }
        assert_eq!(
            argvs(&log),
            vec![words(&["sudo", "true"]), words(&["sudo", "-k"])]
        );
    }

    #[test]
    fn sudo_revoke_failure_is_reported_and_the_drop_retries_it() {
        let log = CallLog::default();
        let t0 = Instant::now();
        {
            // Call 1 (authorize) succeeds, call 2 (end's revoke) fails, call 3 (drop) succeeds.
            let mut session = SudoSession::new(sudo_fake(&log, &[true, false]), REFRESH_INTERVAL);
            session.authorize(t0).unwrap();
            assert_eq!(session.end(), Some(SudoEvent::RevokeFailed));
        }
        assert_eq!(
            argvs(&log),
            vec![
                words(&["sudo", "true"]),
                words(&["sudo", "-k"]),
                words(&["sudo", "-k"]),
            ]
        );
    }

    #[test]
    fn sudo_withhold_reports_a_failed_revoke_and_end_retries_it() {
        let log = CallLog::default();
        let t0 = Instant::now();
        let mut session = SudoSession::new(sudo_fake(&log, &[true, false]), REFRESH_INTERVAL);
        session.authorize(t0).unwrap();
        assert_eq!(
            session.withhold(),
            vec![SudoEvent::RevokeFailed, SudoEvent::Withheld]
        );
        assert_eq!(session.tick(t0 + REFRESH_INTERVAL), None);
        assert_eq!(session.end(), Some(SudoEvent::Revoked));
        assert_eq!(session.end(), None);
        assert_eq!(argvs(&log).len(), 3);
    }

    #[test]
    fn sudo_failed_authorize_stays_unauthorized_and_leaves_nothing_to_revoke() {
        let log = CallLog::default();
        let t0 = Instant::now();
        {
            let mut session = SudoSession::new(sudo_fake(&log, &[false]), REFRESH_INTERVAL);
            assert_eq!(
                session.authorize(t0),
                Err(SudoError::AuthorizeFailed { code: Some(1) })
            );
            assert_eq!(session.tick(t0 + REFRESH_INTERVAL), None);
        }
        assert_eq!(argvs(&log), vec![words(&["sudo", "true"])]);
    }

    #[test]
    fn sudo_second_authorize_is_refused_without_running_sudo() {
        let log = CallLog::default();
        let t0 = Instant::now();
        let mut session = SudoSession::new(sudo_fake(&log, &[]), REFRESH_INTERVAL);
        session.authorize(t0).unwrap();
        assert_eq!(session.authorize(t0), Err(SudoError::NotUnauthorized));
        assert_eq!(argvs(&log), vec![words(&["sudo", "true"])]);
    }

    #[test]
    fn sudo_no_update_call_is_an_argument_vector_on_the_fixed_env() {
        let command = [OsString::from("makepkg"), OsString::from("--syncdeps")];
        let call = no_update_call(&command);
        assert_eq!(
            call.argv,
            vec![
                OsString::from("sudo"),
                OsString::from("-N"),
                OsString::from("makepkg"),
                OsString::from("--syncdeps"),
            ]
        );
        assert_eq!(call.env, SUDO_ENV);
    }
}
