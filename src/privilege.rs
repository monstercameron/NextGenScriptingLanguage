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

    pub(super) fn sha256_hex(bytes: &[u8]) -> String {
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

    /// `sudo -n -- <command...>`: run a fixed command without prompting. `-n` makes sudo fail
    /// instead of asking for a password, and `--` ends sudo's own options, so the first word
    /// of the command is never read as a sudo option. The command is a vector, never a shell.
    pub fn non_interactive_call(command: &[OsString]) -> SudoCall {
        let mut argv = vec![
            OsString::from("sudo"),
            OsString::from("-n"),
            OsString::from("--"),
        ];
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

/// Elevated applet calls (issue #317).
///
/// One in-process applet call runs as root through a minimal helper, `execute-applet`
/// (subcommand `elevated-applet`), instead of through a root shell. The interpreter keeps its
/// own privileges. It runs `sudo -n -- <helper> elevated-applet` with the JSON request on
/// standard input, and the helper calls `authorize_elevated` again before it dispatches,
/// whatever the interpreter already checked.
///
/// This module is pure. It builds and parses the request, decides whether an applet and its
/// paths may run as root, builds the transport argument vector, and renders the audit record
/// for explain and what-if. It runs no process and reads no file. The allow-list and the
/// parameter table are compiled in, and a test checks the table against the shipped manifests.
///
/// Path patterns are absolute and split on `/`. `*` matches any run of characters inside one
/// component and never a `/`. `**` is a whole component that matches one or more whole
/// components, so `/etc/sudoers.d/**` matches `/etc/sudoers.d/x` but not `/etc/sudoers.d`.
/// Matching is anchored at both ends, so a prefix of a longer name never matches.
pub mod elevated {
    use std::ffi::OsString;
    use std::fmt;

    use serde::de::{self, Deserializer, MapAccess, Visitor};
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Map, Value};

    use super::dropin::sha256_hex;
    use super::sudo_session::{non_interactive_call, SudoCall};

    /// The protocol id of the request, in the house style (`execute.<name>/<n>`).
    pub const PROTOCOL: &str = "execute.elevated_applet/1";
    /// The request version. It must agree with the version at the end of `PROTOCOL`.
    pub const VERSION: u32 = 1;
    /// The largest request the helper reads, in bytes.
    pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
    /// The fixed subcommand the transport passes to the helper.
    pub const HELPER_SUBCOMMAND: &str = "elevated-applet";
    const MAX_APPLET_NAME_BYTES: usize = 64;
    const MAX_PATH_BYTES: usize = 4096;

    /// How one parameter of an allowed applet is treated.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        /// A filesystem path. It must be absolute, normalized and inside the policy.
        Path,
        /// Text written to the target. The audit record holds its SHA-256, never the text.
        Content,
        /// A file mode. The audit record shows it.
        Mode,
        /// An owner. The audit record shows it.
        Owner,
        /// A group. The audit record shows it.
        Group,
        /// Any other parameter. It is not checked here; the applet validates it.
        Other,
    }

    /// One applet on the allow-list, with every parameter its shipped manifest declares.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AppletSpec {
        pub name: &'static str,
        pub params: &'static [(&'static str, Kind)],
    }

    /// The applets that may run as root. Each one changes only the files its path parameters
    /// name. None reads arbitrary files, runs a process or uses the network. `fs.unlink_checked`,
    /// `fs.transaction` and the `jsonc.*` applets are not listed; they need a separate decision.
    /// The table repeats each shipped manifest's parameter names, and a test fails when a
    /// manifest gains or loses one.
    pub const ALLOWED_APPLETS: &[AppletSpec] = &[
        AppletSpec {
            name: "fs.ensure_file",
            params: &[
                ("path", Kind::Path),
                ("content", Kind::Content),
                ("mode", Kind::Mode),
            ],
        },
        AppletSpec {
            name: "fs.ensure_line",
            params: &[
                ("path", Kind::Path),
                ("line", Kind::Content),
                ("state", Kind::Other),
            ],
        },
        AppletSpec {
            name: "fs.ensure_dir",
            params: &[("path", Kind::Path), ("mode", Kind::Mode)],
        },
        AppletSpec {
            name: "fs.ensure_symlink",
            params: &[("path", Kind::Path), ("target", Kind::Path)],
        },
        AppletSpec {
            name: "fs.write_atomic",
            params: &[
                ("path", Kind::Path),
                ("lock", Kind::Path),
                ("content", Kind::Content),
                ("mode", Kind::Mode),
                ("expected_content_hash", Kind::Other),
                ("symlink_policy", Kind::Other),
                ("permissions_policy", Kind::Other),
                ("lock_timeout_ms", Kind::Other),
            ],
        },
        AppletSpec {
            name: "conf.set",
            params: &[
                ("path", Kind::Path),
                ("key", Kind::Other),
                ("value", Kind::Content),
                ("separator", Kind::Other),
                ("comment", Kind::Other),
                ("spaced", Kind::Other),
                ("uncomment", Kind::Other),
            ],
        },
        AppletSpec {
            name: "conf.ensure_block",
            params: &[
                ("path", Kind::Path),
                ("id", Kind::Other),
                ("content", Kind::Content),
                ("comment", Kind::Other),
            ],
        },
        AppletSpec {
            name: "conf.remove_block",
            params: &[
                ("path", Kind::Path),
                ("id", Kind::Other),
                ("comment", Kind::Other),
            ],
        },
    ];

    fn spec_for(name: &str) -> Option<&'static AppletSpec> {
        ALLOWED_APPLETS.iter().find(|spec| spec.name == name)
    }

    fn kind_of(spec: &AppletSpec, parameter: &str) -> Option<Kind> {
        spec.params
            .iter()
            .find(|(name, _)| *name == parameter)
            .map(|(_, kind)| *kind)
    }

    /// Why a path was refused.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PathRefusal {
        Empty,
        NulByte,
        TooLong,
        NotAbsolute,
        ParentComponent,
        /// An empty component (`//`, a trailing `/`) or a `.` component.
        Unnormalized,
        NotAllowedByPolicy,
    }

    impl fmt::Display for PathRefusal {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::Empty => "the path is empty",
                Self::NulByte => "the path contains a NUL byte",
                Self::TooLong => "the path is longer than the limit",
                Self::NotAbsolute => "the path is relative; paths must be absolute",
                Self::ParentComponent => "the path contains a '..' component",
                Self::Unnormalized => "the path contains an empty or '.' component",
                Self::NotAllowedByPolicy => "no path pattern in the policy allows this path",
            })
        }
    }

    /// Split an absolute path into its components. A relative path is refused, never resolved.
    fn split_path(path: &str) -> Result<Vec<&str>, PathRefusal> {
        if path.is_empty() {
            return Err(PathRefusal::Empty);
        }
        if path.contains('\0') {
            return Err(PathRefusal::NulByte);
        }
        if path.len() > MAX_PATH_BYTES {
            return Err(PathRefusal::TooLong);
        }
        let Some(rest) = path.strip_prefix('/') else {
            return Err(PathRefusal::NotAbsolute);
        };
        let parts: Vec<&str> = rest.split('/').collect();
        for part in &parts {
            match *part {
                ".." => return Err(PathRefusal::ParentComponent),
                "" | "." => return Err(PathRefusal::Unnormalized),
                _ => {}
            }
        }
        Ok(parts)
    }

    /// A glob component: `*` matches any run of characters, every other byte matches itself.
    /// Anchored at both ends. Iterative, so a long name cannot blow the stack.
    fn wildcard_matches(pattern: &str, text: &str) -> bool {
        let pat = pattern.as_bytes();
        let txt = text.as_bytes();
        let (mut p, mut t) = (0usize, 0usize);
        let mut star: Option<usize> = None;
        let mut resume = 0usize;
        while t < txt.len() {
            if p < pat.len() && pat[p] == b'*' {
                star = Some(p);
                resume = t;
                p += 1;
            } else if p < pat.len() && pat[p] == txt[t] {
                p += 1;
                t += 1;
            } else if let Some(star_at) = star {
                p = star_at + 1;
                resume += 1;
                t = resume;
            } else {
                return false;
            }
        }
        pat[p..].iter().all(|&byte| byte == b'*')
    }

    /// Whole-path matching over components. `**` consumes one or more components. Every other
    /// segment consumes exactly one component and must match it with `wildcard_matches`.
    /// The table runs in O(segments x components) and cannot backtrack exponentially.
    fn segments_match(pattern: &[String], path: &[&str]) -> bool {
        let mut reach = vec![false; path.len() + 1];
        reach[0] = true;
        for segment in pattern {
            let mut next = vec![false; path.len() + 1];
            if segment.as_str() == "**" {
                // next[k] is true when some j < k is reachable, so ** took path[j..k].
                let mut seen = false;
                for (slot, &here) in next.iter_mut().zip(reach.iter()) {
                    *slot = seen;
                    seen |= here;
                }
            } else {
                for (index, name) in path.iter().enumerate() {
                    next[index + 1] = reach[index] && wildcard_matches(segment, name);
                }
            }
            reach = next;
        }
        reach[path.len()]
    }

    /// A malformed policy pattern.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PatternError {
        pub pattern: String,
        pub reason: &'static str,
    }

    impl fmt::Display for PatternError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "policy path pattern {:?} {}",
                self.pattern, self.reason
            )
        }
    }

    fn pattern_reason(refusal: PathRefusal) -> &'static str {
        match refusal {
            PathRefusal::Empty => "is empty",
            PathRefusal::NulByte => "contains a NUL byte",
            PathRefusal::TooLong => "is too long",
            PathRefusal::NotAbsolute => "must be absolute",
            PathRefusal::ParentComponent => "contains a '..' component",
            PathRefusal::Unnormalized => "contains an empty or '.' component",
            PathRefusal::NotAllowedByPolicy => "is not a valid pattern",
        }
    }

    fn parse_pattern(pattern: &str) -> Result<Vec<String>, PatternError> {
        let fail = |reason: &'static str| PatternError {
            pattern: pattern.to_string(),
            reason,
        };
        let parts = split_path(pattern).map_err(|refusal| fail(pattern_reason(refusal)))?;
        if parts
            .iter()
            .any(|part| part.contains("**") && *part != "**")
        {
            return Err(fail("uses '**' outside a whole component"));
        }
        Ok(parts.into_iter().map(str::to_string).collect())
    }

    /// The path patterns a helper allows. An empty policy allows nothing.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct ElevationPolicy {
        patterns: Vec<Vec<String>>,
    }

    impl ElevationPolicy {
        /// Build a policy from patterns. A malformed pattern is an error, not a silent drop.
        pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self, PatternError> {
            let mut parsed = Vec::new();
            for pattern in patterns {
                parsed.push(parse_pattern(pattern)?);
            }
            Ok(Self { patterns: parsed })
        }

        /// `Ok` when the path is absolute, normalized and matches at least one pattern.
        pub fn check_path(&self, path: &str) -> Result<(), PathRefusal> {
            let parts = split_path(path)?;
            if self
                .patterns
                .iter()
                .any(|segments| segments_match(segments, &parts))
            {
                Ok(())
            } else {
                Err(PathRefusal::NotAllowedByPolicy)
            }
        }
    }

    /// The largest protocol request that is accepted, checked before parsing.
    fn check_size(bytes: usize) -> Result<(), RequestError> {
        if bytes > MAX_REQUEST_BYTES {
            Err(RequestError::TooLarge {
                bytes,
                limit: MAX_REQUEST_BYTES,
            })
        } else {
            Ok(())
        }
    }

    fn well_formed_applet_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= MAX_APPLET_NAME_BYTES
            && name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'.'
            })
    }

    /// One applet call as data: exactly one applet name and its JSON arguments.
    #[derive(Debug, Clone, PartialEq)]
    pub struct ElevatedRequest {
        applet: String,
        arguments: Map<String, Value>,
    }

    impl ElevatedRequest {
        pub fn new(applet: &str, arguments: Map<String, Value>) -> Result<Self, RequestError> {
            if !well_formed_applet_name(applet) {
                return Err(RequestError::BadAppletName {
                    name: applet.to_string(),
                });
            }
            Ok(Self {
                applet: applet.to_string(),
                arguments,
            })
        }

        pub fn applet(&self) -> &str {
            &self.applet
        }

        pub fn arguments(&self) -> &Map<String, Value> {
            &self.arguments
        }
    }

    /// A request that is malformed or outside the protocol. None of these runs anything.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum RequestError {
        TooLarge { bytes: usize, limit: usize },
        Malformed { message: String },
        WrongProtocol { found: String },
        UnsupportedVersion { found: u32 },
        BadAppletName { name: String },
    }

    impl fmt::Display for RequestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::TooLarge { bytes, limit } => {
                    write!(formatter, "request is {bytes} bytes; the limit is {limit}")
                }
                Self::Malformed { message } => write!(formatter, "malformed request: {message}"),
                Self::WrongProtocol { found } => write!(
                    formatter,
                    "unexpected protocol {found:?}; expected {:?}",
                    PROTOCOL
                ),
                Self::UnsupportedVersion { found } => {
                    write!(formatter, "unsupported request version {found}")
                }
                Self::BadAppletName { name } => {
                    write!(formatter, "applet name {name:?} is not a valid name")
                }
            }
        }
    }

    /// The wire form. `deny_unknown_fields` refuses extra keys, and the arguments must be an
    /// object with no repeated key.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        protocol: String,
        version: u32,
        applet: String,
        #[serde(deserialize_with = "strict_arguments")]
        arguments: Map<String, Value>,
    }

    /// Accepts only a JSON object and refuses a repeated top-level argument key. Values are
    /// parsed as ordinary JSON; a repeated key inside a nested value is not checked here.
    fn strict_arguments<'de, D>(deserializer: D) -> Result<Map<String, Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Arguments;

        impl<'de> Visitor<'de> for Arguments {
            type Value = Map<String, Value>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object of applet arguments")
            }

            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut arguments = Map::new();
                while let Some((key, value)) = access.next_entry::<String, Value>()? {
                    if arguments.contains_key(&key) {
                        return Err(de::Error::custom(format!("duplicate argument {key:?}")));
                    }
                    arguments.insert(key, value);
                }
                Ok(arguments)
            }
        }

        deserializer.deserialize_map(Arguments)
    }

    /// Serialise a request to the bytes the helper reads on standard input. The output is one
    /// JSON object. Its argument keys are in sorted order, so the same request always gives the
    /// same bytes.
    pub fn encode_request(request: &ElevatedRequest) -> Result<Vec<u8>, RequestError> {
        let wire = Wire {
            protocol: PROTOCOL.to_string(),
            version: VERSION,
            applet: request.applet.clone(),
            arguments: request.arguments.clone(),
        };
        let bytes = serde_json::to_vec(&wire).map_err(|error| RequestError::Malformed {
            message: error.to_string(),
        })?;
        check_size(bytes.len())?;
        Ok(bytes)
    }

    /// Parse the helper's standard input. It must be exactly one JSON object with the right
    /// protocol and version, no unknown or repeated top-level key, a valid applet name and an
    /// object of arguments.
    pub fn decode_request(bytes: &[u8]) -> Result<ElevatedRequest, RequestError> {
        check_size(bytes.len())?;
        let wire: Wire =
            serde_json::from_slice(bytes).map_err(|error| RequestError::Malformed {
                message: error.to_string(),
            })?;
        if wire.protocol != PROTOCOL {
            return Err(RequestError::WrongProtocol {
                found: wire.protocol,
            });
        }
        if wire.version != VERSION {
            return Err(RequestError::UnsupportedVersion {
                found: wire.version,
            });
        }
        ElevatedRequest::new(&wire.applet, wire.arguments)
    }

    /// Why a request was refused. Nothing ran.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Refusal {
        AppletNotAllowed {
            applet: String,
        },
        UnknownParameter {
            applet: &'static str,
            parameter: String,
        },
        NotAString {
            applet: &'static str,
            parameter: String,
        },
        Path {
            applet: &'static str,
            parameter: String,
            path: String,
            reason: PathRefusal,
        },
    }

    impl fmt::Display for Refusal {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::AppletNotAllowed { applet } => write!(
                    formatter,
                    "applet {applet:?} is not on the elevation allow-list"
                ),
                Self::UnknownParameter { applet, parameter } => {
                    write!(formatter, "{applet} has no parameter {parameter:?}")
                }
                Self::NotAString { applet, parameter } => {
                    write!(
                        formatter,
                        "parameter {parameter:?} of {applet} must be a string"
                    )
                }
                Self::Path {
                    applet,
                    parameter,
                    path,
                    reason,
                } => write!(
                    formatter,
                    "parameter {parameter:?} of {applet} refused for {path:?}: {reason}"
                ),
            }
        }
    }

    /// A request the helper may run. It can be built only by `authorize_elevated`.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Authorized {
        request: ElevatedRequest,
        spec: &'static AppletSpec,
    }

    impl Authorized {
        pub fn applet(&self) -> &'static str {
            self.spec.name
        }

        pub fn request(&self) -> &ElevatedRequest {
            &self.request
        }
    }

    /// The helper-side check, run again inside the helper whatever the interpreter checked.
    /// The applet must be on the compiled-in list. Every argument must be a declared
    /// parameter. Every path parameter must be a string that passes the path rules and the
    /// policy, and every content parameter must be a string.
    pub fn authorize_elevated(
        request: &ElevatedRequest,
        policy: &ElevationPolicy,
    ) -> Result<Authorized, Refusal> {
        let Some(spec) = spec_for(request.applet()) else {
            return Err(Refusal::AppletNotAllowed {
                applet: request.applet().to_string(),
            });
        };
        for (parameter, value) in request.arguments() {
            let Some(kind) = kind_of(spec, parameter) else {
                return Err(Refusal::UnknownParameter {
                    applet: spec.name,
                    parameter: parameter.clone(),
                });
            };
            if matches!(kind, Kind::Path | Kind::Content) && !value.is_string() {
                return Err(Refusal::NotAString {
                    applet: spec.name,
                    parameter: parameter.clone(),
                });
            }
            if let (Kind::Path, Some(path)) = (kind, value.as_str()) {
                policy.check_path(path).map_err(|reason| Refusal::Path {
                    applet: spec.name,
                    parameter: parameter.clone(),
                    path: path.to_string(),
                    reason,
                })?;
            }
        }
        Ok(Authorized {
            request: request.clone(),
            spec,
        })
    }

    /// The transport the interpreter runs: the argument vector and the bytes for standard
    /// input. The request is on standard input only, never in argv.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ElevatedCall {
        pub sudo: SudoCall,
        pub stdin: Vec<u8>,
    }

    /// Why a transport could not be built.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TransportError {
        /// The helper path is relative, unnormalized or otherwise not a usable absolute path.
        HelperPath(PathRefusal),
        Request(RequestError),
    }

    impl fmt::Display for TransportError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::HelperPath(reason) => {
                    write!(formatter, "the helper path is refused: {reason}")
                }
                Self::Request(error) => write!(formatter, "{error}"),
            }
        }
    }

    /// Build `sudo -n -- <helper> elevated-applet` with the fixed sudo environment and the
    /// encoded request as standard input. The helper path must be absolute. A relative path
    /// is refused rather than resolved, because the helper's working directory is not the
    /// caller's.
    pub fn transport_call(
        helper: &str,
        authorized: &Authorized,
    ) -> Result<ElevatedCall, TransportError> {
        split_path(helper)
            .map(|_| ())
            .map_err(TransportError::HelperPath)?;
        let stdin = encode_request(authorized.request()).map_err(TransportError::Request)?;
        let sudo =
            non_interactive_call(&[OsString::from(helper), OsString::from(HELPER_SUBCOMMAND)]);
        Ok(ElevatedCall { sudo, stdin })
    }

    /// What explain and what-if list for an elevated call. A content argument appears only as
    /// its SHA-256. Owner, group and mode appear when the spec declares them and the call sets
    /// them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AuditRecord {
        pub applet: &'static str,
        /// (parameter, path), in parameter-name order.
        pub paths: Vec<(String, String)>,
        /// (parameter, SHA-256 in lowercase hex) for each content parameter.
        pub content_sha256: Vec<(String, String)>,
        pub mode: Option<Value>,
        pub owner: Option<Value>,
        pub group: Option<Value>,
    }

    pub fn audit_record(authorized: &Authorized) -> AuditRecord {
        let spec = authorized.spec;
        let mut record = AuditRecord {
            applet: spec.name,
            paths: Vec::new(),
            content_sha256: Vec::new(),
            mode: None,
            owner: None,
            group: None,
        };
        for (parameter, value) in authorized.request().arguments() {
            match kind_of(spec, parameter) {
                Some(Kind::Path) => {
                    if let Some(path) = value.as_str() {
                        record.paths.push((parameter.clone(), path.to_string()));
                    }
                }
                Some(Kind::Content) => {
                    if let Some(text) = value.as_str() {
                        record
                            .content_sha256
                            .push((parameter.clone(), sha256_hex(text.as_bytes())));
                    }
                }
                Some(Kind::Mode) => record.mode = Some(value.clone()),
                Some(Kind::Owner) => record.owner = Some(value.clone()),
                Some(Kind::Group) => record.group = Some(value.clone()),
                Some(Kind::Other) | None => {}
            }
        }
        record
    }

    impl AuditRecord {
        pub fn to_json(&self) -> Value {
            let mut object = Map::new();
            object.insert("applet".to_string(), Value::from(self.applet));
            object.insert(
                "paths".to_string(),
                Value::Array(
                    self.paths
                        .iter()
                        .map(|(parameter, path)| json!({"parameter": parameter, "path": path}))
                        .collect(),
                ),
            );
            object.insert(
                "content".to_string(),
                Value::Array(
                    self.content_sha256
                        .iter()
                        .map(
                            |(parameter, digest)| json!({"parameter": parameter, "sha256": digest}),
                        )
                        .collect(),
                ),
            );
            if let Some(mode) = &self.mode {
                object.insert("mode".to_string(), mode.clone());
            }
            if let Some(owner) = &self.owner {
                object.insert("owner".to_string(), owner.clone());
            }
            if let Some(group) = &self.group {
                object.insert("group".to_string(), group.clone());
            }
            Value::Object(object)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::sudo_session::SUDO_ENV;
        use super::*;
        use sha2::{Digest, Sha256};
        use std::fs;
        use std::path::Path;

        fn policy(patterns: &[&str]) -> ElevationPolicy {
            ElevationPolicy::new(patterns.iter().copied()).expect("the test policy is valid")
        }

        fn request(applet: &str, arguments: Value) -> ElevatedRequest {
            let Value::Object(map) = arguments else {
                panic!("test arguments must be a JSON object");
            };
            ElevatedRequest::new(applet, map).expect("the test request is well formed")
        }

        fn outcome(
            patterns: &[&str],
            applet: &str,
            arguments: Value,
        ) -> Result<Authorized, Refusal> {
            authorize_elevated(&request(applet, arguments), &policy(patterns))
        }

        fn allowed(patterns: &[&str], path: &str) -> bool {
            outcome(patterns, "fs.ensure_dir", json!({"path": path})).is_ok()
        }

        fn path_reason(patterns: &[&str], path: &str) -> PathRefusal {
            match outcome(patterns, "fs.ensure_dir", json!({"path": path})) {
                Err(Refusal::Path { reason, .. }) => reason,
                other => panic!("expected a path refusal for {path:?}, got {other:?}"),
            }
        }

        fn sample_authorized() -> Authorized {
            outcome(
                &["/srv/test/**"],
                "fs.ensure_file",
                json!({"path": "/srv/test/a", "content": "SECRET-LINE\n"}),
            )
            .expect("the sample call is allowed")
        }

        #[test]
        fn request_round_trips_and_encodes_one_sorted_json_object() {
            let original = request(
                "fs.ensure_file",
                json!({"path": "/srv/app/a.conf", "content": "x=1\n", "mode": "0640"}),
            );
            let bytes = encode_request(&original).expect("encodes");
            let value: Value = serde_json::from_slice(&bytes).expect("the bytes are JSON");
            assert_eq!(
                value,
                json!({
                    "protocol": PROTOCOL,
                    "version": 1,
                    "applet": "fs.ensure_file",
                    "arguments": {"path": "/srv/app/a.conf", "content": "x=1\n", "mode": "0640"}
                })
            );
            let decoded = decode_request(&bytes).expect("decodes");
            assert_eq!(decoded, original);
            assert_eq!(encode_request(&decoded).expect("re-encodes"), bytes);
        }

        #[test]
        fn request_refuses_extra_fields_wrong_protocol_and_wrong_version() {
            let body = |protocol: &str, version: &str, extra: &str| {
                format!(
                    r#"{{"protocol":"{protocol}","version":{version},"applet":"fs.ensure_dir","arguments":{{"path":"/srv/a"}}{extra}}}"#
                )
            };
            assert!(decode_request(body(PROTOCOL, "1", "").as_bytes()).is_ok());
            assert!(matches!(
                decode_request(body(PROTOCOL, "1", r#","extra":true"#).as_bytes()),
                Err(RequestError::Malformed { .. })
            ));
            assert!(matches!(
                decode_request(body("execute.elevated_applet/2", "1", "").as_bytes()),
                Err(RequestError::WrongProtocol { .. })
            ));
            assert!(matches!(
                decode_request(body(PROTOCOL, "2", "").as_bytes()),
                Err(RequestError::UnsupportedVersion { found: 2 })
            ));
            let without_version = r#"{"protocol":"execute.elevated_applet/1","applet":"fs.ensure_dir","arguments":{}}"#;
            assert!(matches!(
                decode_request(without_version.as_bytes()),
                Err(RequestError::Malformed { .. })
            ));
        }

        #[test]
        fn request_refuses_non_object_arguments_repeated_keys_and_trailing_data() {
            let head =
                r#"{"protocol":"execute.elevated_applet/1","version":1,"applet":"fs.ensure_dir""#;
            for arguments in [r#""/srv/a""#, "[]", "null", "7"] {
                let text = format!(r#"{head},"arguments":{arguments}}}"#);
                assert!(
                    matches!(
                        decode_request(text.as_bytes()),
                        Err(RequestError::Malformed { .. })
                    ),
                    "arguments {arguments} must be refused"
                );
            }
            let repeated_argument =
                format!(r#"{head},"arguments":{{"path":"/srv/a","path":"/etc/x"}}}}"#);
            assert!(matches!(
                decode_request(repeated_argument.as_bytes()),
                Err(RequestError::Malformed { .. })
            ));
            let repeated_field =
                format!(r#"{head},"applet":"fs.ensure_dir","arguments":{{"path":"/srv/a"}}}}"#);
            assert!(matches!(
                decode_request(repeated_field.as_bytes()),
                Err(RequestError::Malformed { .. })
            ));
            let trailing = format!(r#"{head},"arguments":{{"path":"/srv/a"}}}} {{}}"#);
            assert!(matches!(
                decode_request(trailing.as_bytes()),
                Err(RequestError::Malformed { .. })
            ));
            assert!(matches!(
                decode_request(&[0xff, 0xfe, b'{']),
                Err(RequestError::Malformed { .. })
            ));
        }

        #[test]
        fn request_refuses_bad_applet_names_and_oversize_input() {
            for name in [
                "",
                "FS.ENSURE_DIR",
                "fs ensure",
                "fs/../x",
                "fs.ensure_dir ",
            ] {
                assert!(
                    matches!(
                        ElevatedRequest::new(name, Map::new()),
                        Err(RequestError::BadAppletName { .. })
                    ),
                    "name {name:?} must be refused"
                );
            }
            let oversize = vec![b' '; MAX_REQUEST_BYTES + 1];
            assert!(matches!(
                decode_request(&oversize),
                Err(RequestError::TooLarge { .. })
            ));
        }

        #[test]
        fn every_allow_listed_applet_is_accepted_when_its_paths_are_in_policy() {
            for spec in ALLOWED_APPLETS {
                let mut arguments = Map::new();
                for (name, kind) in spec.params {
                    let value = match kind {
                        Kind::Path => json!(format!("/srv/test/{name}")),
                        Kind::Content => json!("text"),
                        Kind::Mode => json!("0644"),
                        Kind::Owner | Kind::Group => json!("root"),
                        Kind::Other => json!("value"),
                    };
                    arguments.insert((*name).to_string(), value);
                }
                let request = ElevatedRequest::new(spec.name, arguments).expect("well formed");
                let authorized = authorize_elevated(&request, &policy(&["/srv/test/**"]))
                    .unwrap_or_else(|refusal| panic!("{}: {refusal}", spec.name));
                assert_eq!(authorized.applet(), spec.name);
            }
        }

        #[test]
        fn representative_applets_outside_the_list_are_refused_by_name() {
            for applet in [
                "run",
                "llm.ask",
                "http.get",
                "fs.read",
                "exec.which",
                "fs.unlink_checked",
                "fs.transaction",
                "jsonc.set",
                "fs.ensure_file.bak",
                "unknown.applet",
            ] {
                let refusal = outcome(
                    &["/srv/test/**"],
                    applet,
                    json!({"path": "/srv/test/a", "content": "x"}),
                )
                .expect_err("the applet must be refused");
                assert_eq!(
                    refusal,
                    Refusal::AppletNotAllowed {
                        applet: applet.to_string()
                    }
                );
            }
        }

        #[test]
        fn hostile_parent_components_are_refused_even_inside_the_policy() {
            assert_eq!(
                path_reason(&["/etc/**"], "/etc/../root/.ssh/authorized_keys"),
                PathRefusal::ParentComponent
            );
            assert_eq!(
                path_reason(&["/etc/**"], "/etc/sudoers.d/../../root/x"),
                PathRefusal::ParentComponent
            );
            assert_eq!(path_reason(&["/**"], "/a/.."), PathRefusal::ParentComponent);
        }

        #[test]
        fn relative_and_unnormalized_paths_are_refused_not_resolved() {
            let policy_all = &["/**"];
            assert_eq!(
                path_reason(policy_all, "etc/sudoers.d/x"),
                PathRefusal::NotAbsolute
            );
            assert_eq!(path_reason(policy_all, "./x"), PathRefusal::NotAbsolute);
            assert_eq!(path_reason(policy_all, "../x"), PathRefusal::NotAbsolute);
            assert_eq!(path_reason(policy_all, ""), PathRefusal::Empty);
            assert_eq!(
                path_reason(policy_all, "//etc/x"),
                PathRefusal::Unnormalized
            );
            assert_eq!(
                path_reason(policy_all, "/etc/./x"),
                PathRefusal::Unnormalized
            );
            assert_eq!(
                path_reason(policy_all, "/etc/x/"),
                PathRefusal::Unnormalized
            );
            assert_eq!(
                path_reason(policy_all, "/etc//x"),
                PathRefusal::Unnormalized
            );
        }

        #[test]
        fn nul_bytes_are_refused() {
            assert_eq!(
                path_reason(&["/etc/**"], "/etc/sudoers.d/x\0"),
                PathRefusal::NulByte
            );
        }

        #[test]
        fn a_pattern_names_a_whole_directory_and_never_a_prefix_of_a_longer_name() {
            let pattern = &["/etc/sudoers.d/*"];
            assert!(allowed(pattern, "/etc/sudoers.d/omarchy"));
            assert_eq!(
                path_reason(pattern, "/etc/sudoers.d.evil/x"),
                PathRefusal::NotAllowedByPolicy
            );
            assert_eq!(
                path_reason(pattern, "/etc/sudoers.d"),
                PathRefusal::NotAllowedByPolicy
            );
            assert_eq!(
                path_reason(pattern, "/etc/sudoers"),
                PathRefusal::NotAllowedByPolicy
            );
            assert_eq!(
                path_reason(&["/etc/sudoers.d"], "/etc/sudoers.d.evil/x"),
                PathRefusal::NotAllowedByPolicy
            );
        }

        #[test]
        fn a_star_stays_in_one_component_and_double_star_crosses_them() {
            assert!(!allowed(&["/etc/sudoers.d/*"], "/etc/sudoers.d/a/b"));
            assert!(allowed(&["/etc/sudoers.d/**"], "/etc/sudoers.d/a/b/c"));
            assert!(!allowed(&["/etc/sudoers.d/**"], "/etc/sudoers.d"));
            assert!(allowed(&["/etc/**/conf"], "/etc/a/b/conf"));
            assert!(!allowed(&["/etc/**/conf"], "/etc/conf"));
            assert!(allowed(&["/etc/pam.d/su*"], "/etc/pam.d/sudo"));
            assert!(!allowed(&["/etc/pam.d/su*"], "/etc/pam.d/x/sudo"));
            assert!(!allowed(&["/etc/pam.d/su*"], "/etc/pam.d/bsudo"));
        }

        #[test]
        fn an_empty_policy_allows_nothing() {
            assert_eq!(
                path_reason(&[], "/etc/sudoers.d/x"),
                PathRefusal::NotAllowedByPolicy
            );
            assert_eq!(path_reason(&[], "/srv/x"), PathRefusal::NotAllowedByPolicy);
        }

        #[test]
        fn malformed_patterns_are_rejected_when_the_policy_is_built() {
            for bad in [
                "",
                "etc/**",
                "/etc/../x",
                "/etc//x",
                "/etc/**x",
                "/etc/a\0",
                "/etc/./x",
                "/",
            ] {
                assert!(
                    ElevationPolicy::new([bad]).is_err(),
                    "pattern {bad:?} must be refused"
                );
            }
            assert!(ElevationPolicy::new(["/etc/sudoers.d/*"]).is_ok());
        }

        #[test]
        fn path_parameters_must_be_strings_and_undeclared_parameters_are_refused() {
            assert!(matches!(
                outcome(&["/**"], "fs.ensure_dir", json!({"path": ["/etc/x"]})),
                Err(Refusal::NotAString { parameter, .. }) if parameter == "path"
            ));
            assert!(matches!(
                outcome(
                    &["/etc/**"],
                    "fs.ensure_file",
                    json!({"path": "/etc/a", "content": "x", "lock": "/etc/b"}),
                ),
                Err(Refusal::UnknownParameter { parameter, .. }) if parameter == "lock"
            ));
        }

        #[test]
        fn a_second_path_parameter_is_checked_too() {
            assert!(matches!(
                outcome(
                    &["/srv/test/**"],
                    "fs.ensure_symlink",
                    json!({"path": "/srv/test/l", "target": "/etc/../shadow"}),
                ),
                Err(Refusal::Path { parameter, reason: PathRefusal::ParentComponent, .. })
                    if parameter == "target"
            ));
        }

        #[test]
        fn the_table_matches_the_shipped_manifests() {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
            let manifests: Vec<Value> = fs::read_dir(&dir)
                .expect("the manifests directory is readable")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .map(|path| {
                    let text = fs::read_to_string(&path).expect("the manifest is readable");
                    serde_json::from_str(&text).expect("the manifest is JSON")
                })
                .collect();
            for spec in ALLOWED_APPLETS {
                let matching: Vec<&Value> = manifests
                    .iter()
                    .filter(|manifest| manifest["name"] == spec.name)
                    .collect();
                assert_eq!(
                    matching.len(),
                    1,
                    "exactly one manifest must declare {}",
                    spec.name
                );
                let declared = matching[0]["parameters"]
                    .as_object()
                    .expect("the manifest declares parameters");
                let mut declared_names: Vec<&str> = declared.keys().map(String::as_str).collect();
                declared_names.sort_unstable();
                let mut table_names: Vec<&str> =
                    spec.params.iter().map(|(name, _)| *name).collect();
                table_names.sort_unstable();
                assert_eq!(
                    table_names, declared_names,
                    "{} gained or lost a parameter; update ALLOWED_APPLETS",
                    spec.name
                );
                for (name, kind) in spec.params {
                    let manifest_is_path = declared[*name]["type"] == "path";
                    assert_eq!(
                        *kind == Kind::Path,
                        manifest_is_path,
                        "{}.{} kind does not match its manifest type",
                        spec.name,
                        name
                    );
                }
            }
        }

        #[test]
        fn every_allow_listed_applet_is_in_the_applet_dispatch_table() {
            let dispatch = include_str!("applets/mod.rs");
            for spec in ALLOWED_APPLETS {
                assert!(
                    dispatch.contains(&format!("\"{}\" =>", spec.name)),
                    "{} is not dispatched by src/applets/mod.rs",
                    spec.name
                );
            }
        }

        #[test]
        fn the_transport_argv_is_fixed_and_the_request_travels_on_stdin() {
            let authorized = sample_authorized();
            let call = transport_call("/usr/libexec/execute/execute-applet", &authorized)
                .expect("the transport builds");
            let argv: Vec<String> = call
                .sudo
                .argv
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                argv,
                [
                    "sudo",
                    "-n",
                    "--",
                    "/usr/libexec/execute/execute-applet",
                    "elevated-applet"
                ]
            );
            assert_eq!(call.sudo.env, SUDO_ENV);
            assert_eq!(
                call.stdin,
                encode_request(authorized.request()).expect("encodes")
            );
            assert!(String::from_utf8_lossy(&call.stdin).contains("SECRET-LINE"));
            assert!(argv.iter().all(|part| {
                !part.contains("SECRET")
                    && !part.contains("/srv/test/a")
                    && !part.contains("fs.ensure_file")
            }));
        }

        #[test]
        fn a_relative_or_hostile_helper_path_is_never_built() {
            let authorized = sample_authorized();
            for helper in [
                "execute-applet",
                "./execute-applet",
                "/usr/../bin/execute-applet",
                "",
            ] {
                assert!(
                    matches!(
                        transport_call(helper, &authorized),
                        Err(TransportError::HelperPath(_))
                    ),
                    "helper {helper:?} must be refused"
                );
            }
        }

        #[test]
        fn the_audit_record_names_paths_and_digests_but_never_content() {
            let content = "auth required pam_unix.so PAM-SECRET-7\n";
            let req = request(
                "fs.write_atomic",
                json!({
                    "path": "/etc/pam.d/sudo",
                    "lock": "/etc/pam.d/sudo.lock",
                    "content": content,
                    "mode": 420,
                    "expected_content_hash": "0",
                    "symlink_policy": "refuse",
                    "permissions_policy": "keep",
                    "lock_timeout_ms": 5000
                }),
            );
            let authorized =
                authorize_elevated(&req, &policy(&["/etc/pam.d/**"])).expect("allowed");
            let record = audit_record(&authorized);
            assert_eq!(record.applet, "fs.write_atomic");
            assert!(record
                .paths
                .contains(&("path".to_string(), "/etc/pam.d/sudo".to_string())));
            assert!(record
                .paths
                .contains(&("lock".to_string(), "/etc/pam.d/sudo.lock".to_string())));
            let digest: String = Sha256::digest(content.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(
                record.content_sha256,
                vec![("content".to_string(), digest.clone())]
            );
            assert_eq!(record.mode, Some(json!(420)));
            assert_eq!(record.owner, None);
            assert_eq!(record.group, None);
            let text = record.to_json().to_string();
            assert!(!text.contains("PAM-SECRET-7"));
            assert!(text.contains(&digest));
        }

        #[test]
        fn the_audit_record_carries_owner_and_group_when_a_spec_declares_them() {
            static SPEC: AppletSpec = AppletSpec {
                name: "fs.ensure_dir",
                params: &[
                    ("path", Kind::Path),
                    ("owner", Kind::Owner),
                    ("group", Kind::Group),
                ],
            };
            let authorized = Authorized {
                request: request(
                    "fs.ensure_dir",
                    json!({"path": "/srv/test/d", "owner": "root", "group": "adm"}),
                ),
                spec: &SPEC,
            };
            let record = audit_record(&authorized);
            assert_eq!(record.owner, Some(json!("root")));
            assert_eq!(record.group, Some(json!("adm")));
            assert_eq!(record.to_json()["group"], json!("adm"));
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
