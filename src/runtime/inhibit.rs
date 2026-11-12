//! Argument vector for `systemd-inhibit`, the logind inhibitor launcher. This module only
//! builds the vector. It starts no process and is not wired into the runtime yet.
//!
//! logind keeps an inhibitor lock for as long as the command that `systemd-inhibit` runs
//! is alive, so wrapping a command with these arguments holds the lock for that command's
//! life. The kernel drops the lock when the command exits or is killed.
use anyhow::{bail, Result};

/// The `--what=` values systemd-inhibit accepts. Anything else is rejected before a
/// process is started.
pub(crate) const INHIBIT_WHAT: &[&str] = &[
    "shutdown",
    "sleep",
    "idle",
    "handle-power-key",
    "handle-suspend-key",
    "handle-hibernate-key",
    "handle-lid-switch",
];

/// The `--mode=` values. `block` refuses the operation, `delay` only postpones it briefly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InhibitMode {
    Block,
    Delay,
}

impl InhibitMode {
    fn as_str(self) -> &'static str {
        match self {
            InhibitMode::Block => "block",
            InhibitMode::Delay => "delay",
        }
    }
}

/// One inhibitor hold: what it blocks, who holds it, why, how, and the command it lives as.
pub(crate) struct InhibitRequest<'a> {
    pub what: &'a [&'a str],
    pub who: &'a str,
    pub why: &'a str,
    pub mode: InhibitMode,
    /// The program and its arguments. The first element is the program.
    pub command: &'a [&'a str],
}

/// Builds the arguments for `systemd-inhibit` (the program name itself is not included):
/// `--what=a:b --who=.. --why=.. --mode=.. -- PROGRAM ARGS...`.
///
/// Errors when `what` is empty, when a `what` value is not in [`INHIBIT_WHAT`], or when
/// the command is empty or its program name is empty.
pub(crate) fn systemd_inhibit_args(req: &InhibitRequest<'_>) -> Result<Vec<String>> {
    if req.what.is_empty() {
        bail!("inhibitor needs at least one `what` value");
    }
    for what in req.what {
        if !INHIBIT_WHAT.contains(what) {
            bail!(
                "unknown inhibitor `what` value `{what}`; expected one of {}",
                INHIBIT_WHAT.join(", ")
            );
        }
    }
    if req.command.first().is_none_or(|program| program.is_empty()) {
        bail!("inhibitor needs a command to hold the lock for");
    }

    let mut args = vec![
        format!("--what={}", req.what.join(":")),
        format!("--who={}", req.who),
        format!("--why={}", req.why),
        format!("--mode={}", req.mode.as_str()),
        "--".to_string(),
    ];
    args.extend(req.command.iter().map(|arg| arg.to_string()));
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(what: &'a [&'a str], command: &'a [&'a str]) -> InhibitRequest<'a> {
        InhibitRequest {
            what,
            who: "execute",
            why: "update running",
            mode: InhibitMode::Block,
            command,
        }
    }

    #[test]
    fn builds_block_vector_with_command_after_separator() {
        let req = request(&["sleep", "idle"], &["apt-get", "full-upgrade"]);
        let args = systemd_inhibit_args(&req).unwrap();
        assert_eq!(
            args,
            vec![
                "--what=sleep:idle",
                "--who=execute",
                "--why=update running",
                "--mode=block",
                "--",
                "apt-get",
                "full-upgrade",
            ]
        );
    }

    #[test]
    fn delay_mode_and_lid_switch_are_rendered() {
        let what = ["handle-lid-switch"];
        let command = ["sleep", "5"];
        let req = InhibitRequest {
            what: &what,
            who: "omarchy",
            why: "lid stays open",
            mode: InhibitMode::Delay,
            command: &command,
        };
        let args = systemd_inhibit_args(&req).unwrap();
        assert_eq!(args[0], "--what=handle-lid-switch");
        assert_eq!(args[3], "--mode=delay");
        assert_eq!(&args[4..], ["--", "sleep", "5"]);
    }

    #[test]
    fn every_accepted_what_value_is_allowed() {
        for what in INHIBIT_WHAT {
            let req = request(std::slice::from_ref(what), &["true"]);
            assert!(
                systemd_inhibit_args(&req).is_ok(),
                "`{what}` should be accepted"
            );
        }
    }

    #[test]
    fn unknown_what_value_is_rejected() {
        for bad in ["suspend", "sleep:idle", "", "Sleep", "lid"] {
            let what = [bad];
            let req = request(&what, &["true"]);
            let err = systemd_inhibit_args(&req).unwrap_err().to_string();
            assert!(err.contains("unknown inhibitor"), "{bad}: {err}");
        }
    }

    #[test]
    fn one_bad_value_in_a_list_rejects_the_whole_request() {
        let req = request(&["sleep", "reboot"], &["true"]);
        assert!(systemd_inhibit_args(&req).is_err());
    }

    #[test]
    fn empty_what_list_is_rejected() {
        let req = request(&[], &["true"]);
        assert!(systemd_inhibit_args(&req).is_err());
    }

    #[test]
    fn empty_command_is_rejected() {
        let req = request(&["sleep"], &[]);
        assert!(systemd_inhibit_args(&req).is_err());
    }

    #[test]
    fn empty_program_name_is_rejected() {
        let req = request(&["sleep"], &[""]);
        assert!(systemd_inhibit_args(&req).is_err());
    }

    #[test]
    fn command_arguments_pass_through_unchanged() {
        let command = ["rsync", "--delete", "--what=x", "a b", ""];
        let req = request(&["shutdown"], &command);
        let args = systemd_inhibit_args(&req).unwrap();
        assert_eq!(&args[5..], ["rsync", "--delete", "--what=x", "a b", ""]);
    }
}
