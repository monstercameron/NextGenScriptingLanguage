//! Theme switch example: render into a new theme directory, swap the current-theme
//! link, reload, and roll the link back when the reload fails. The portable test
//! checks the program; the Unix tests create real links and a fake `hyprctl`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/desktop/theme-switch.program")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn switch_inputs<'a>(
    theme_name: &'a str,
    theme_root: &'a str,
    current_link: &'a str,
    templates: &'a str,
    values: &'a str,
) -> [(&'a str, &'a str); 5] {
    [
        ("theme_name", theme_name),
        ("theme_root", theme_root),
        ("current_link", current_link),
        ("templates", templates),
        ("values_json", values),
    ]
}

#[test]
fn theme_switch_program_checks_with_its_declared_inputs() {
    let directory = TempDir::new().unwrap();
    let inputs = switch_inputs(
        "dark",
        "/themes",
        "/current/theme",
        "/templates/*.tpl",
        "{}",
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory.path())
        .arg("check")
        .arg(example())
        .arg("--manifest-dir")
        .arg(manifests());
    for (name, value) in inputs {
        command.arg("--input").arg(format!("{name}={value}"));
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    const FAKE_HYPRCTL: &str = r#"#!/bin/sh
printf '%s\n' "hyprctl $*" >> "$FAKE_LOG"
exit "${FAKE_EXIT:-0}"
"#;

    struct Fixture {
        root: TempDir,
        bin: TempDir,
        log: PathBuf,
    }

    impl Fixture {
        /// Templates for a dark theme, a theme root, and a current-theme link that
        /// points at an existing "old" theme.
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let bin = TempDir::new().unwrap();
            let hyprctl = bin.path().join("hyprctl");
            fs::write(&hyprctl, FAKE_HYPRCTL).unwrap();
            fs::set_permissions(&hyprctl, fs::Permissions::from_mode(0o755)).unwrap();
            let templates = root.path().join("templates");
            fs::create_dir_all(&templates).unwrap();
            fs::write(
                templates.join("alacritty.toml.tpl"),
                "background = \"{{ background }}\"\n",
            )
            .unwrap();
            fs::write(
                templates.join("mako.conf.tpl"),
                "foreground={{ foreground }}\n",
            )
            .unwrap();
            let themes = root.path().join("themes");
            fs::create_dir_all(themes.join("old")).unwrap();
            fs::create_dir_all(root.path().join("current")).unwrap();
            symlink(themes.join("old"), root.path().join("current/theme")).unwrap();
            let log = root.path().join("calls.log");
            Fixture { root, bin, log }
        }

        fn themes(&self) -> PathBuf {
            self.root.path().join("themes")
        }

        fn link(&self) -> PathBuf {
            self.root.path().join("current/theme")
        }

        fn run(&self, values: &str, reload_exit: &str) -> Output {
            let templates = self.root.path().join("templates/*.tpl");
            let themes = self.themes();
            let link = self.link();
            let inputs = switch_inputs(
                "dark",
                themes.to_str().unwrap(),
                link.to_str().unwrap(),
                templates.to_str().unwrap(),
                values,
            );
            let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
            command
                .current_dir(self.root.path())
                .arg(example())
                .arg("--manifest-dir")
                .arg(manifests());
            for (name, value) in inputs {
                command.arg("--input").arg(format!("{name}={value}"));
            }
            let path = format!(
                "{}:{}",
                self.bin.path().display(),
                std::env::var("PATH").unwrap_or_default()
            );
            command
                .env("PATH", path)
                .env("FAKE_LOG", &self.log)
                .env("FAKE_EXIT", reload_exit);
            command.output().unwrap()
        }
    }

    const VALUES: &str = r##"{"background":"#1e1e2e","foreground":"#cdd6f4"}"##;

    #[test]
    fn successful_switch_renders_swaps_the_link_and_reloads() {
        let fixture = Fixture::new();
        let output = fixture.run(VALUES, "0");
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(
            stdout(&output).contains("theme dark active"),
            "{}",
            stdout(&output)
        );

        let dark = fixture.themes().join("dark");
        assert_eq!(
            fs::read_to_string(dark.join("alacritty.toml")).unwrap(),
            "background = \"#1e1e2e\"\n"
        );
        assert_eq!(
            fs::read_to_string(dark.join("mako.conf")).unwrap(),
            "foreground=#cdd6f4\n"
        );
        assert_eq!(fs::read_link(fixture.link()).unwrap(), dark);
        assert_eq!(
            fs::read_to_string(&fixture.log).unwrap(),
            "hyprctl reload\n"
        );
    }

    #[test]
    fn failed_reload_restores_the_previous_theme_and_fails() {
        let fixture = Fixture::new();
        let output = fixture.run(VALUES, "1");
        // The rollback message is printed before the program fails, and the final
        // `false` replaces it as the pipeline value, so only the status and the link
        // are asserted here.
        assert!(!output.status.success(), "{}", stdout(&output));
        assert_eq!(
            fs::read_link(fixture.link()).unwrap(),
            fixture.themes().join("old")
        );
    }

    #[test]
    fn render_failure_stops_before_the_link_changes() {
        let fixture = Fixture::new();
        // The `foreground` key is missing, so the second template cannot render.
        let output = fixture.run(r##"{"background":"#1e1e2e"}"##, "0");
        assert!(!output.status.success());
        assert_eq!(
            fs::read_link(fixture.link()).unwrap(),
            fixture.themes().join("old")
        );
        assert!(!fixture.log.exists(), "reload ran after a failed render");
    }

    #[test]
    fn empty_template_match_is_not_reported_as_an_error() {
        // Documented limit: a glob that matches nothing renders no files, and the
        // switch proceeds. The example does not count matches.
        let fixture = Fixture::new();
        let templates = fixture.root.path().join("nothing/*.tpl");
        let themes = fixture.themes();
        let link = fixture.link();
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(fixture.root.path())
            .arg(example())
            .arg("--manifest-dir")
            .arg(manifests());
        for (name, value) in switch_inputs(
            "empty",
            themes.to_str().unwrap(),
            link.to_str().unwrap(),
            templates.to_str().unwrap(),
            "{}",
        ) {
            command.arg("--input").arg(format!("{name}={value}"));
        }
        let path = format!(
            "{}:{}",
            fixture.bin.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .env("PATH", path)
            .env("FAKE_LOG", &fixture.log)
            .env("FAKE_EXIT", "0");
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(themes.join("empty").is_dir());
    }
}
