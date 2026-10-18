//! Privileged steps: a step that can run a command with elevated rights, or that can
//! run another program chosen by its arguments.
//!
//! Classification is by executable name, after its directory and any `.exe` suffix
//! are removed, compared case-insensitively. Both the name written in the program and
//! the name of the executable that would actually run (the canonical resolved path,
//! see `capabilities::check_privileged_launch`) are checked, so a link or a renamed
//! copy of an elevator is still an elevator. A name that is not on a list is not
//! recognised: this is a marker and a policy hook, not a sandbox.
//!
//! The lists are conservative. `deny_privileged` in a capability policy refuses every
//! listed elevator, and it also refuses every wrapper and shell listed here, because
//! their arguments can name a program the program text never mentions. A wrapper can
//! be allowed by name with `allow_wrappers`. Elevators cannot be allowed.

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

fn classify_one(name: &str) -> Privilege {
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
/// the single answer `explain` (text and JSON) uses. It looks at the name only.
pub fn is_privileged_invocation(binary: &str) -> bool {
    match classify(binary) {
        Privilege::Standard => false,
        Privilege::Wrapper => !is_interpreter(binary),
        Privilege::Elevator => true,
    }
}

/// Whether the name is on [`INTERPRETERS`]: a wrapper for `deny_privileged`, but not
/// marked privileged by `explain`.
pub fn is_interpreter(binary: &str) -> bool {
    normalized_name(binary).is_some_and(|name| INTERPRETERS.contains(&name.as_str()))
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
}
