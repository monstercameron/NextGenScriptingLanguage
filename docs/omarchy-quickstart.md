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

**Not yet available.** This repository does not contain a `theme-set.d` hook directory
or a shim that calls hooks. The reviewable example for a theme change is
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

## Not yet available

- **Installing from the AUR.** The PKGBUILDs exist, but the packages are not submitted.
- **A verified Hyprland key binding** and an Omarchy `uwsm-app` launch line.
- **A theme hook directory** (`theme-set.d`) and the hook shim that would call it.
- **A desktop-VM docs test.** The steps in this page are not run in CI. The
  [desktop acceptance workflow](../.github/workflows/desktop-live-acceptance.yml) is a
  manually started run on a disposable Arch machine and does not execute this page.
- **A timed new-user trial.** No trial has been recorded, so the twenty-minute target
  is not measured.

The remaining production gates, including the certified Omarchy desktop run, are in
[production readiness](production-readiness.md).
