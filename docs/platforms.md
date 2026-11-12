# Platform support tiers

This page states which platforms Execute supports at which level, what CI runs
on each, and which features depend on Linux or on a desktop stack. It is a
policy for contributors and users. Where the policy and the current workflows
differ, the difference is listed under [Current state](#current-state) rather
than hidden.

## Tier definitions

**Tier 1: Linux, x86_64 and aarch64.** Target distributions are Arch and
Omarchy, and Ubuntu and Debian. Every feature is in scope, including the
Linux-only families below. Tier 1 is the first priority for fixes and for
release gating: a Tier 1 failure blocks a release. The target is CI that runs
on every push to `main` and every pull request, with release-blocking status.

**Tier 2: macOS.** The core language and tooling are in scope: parsing, the
runtime, the built-in applets and the command-line interface. Platform-specific
features such as the `sandbox-exec` launch path are in scope where they exist.
Desktop integration for Linux is not. The target is CI on merge to `main`. A
Tier 2 failure is fixed before the next release but does not by itself block a
change to Tier 1 code.

**Tier 3: Windows, x64 and ARM64.** The core language and tooling are in scope,
as described in [Native Windows](windows.md). Fixes are best effort. The target
is a nightly CI run rather than a per-change gate. Windows contributors are
expected to keep Tier 1 and Tier 2 code compiling and to test their own
Windows changes.

Support at a tier means the project runs the tests for that tier and fixes
defects reported against it. It does not mean every optional feature is
available on that platform. The tables below say which features are not.

## Linux-only feature families

These feature families depend on Linux services, a Linux desktop or a Linux
service manager. Some are compiled only on Linux; others compile on every
non-wasm target and fail or report unavailability at run time. The second column
says which kind applies, as far as this page has checked the source.

| Family | Dependency | Compile-time or run-time | Evidence |
| --- | --- | --- | --- |
| Linux launch sandbox | bubblewrap | Linux backend, `cfg(target_os = "linux")` | [src/platform/sandbox.rs](../src/platform/sandbox.rs). The module header says other platforms fail before the applet starts. |
| Hyprland | `hyprctl` and the Hyprland IPC socket | Run-time. The `hypr_ipc` module is built on every non-wasm target. | [src/hypr_ipc.rs](../src/hypr_ipc.rs); [Omarchy quickstart](omarchy-quickstart.md). Real desktop acceptance is open. |
| systemd | per-user units and timers | Generation is run-time text output on any host. Installation is not implemented. | [guide](guide.md) section on `service generate systemd` and `systemd-timer`; [audit resolution R15](audit-resolution.md). |
| D-Bus and systemd1 calls | `org.freedesktop.systemd1`, `org.freedesktop.Flatpak`, `.desktop` activation | Run-time. Not checked for compile-time gating here. | [applet contracts](applet-contracts.md), around the systemd and Flatpak entries. |
| Quickshell renderer | Quickshell and QML on a Wayland session | Run-time. Native QML execution is untested. | [audit resolution O01 to O06](audit-resolution.md). |
| GNOME settings | `gsettings` values | Run-time. Not checked for compile-time gating here. | `src/gsettings_value.rs`. |

Two items commonly grouped with Linux are not Linux-only in this repository.
The sandbox has a macOS backend (`sandbox-exec`) as well as Linux. The
`launchd` plist generator produces macOS service files in the same way the
systemd generator produces Linux units.

Generated service units are review-only. The repository does not test their
installation on a live service manager, so no tier currently claims a service
manager is certified.

## What a contributor must test

- A change to a Linux-only family is tested on Linux. A Windows or macOS
  contributor should still confirm the code compiles on their platform when the
  code is compiled there.
- A change to shared language, runtime or applet code is tested on Tier 1 and
  on at least one of Tier 2 or Tier 3 when the change touches platform-specific
  code (process spawning, paths, pipes, sandboxing).
- A change that only edits documentation needs no platform test beyond the
  documentation checks in [Contributing](../CONTRIBUTING.md).

## Current state

The workflows in `.github/workflows/` show the current CI, which differs from
the tier targets above:

- `ci.yml` (Native checks) runs on every push to `main` and on every pull
  request. Its Unix job covers `ubuntu-24.04` and `macos-15`. Its Windows job
  covers `windows-2022` and `windows-11-arm`. Its feature-matrix job covers
  `ubuntu-24.04` and `windows-2022`. This is the same per-change cost for all
  three platforms, which the tier targets would change.
- `flaky.yml` runs nightly on `ubuntu-24.04`, `macos-15` and `windows-2022`.
- `platforms.yml` runs weekly. It covers the minimum supported Rust version,
  Linux arm64 library tests and a static musl build.
- `packages.yml` builds release packages on Linux x86_64 and arm64, macOS
  x86_64 and arm64, and Windows x64 and ARM64.

No CI job is marked release-blocking in these files. The triggers in `ci.yml`
are not yet aligned with the tiers. That change depends on the CI cost work in
issue #441, which is not part of this document.

Real-hardware evidence for the Linux desktop is not available. The Omarchy
desktop, Hyprland and Quickshell sessions have not been run on a native target
session; see [audit resolution](audit-resolution.md) items A11, O04 and O05.
