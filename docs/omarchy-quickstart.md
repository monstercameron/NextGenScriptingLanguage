# Omarchy quickstart

This page walks an Omarchy user through a small Execute workflow that touches the
desktop: a program that toggles a Hyprland state, a test that runs it against a fake
`hyprctl`, a theme hook, and a monitor listener that runs as a user service. Each step
names the command or file in this repository that it depends on. Where the repository
does not yet provide a step, the step says so under **Not yet available**.

Nothing here has been run on a live Omarchy session. The Omarchy command ports are
checked against fake backends only (see [the Omarchy ports](../examples/omarchy-ports/README.md)).

## 1. Install

The Arch Linux packaging is in the repository as two PKGBUILDs,
[`packaging/aur/execute`](../packaging/aur/execute/PKGBUILD) for a source build and
`packaging/aur/execute-bin` for the release archive. The packages are not yet
submitted to the AUR, so there is no `yay` or `paru` package to install today. See
[releases](releases.md) for the packaging status.

Build from source. Rust stable 1.89.0 or newer is required:

```sh
git clone https://github.com/monstercameron/NextGenScriptingLanguage.git
cd NextGenScriptingLanguage
cargo build --locked --release
export PATH="$PWD/target/release:$PATH"
execute --version
```

The latest published release is `0.9.0-rc.4`. This guide was written against the current
source tree, and the [README](../README.md#scope-and-current-limits) states that
everything added since rc.4 requires a source build, so use the source build above.

## 2. Write a toggle

Create `toggle-floating.program`. It toggles floating mode on the focused window
through Hyprland's `dispatch` applet:

```text
# Toggle floating mode on the focused window.
run hypr.dispatch with arguments ["togglefloating"]
```

`hypr.dispatch` accepts only a fixed set of non-spawning dispatchers, and
`togglefloating` is one of them. The full list is in the
[applet contracts](applet-contracts.md) and in
[`manifests/core.hypr_dispatch.json`](../manifests/core.hypr_dispatch.json).
A dispatcher that is not on that list is refused before `hyprctl` starts.

To read state before deciding, use an applet that returns JSON. The desktop example
reads the active window:

```text
run hypr.activewindow as active
```

Dispatchers that are not on the allowlist, such as `keyword` or `eval`, can only be
called through a plain native command, `run "hyprctl" with arguments [...]`. The
[workspace layout toggle](../examples/omarchy-rewrites/07-workspace-layout-toggle.program)
uses that form. Such calls are not restricted by the allowlist, so review them as
ordinary commands.

Review the program before it runs:

```sh
execute check toggle-floating.program
execute explain toggle-floating.program
execute run toggle-floating.program --what-if
```

`check` validates without running anything. `explain` prints the plan and the argv of
each step. `--what-if` prints the plan and a fingerprint and runs nothing. The
fingerprint can be passed to `--confirm` on a later run.

## 3. Bind it in Hyprland

**Not yet available.** This repository does not contain a verified Hyprland key binding
or a launcher wrapper for Omarchy sessions. Bind the program's path to a key in your
own Hyprland configuration, using the binding syntax of your Hyprland version. Running
the program directly from a terminal with `execute toggle-floating.program` is the
tested path.

## 4. Test it with a fake hyprctl

`execute test` runs each `*.test.program` in its own working directory. Fake tools
placed in a `fixtures/bin` directory beside the test are first on `PATH` for that test.
The layout, the optional JSON sidecar and the expectation fields are described in
[testing programs](guide.md#testing-programs). The demo fixture
[`examples/demo/fixtures/bin/hyprctl`](../examples/demo/fixtures/bin/hyprctl) shows a
fake `hyprctl` that is used with the theme demo.

Put `toggle-floating.program` and a `toggle-floating.test.program` in `tests/`, and the
fake `hyprctl` in `tests/fixtures/bin/`. The test program calls the toggle and
the sidecar lists the expected status and stdout. Then run:

```sh
execute test tests/
```

The test runs the real program with the fake on `PATH`, so no compositor is touched.
On Unix a fake tool can be a shell script. On Windows, process creation finds only
`.exe` files on `PATH`, so fake tools there must be compiled programs.

## 5. Run a hook when the theme changes

**Partly available.** `execute shim` writes a Bash stub that runs one `.program` through
`execute run`. This repository does not contain a `theme-set.d` hook directory, and the hook
installer in `src/service_deployment.rs` has no command-line entry point. You create the stub
in the hook directory yourself. The directory must already exist:

```sh
execute shim "$PWD/theme-refresh.program" --name theme-refresh.sh --out ~/.config/omarchy/hooks/theme-set.d
```

Read the [entry-point compatibility](#entry-point-compatibility) table before relying on the
stub. The reviewable example for a theme change is
[`examples/demo/theme-refresh.program`](../examples/demo/theme-refresh.program). It
renders templates, swaps the current-theme link and runs `hyprctl reload`. Its effects
can be reviewed with `execute explain` before it runs, and an interrupted reload is
retried only with an explicit `--retry-uncertain`, as described in the
[practical guide](guide.md#cache-work-repair-failures-resume-progress).

## 6. Run a monitor listener as a user service

The desktop example
[`examples/desktop/hyprland-events.program`](../examples/desktop/hyprland-events.program)
is a listener on Hyprland's event socket. The socket path is built from the session
environment and passed as an input:

```sh
execute examples/desktop/hyprland-events.program \
  --input socket_path="$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock"
```

To run it as a user service, generate the unit and review it. The command writes the
definition to standard output and installs nothing:

```sh
execute service generate systemd examples/desktop/hyprland-events.program \
  --name hyprland-events \
  --working-dir "$PWD" \
  --manifest-dir "$PWD/manifests" \
  --input socket_path="$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock"
```

Move the reviewed output into your user service configuration yourself, then start it
with your service manager. The generated unit restarts on failure, and it does not
allocate a terminal. See
[generate a reviewable service definition](guide.md#generate-a-reviewable-service-definition)
for the exact unit settings.

## Entry-point compatibility

Omarchy runs its scripts in several ways. This table says what a `.program` gets when
`execute shim` replaces the file at each entry point, and what the repository shows.

The upstream Omarchy scripts are not in this repository. The file
[`docs/omarchy-inventory.json`](omarchy-inventory.json) covers only the direct files in `bin/`.
It says nothing about hooks, migrations or install leaves. So the caller behaviour in the
table comes from the issue that asked for the shim (#400), unless a row names a file in this
repository. No test here runs a stub under the pinned upstream scripts, so every row is
unverified against the real callers. Hook names other than `battery-low` appear only in the
issue text.

| Entry point | How the caller runs the file | Shim outcome | Status |
| --- | --- | --- | --- |
| Hook `battery-low` (`hooks/battery-low.d/`) | `bin/omarchy-battery-low` runs battery-low hooks. The hook runner runs each file with `bash FILE` (per #400). | The stub runs under `bash` and `exec`s `execute run`. | works |
| Hook `font-set` (`hooks/font-set.d/`) | `bash FILE` (per #400). No caller in this repository. | The stub runs under `bash` and `exec`s `execute run`. | works |
| Hook `post-boot` (`hooks/post-boot.d/`) | `bash FILE` (per #400). No caller in this repository. | The stub runs under `bash` and `exec`s `execute run`. | works |
| Hook `post-update` (`hooks/post-update.d/`) | `bash FILE` (per #400). No caller in this repository. | The stub runs under `bash` and `exec`s `execute run`. | works |
| Hook `pre-refresh-pacman` (`hooks/pre-refresh-pacman.d/`) | `bash FILE` (per #400). No caller in this repository. | The stub runs under `bash` and `exec`s `execute run`. | works |
| Hook `theme-set` (`hooks/theme-set.d/`) | `bash FILE` (per #400). No caller in this repository. | The stub runs under `bash` and `exec`s `execute run`. | works |
| Migration `migrations/NNN.sh` | `bash -euo pipefail FILE` (per #400). | `execute shim --name NNN.sh` gives a file with that name. The `-euo pipefail` options apply to the stub's Bash, not to the program. | works |
| `bin/` scripts that run a file with `bash "$..."` (19 per #400, not recounted here) | `bash FILE` on a path inside the Omarchy tree. | A stub at that path is run by `bash`. | works |
| Upstream test files that run `bash "$ROOT/bin/omarchy-..."` (32 per #400, not recounted here) | `bash FILE`. | Same as above. Output must still match the upstream test's expectations, which is not checked here. | works |
| `bin/` scripts that `source` another file | Sourced in the caller's shell. | The stub runs in a child process. Variables, functions and directory changes made by the file do not reach the caller. | cannot work |
| Install leaves under `install/` | Sourced by `run_logged`. No shebang, by design (per #400). | Same as above. | cannot work |
| Any caller that runs the file through its shebang | Kernel runs `#!/usr/bin/env -S execute --` directly. Linux only, and not tested here. | No stub needed. | works |
| `omarchy-hook` and `omarchy-migrate` | Upstream runners use `bash FILE`. | Upstream would have to run files by shebang, or through `execute`, for the stub to be unnecessary. | needs upstream change |
| Install leaves and `source` callers | The caller would have to run each file as a process instead of sourcing it. | Not possible with a stub. | needs upstream change |

Behaviour that applies to every row:

- The stub is the file `execute shim` writes (see `src/cli/shim.rs`). It contains
  `exec execute run '<absolute path>' -- "$@"`. `exec` replaces the stub's process, so the
  exit status is the program's.
- Arguments reach the program only through a declared input, such as
  `accept arguments as list of text from arguments`. A program with no such declaration
  refuses any argument (see `parse_cli_arguments` in `src/inputs.rs`).
- The stub calls `execute` by name from `PATH`. The caller's environment must have
  `execute` on `PATH`. A hook or migration run from a service with a minimal environment may not.
- The stub hard-codes the program's absolute path. If the program moves, regenerate the stub.
- The stub does not pass `--confirm`, so it does not check a reviewed fingerprint. Review the
  program with `execute explain` and `execute run FILE --what-if` before you generate the stub.

## Not yet available

- **Installing from the AUR.** The PKGBUILDs exist, but the packages are not submitted.
- **A verified Hyprland key binding** and an Omarchy `uwsm-app` launch line.
- **A command-line hook installer.** `src/service_deployment.rs` renders and installs hook
  shims with a fingerprint check, but no command calls it. Use `execute shim` instead, which
  has no fingerprint check.
- **A theme hook directory** (`theme-set.d`). The directory belongs to the Omarchy
  installation under `~/.config/omarchy/hooks/`. This repository does not contain one.
- **A desktop-VM docs test.** The steps in this page are not run in CI. The
  [desktop acceptance workflow](../.github/workflows/desktop-live-acceptance.yml) is a
  manually started run on a disposable Arch machine and does not execute this page.
- **A timed new-user trial.** No trial has been recorded, so the twenty-minute target
  is not measured.

The remaining production gates, including the certified Omarchy desktop run, are in
[production readiness](production-readiness.md).
