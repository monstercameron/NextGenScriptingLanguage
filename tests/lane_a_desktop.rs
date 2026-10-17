//! Desktop manifests: catalog validity, argv shapes checked with `execute explain`,
//! input validation with `execute check`, and typed outcomes through fake tools.
//! The explain and check tests are portable and never run the tools. The fake-tool
//! tests are Unix-only and put owned shell scripts first on PATH, so no real
//! notifier, systemd, pacman, Hyprland or D-Bus is needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const MANIFEST_NAMES: &[&str] = &[
    "notify.send",
    "notify.dismiss",
    "notify.restore",
    "osd.volume",
    "osd.brightness",
    "osd.caps",
    "menu.walker",
    "menu.fuzzel",
    "menu.gum_choose",
    "menu.gum_input",
    "menu.gum_confirm",
    "systemd.user.is_active",
    "systemd.user.start",
    "systemd.user.stop",
    "systemd.user.restart",
    "systemd.user.enable",
    "systemd.user.disable",
    "dbus.monitor_user",
    "dbus.monitor_system",
    "package.query",
    "package.install",
    "package.install_aur",
    "package.check_updates",
    "hypr.clients",
    "hypr.monitors",
    "hypr.workspaces",
    "hypr.activewindow",
    "hypr.dispatch",
    "hypr.socket2",
];

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/desktop")
}

fn program(directory: &TempDir, source: &str) -> PathBuf {
    let path = directory.path().join("workflow.program");
    fs::write(&path, source).unwrap();
    path
}

fn execute_command(
    directory: &Path,
    subcommand: Option<&str>,
    file: &Path,
    inputs: &[(&str, &str)],
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.current_dir(directory);
    if let Some(subcommand) = subcommand {
        command.arg(subcommand);
    }
    command.arg(file).arg("--manifest-dir").arg(manifests());
    for (name, value) in inputs {
        command.arg("--input").arg(format!("{name}={value}"));
    }
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The explain line `Start: "<tool>" argv=[...]` for one literal argv.
fn start_line(tool: &str, argv: &[&str]) -> String {
    format!(
        "Start: \"{tool}\" argv={}",
        serde_json::to_string(argv).unwrap()
    )
}

fn explain_source(source: &str, inputs: &[(&str, &str)]) -> String {
    let directory = tempfile::tempdir().unwrap();
    let file = program(&directory, source);
    let output = execute_command(directory.path(), Some("explain"), &file, inputs)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output)
}

fn check_source(source: &str, inputs: &[(&str, &str)]) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let file = program(&directory, source);
    execute_command(directory.path(), Some("check"), &file, inputs)
        .output()
        .unwrap()
}

#[test]
fn every_desktop_manifest_is_in_the_catalog() {
    for name in MANIFEST_NAMES {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("applets")
            .arg("inspect")
            .arg(name)
            .arg("--manifest-dir")
            .arg(manifests())
            .output()
            .unwrap();
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        assert!(stdout(&output).contains(name), "{name}");
    }
}

#[test]
fn notification_argv_shapes_are_explicit() {
    let send = explain_source(
        "run notify.send with summary \"Hi\" and body \"there\" and urgency \"low\" and expire_ms 3000 and action_1 \"open=Open\"\n",
        &[],
    );
    assert!(
        send.contains(&start_line(
            "notify-send",
            &[
                "--urgency",
                "low",
                "--expire-time",
                "3000",
                "--wait",
                "--action",
                "open=Open",
                "--",
                "Hi",
                "there"
            ],
        )),
        "{send}"
    );

    let minimal = explain_source("run notify.send with summary \"Only\"\n", &[]);
    assert!(
        minimal.contains(&start_line("notify-send", &["--", "Only"])),
        "{minimal}"
    );

    let dismiss = explain_source("run notify.dismiss with notification 7\n", &[]);
    assert!(
        dismiss.contains(&start_line("makoctl", &["dismiss", "-n", "7"])),
        "{dismiss}"
    );

    let restore = explain_source("run notify.restore\n", &[]);
    assert!(
        restore.contains(&start_line("makoctl", &["restore"])),
        "{restore}"
    );

    let volume = explain_source("run osd.volume with action \"raise\"\n", &[]);
    assert!(
        volume.contains(&start_line("swayosd-client", &["--output-volume", "raise"])),
        "{volume}"
    );

    let brightness = explain_source("run osd.brightness with action \"lower\"\n", &[]);
    assert!(
        brightness.contains(&start_line("swayosd-client", &["--brightness", "lower"])),
        "{brightness}"
    );

    let caps = explain_source("run osd.caps\n", &[]);
    assert!(
        caps.contains(&start_line("swayosd-client", &["--caps-lock"])),
        "{caps}"
    );
}

#[test]
fn menu_argv_shapes_are_explicit() {
    let walker = explain_source("run menu.walker with flags [\"--width\", \"40\"]\n", &[]);
    assert!(
        walker.contains(&start_line("walker", &["--dmenu", "--width", "40"])),
        "{walker}"
    );

    let fuzzel = explain_source("run menu.fuzzel with prompt \"Pick> \"\n", &[]);
    assert!(
        fuzzel.contains(&start_line("fuzzel", &["--dmenu", "--prompt", "Pick> "])),
        "{fuzzel}"
    );

    let choose = explain_source(
        "run menu.gum_choose with items [\"a\", \"b\"] and header \"Pick\"\n",
        &[],
    );
    assert!(
        choose.contains(&start_line(
            "gum",
            &["choose", "--header", "Pick", "--", "a", "b"]
        )),
        "{choose}"
    );

    let input = explain_source("run menu.gum_input with prompt \"Name> \"\n", &[]);
    assert!(
        input.contains(&start_line("gum", &["input", "--prompt", "Name> "])),
        "{input}"
    );

    let confirm = explain_source("run menu.gum_confirm with prompt \"Proceed?\"\n", &[]);
    assert!(
        confirm.contains(&start_line("gum", &["confirm", "--", "Proceed?"])),
        "{confirm}"
    );
}

#[test]
fn systemd_and_dbus_argv_shapes_are_explicit() {
    let active = explain_source(
        "run systemd.user.is_active with unit \"app.service\"\n",
        &[],
    );
    assert!(
        active.contains(&start_line(
            "systemctl",
            &["--user", "is-active", "--", "app.service"]
        )),
        "{active}"
    );

    for verb in ["start", "stop", "restart", "enable", "disable"] {
        let source = format!("run systemd.user.{verb} with unit \"app.service\"\n");
        let shape = explain_source(&source, &[]);
        assert!(
            shape.contains(&start_line(
                "systemctl",
                &["--user", verb, "--", "app.service"]
            )),
            "{verb}: {shape}"
        );
    }

    let monitor = explain_source("run dbus.monitor_user with match \"type='signal'\"\n", &[]);
    assert!(
        monitor.contains(&start_line(
            "busctl",
            &[
                "--user",
                "monitor",
                "--json=short",
                "--match",
                "type='signal'"
            ],
        )),
        "{monitor}"
    );

    let system = explain_source("run dbus.monitor_system\n", &[]);
    assert!(
        system.contains(&start_line(
            "busctl",
            &["--system", "monitor", "--json=short"]
        )),
        "{system}"
    );
}

#[test]
fn package_argv_shapes_are_explicit() {
    let query = explain_source("run package.query with package \"vim\"\n", &[]);
    assert!(
        query.contains(&start_line("pacman", &["-Qi", "--", "vim"])),
        "{query}"
    );

    let install = explain_source(
        "run package.install with packages [\"vim\", \"git\"]\n",
        &[],
    );
    assert!(
        install.contains(&start_line(
            "pacman",
            &["-S", "--needed", "--noconfirm", "--", "vim", "git"],
        )),
        "{install}"
    );

    let aur = explain_source(
        "run package.install_aur with packages [\"paru-bin\"]\n",
        &[],
    );
    assert!(
        aur.contains(&start_line("yay", &["-S", "--needed", "--", "paru-bin"])),
        "{aur}"
    );

    let updates = explain_source("run package.check_updates\n", &[]);
    assert!(
        updates.contains(&start_line("checkupdates", &[])),
        "{updates}"
    );
}

#[test]
fn hyprland_argv_shapes_and_socket_address_are_explicit() {
    for (name, sub) in [
        ("clients", "clients"),
        ("monitors", "monitors"),
        ("workspaces", "workspaces"),
    ] {
        let source = format!("run hypr.{name}\n");
        let shape = explain_source(&source, &[]);
        assert!(
            shape.contains(&start_line("hyprctl", &["-j", sub])),
            "{name}: {shape}"
        );
    }
    let active = explain_source("run hypr.activewindow\n", &[]);
    assert!(
        active.contains(&start_line("hyprctl", &["-j", "activewindow"])),
        "{active}"
    );

    let dispatch = explain_source(
        "run hypr.dispatch with arguments [\"exec\", \"kitty\"]\n",
        &[],
    );
    assert!(
        dispatch.contains(&start_line("hyprctl", &["dispatch", "exec", "kitty"])),
        "{dispatch}"
    );

    let socket = explain_source(
        "run hypr.socket2 with address \"UNIX-CONNECT:/run/user/1000/hypr/sig/.socket2.sock\"\n",
        &[],
    );
    assert!(
        socket.contains(&start_line(
            "socat",
            &[
                "-U",
                "-",
                "UNIX-CONNECT:/run/user/1000/hypr/sig/.socket2.sock"
            ],
        )),
        "{socket}"
    );
}

#[test]
fn invalid_choices_and_missing_inputs_fail_before_any_tool_runs() {
    assert!(!check_source("run osd.volume with action \"bogus\"\n", &[])
        .status
        .success());
    assert!(
        !check_source("run osd.brightness with action \"mute-toggle\"\n", &[])
            .status
            .success()
    );
    assert!(!check_source(
        "run notify.send with summary \"x\" and urgency \"loud\"\n",
        &[]
    )
    .status
    .success());
    assert!(!check_source("run systemd.user.start\n", &[])
        .status
        .success());
    assert!(!check_source(
        "run package.install with packages [\"vim\"] and extra \"no\"\n",
        &[]
    )
    .status
    .success());
}

#[test]
fn desktop_examples_check_with_their_declared_inputs() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("notify-osd.program", &[("summary", "Done")]),
        ("systemd-user.program", &[("unit", "app.service")]),
        ("packages.program", &[("package", "vim")]),
        ("hyprland-clients.program", &[]),
        ("dbus-monitor.program", &[]),
        ("hyprland-events.program", &[("address", "UNIX-CONNECT:/x")]),
    ];
    for (file, inputs) in cases {
        let directory = tempfile::tempdir().unwrap();
        let output = execute_command(
            directory.path(),
            Some("check"),
            &examples().join(file),
            inputs,
        )
        .output()
        .unwrap();
        assert!(output.status.success(), "{file}: {}", stderr(&output));
    }
    let directory = tempfile::tempdir().unwrap();
    let output = execute_command(
        directory.path(),
        Some("check"),
        &examples().join("menu-pick.program"),
        &[],
    )
    .arg("--")
    .arg("a")
    .arg("b")
    .output()
    .unwrap();
    assert!(output.status.success(), "menu-pick: {}", stderr(&output));
}

#[test]
fn example_listeners_name_their_producers() {
    let dbus = fs::read_to_string(examples().join("dbus-monitor.program")).unwrap();
    assert!(dbus.contains("listen to dbus.monitor_user"));
    let hypr = fs::read_to_string(examples().join("hyprland-events.program")).unwrap();
    assert!(hypr.contains("listen to hypr.socket2"));
    assert!(hypr.contains("XDG_RUNTIME_DIR"));
    assert!(hypr.contains("HYPRLAND_INSTANCE_SIGNATURE"));
}

#[cfg(unix)]
mod fake_tools {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake tool that records its name and argv in `$FAKE_LOG`, prints
    /// `$FAKE_STDOUT`, optionally prints `$FAKE_LINES` (for listeners) and exits
    /// with `$FAKE_EXIT`. Tests use one copy per tool name.
    const TEMPLATE: &str = r#"#!/bin/sh
name=$(basename "$0")
line="$name"
for argument in "$@"; do
  line="$line [$argument]"
done
printf '%s\n' "$line" >> "$FAKE_LOG"
if [ -n "$FAKE_STDOUT" ]; then printf '%s' "$FAKE_STDOUT"; fi
exit "${FAKE_EXIT:-0}"
"#;

    pub struct Fakes {
        pub directory: TempDir,
        pub log: PathBuf,
    }

    impl Fakes {
        pub fn new(names: &[&str]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            for name in names {
                let path = directory.path().join(name);
                fs::write(&path, TEMPLATE).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let log = directory.path().join("calls.log");
            Fakes { directory, log }
        }

        pub fn run(&self, source: &str, inputs: &[(&str, &str)], env: &[(&str, &str)]) -> Output {
            self.run_with_args(source, inputs, env, &[])
        }

        /// Run with literal program arguments after `--`, as `accept ... from arguments` reads them.
        pub fn run_with_args(
            &self,
            source: &str,
            inputs: &[(&str, &str)],
            env: &[(&str, &str)],
            arguments: &[&str],
        ) -> Output {
            let workflow = program(&self.directory, source);
            let mut command = execute_command(self.directory.path(), None, &workflow, inputs);
            if !arguments.is_empty() {
                command.arg("--").args(arguments);
            }
            let path = format!(
                "{}:{}",
                self.directory.path().display(),
                std::env::var("PATH").unwrap_or_default()
            );
            command.env("PATH", path).env("FAKE_LOG", &self.log);
            for (name, value) in env {
                command.env(name, value);
            }
            command.output().unwrap()
        }

        pub fn calls(&self) -> String {
            fs::read_to_string(&self.log).unwrap_or_default()
        }
    }
}

#[cfg(unix)]
#[test]
fn systemd_outcomes_map_exit_codes_to_named_states() {
    use fake_tools::Fakes;
    let fakes = Fakes::new(&["systemctl"]);
    let example = fs::read_to_string(examples().join("systemd-user.program")).unwrap();

    let active = fakes.run(
        &example,
        &[("unit", "app.service")],
        &[("FAKE_STDOUT", "active\n")],
    );
    assert!(active.status.success(), "{}", stderr(&active));
    assert_eq!(stdout(&active), "app.service is active\n");

    let inactive = fakes.run(
        &example,
        &[("unit", "app.service")],
        &[("FAKE_EXIT", "3"), ("FAKE_STDOUT", "inactive\n")],
    );
    assert!(inactive.status.success(), "{}", stderr(&inactive));
    assert_eq!(stdout(&inactive), "app.service is inactive\n");

    assert!(fakes
        .calls()
        .contains("systemctl [--user] [is-active] [--] [app.service]"));
}

#[cfg(unix)]
#[test]
fn menu_cancel_is_a_typed_outcome_and_selection_is_text() {
    use fake_tools::Fakes;
    let fakes = Fakes::new(&["gum"]);
    let example = fs::read_to_string(examples().join("menu-pick.program")).unwrap();

    let cancelled = fakes.run_with_args(&example, &[], &[("FAKE_EXIT", "130")], &["a", "b"]);
    assert!(cancelled.status.success(), "{}", stderr(&cancelled));
    assert_eq!(stdout(&cancelled), "no selection (cancelled)\n");

    let selected = fakes.run_with_args(&example, &[], &[("FAKE_STDOUT", "b\n")], &["a", "b"]);
    assert!(selected.status.success(), "{}", stderr(&selected));
    assert_eq!(stdout(&selected), "picked: b\n");
}

#[cfg(unix)]
#[test]
fn package_query_reports_not_installed_and_install_failures_stop_the_program() {
    use fake_tools::Fakes;
    let fakes = Fakes::new(&["pacman"]);
    let example = fs::read_to_string(examples().join("packages.program")).unwrap();

    let missing = fakes.run(&example, &[("package", "vim")], &[("FAKE_EXIT", "1")]);
    assert!(missing.status.success(), "{}", stderr(&missing));
    assert_eq!(stdout(&missing), "vim is not_installed\n");

    let installed = fakes.run(
        &example,
        &[("package", "vim")],
        &[("FAKE_STDOUT", "Name : vim\n")],
    );
    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(stdout(&installed).starts_with("installed: Name : vim"));

    let install = "run package.install with packages [\"vim\"]\n";
    let failed = fakes.run(install, &[], &[("FAKE_EXIT", "1")]);
    assert!(!failed.status.success());
    assert!(fakes
        .calls()
        .contains("pacman [-S] [--needed] [--noconfirm] [--] [vim]"));
}

#[cfg(unix)]
#[test]
fn notification_action_selection_is_read_from_stdout_with_wait() {
    use fake_tools::Fakes;
    let fakes = Fakes::new(&["notify-send", "swayosd-client"]);
    let example = fs::read_to_string(examples().join("notify-osd.program")).unwrap();

    let selected = fakes.run(
        &example,
        &[("summary", "Done")],
        &[("FAKE_STDOUT", "open\n")],
    );
    assert!(selected.status.success(), "{}", stderr(&selected));
    assert!(stdout(&selected).contains("selected: open"));
    let calls = fakes.calls();
    assert!(calls.contains("notify-send [--urgency] [low] [--expire-time] [4000] [--wait] [--action] [open=Open] [--] [Done]"), "{calls}");
    assert!(
        calls.contains("swayosd-client [--output-volume] [raise]"),
        "{calls}"
    );
}

#[cfg(unix)]
#[test]
fn hyprland_focused_window_reads_the_class_from_json() {
    use fake_tools::Fakes;
    let fakes = Fakes::new(&["hyprctl"]);
    let example = fs::read_to_string(examples().join("hyprland-clients.program")).unwrap();
    let output = fakes.run(&example, &[], &[("FAKE_STDOUT", "{\"class\":\"kitty\"}")]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "focused: kitty\n");
    assert!(fakes.calls().contains("hyprctl [-j] [activewindow]"));
}

#[cfg(unix)]
#[test]
fn listeners_forward_each_producer_line_to_stdout() {
    use fake_tools::Fakes;
    let dbus = Fakes::new(&["busctl"]);
    let example = fs::read_to_string(examples().join("dbus-monitor.program")).unwrap();
    let output = dbus.run(&example, &[], &[("FAKE_STDOUT", "{\"type\":\"signal\"}\n")]);
    assert!(
        stdout(&output).contains("{\"type\":\"signal\"}"),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(dbus
        .calls()
        .contains("busctl [--user] [monitor] [--json=short] [--match] [type='signal']"));

    let hypr = Fakes::new(&["socat"]);
    let example = fs::read_to_string(examples().join("hyprland-events.program")).unwrap();
    let output = hypr.run(
        &example,
        &[("address", "UNIX-CONNECT:/x")],
        &[("FAKE_STDOUT", "openwindow>>abc\n")],
    );
    assert!(
        stdout(&output).contains("openwindow>>abc"),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(hypr.calls().contains("socat [-U] [-] [UNIX-CONNECT:/x]"));
}
