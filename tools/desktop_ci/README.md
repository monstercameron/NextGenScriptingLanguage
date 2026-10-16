# Disposable Arch desktop smoke

This opt-in runner boots one disposable Arch Linux cloud image in QEMU on a
fresh GitHub-hosted Ubuntu 24.04 runner. It installs the official Arch packages
for Hyprland, Quickshell and the tools used by `tools/omarchy_acceptance.py`,
then starts Hyprland from an autologin on local VT1 and runs the existing direct
Quickshell harness. Only after its evidence passes does the guest run the
RuntimeGraphExecutor-to-Quickshell acceptance for both local and remote clocks.
The runner requires both graph button routes to advance the fixture sync count
and both graph processes to exit through normal SIGINT cancellation with their
observed descendants reaped before it writes `READY`. QEMU's serial connection
is retained for boot logs; compositor startup is not tied to the serial console.

The workflow is manual-only (`workflow_dispatch`). It pins the official
Arch-boxes image release and Arch Linux Archive package snapshot in its
inputs. The default pair is image `20261015.611680` and Arch Archive
snapshot `2026/10/14`. The package snapshot may be the image day or the
preceding daily snapshot so the pin is available when the workflow is run.
It records
the upstream image checksum and downloaded image hash, source commit and
bundle/binary hashes, host and guest package versions, repository-database and
package-cache hashes, harness JSON, serial/setup/QEMU logs, a Wayland screenshot
when available, and shutdown status. Failure cleanup captures a guest artifact
tar capped at 32 MiB and up to 800 journal lines capped at 8 MiB before asking
the guest to power off or removing the overlay; collection status and errors
are retained with the host teardown receipt. The guest disk is a throwaway QCOW2
overlay. The pinned Arch cloud image is 2 GiB virtual size, while the overlay
exposes an 8 GiB virtual disk. This disk capacity is separate from the 5 GiB
host-side per-file write limit; qcow2 remains sparse. NoCloud user data requests
cloud-init growpart and root-filesystem resize. Before package installation,
the guest waits for cloud-init, records growpart, lsblk and df evidence, and
requires at least 4 GiB total and 2 GiB available on root. The job uses two
emulated CPUs, 4 GiB of guest RAM, and a 35-minute timeout.

The runner uses Xvfb's virtual X11 display with GTK OpenGL and QEMU's
`virtio-vga-gl`/virgl device. Mesa is forced to the llvmpipe CPU renderer. A
preflight checks Xvfb GLX, the X11 EGL platform, and a paused QEMU
`virtio-vga-gl` startup before downloading or booting the Arch image. This path
uses QEMU's GTK X11 EGL initialization. It does not use `egl-headless`, whose
QEMU implementation opens a DRM render node, so it does not depend on a host
`/dev/dri/renderD*` node or a visible desktop. The first manual workflow run
must confirm that the hosted runner's real GTK/EGL/virgl stack completes the
preflight and full guest session; the source and package checks establish
feasibility, not a successful run.

The guest receipt records and the host gate requires the compositor login to be
active on seat0 VT1, so logind can grant the session access to the virtual GPU's
seat devices. The runner records the installed Hyprland version and refuses a
package older than 0.55 because this Lua configuration uses the current Lua
config API. The live harness selects the documented Lua dispatcher-object focus
form on Hyprland 0.55 and later, keeps the older named dispatcher form for prior
versions, records the chosen command and result, and verifies that Hyprland
reports the exact Quickshell window address as active before sending input.

The live gate proves a real Arch/Hyprland/Wayland session, Quickshell QML
process, IPC protocol negotiation, widget create/update/destroy, renderer
restart/replay, stale-generation rejection, and a typed Quickshell `clicked`
event following synthetic keyboard input sent through Wayland. A separate
graph harness then runs both clock graphs through `execute run`, focuses their
actual Quickshell windows, sends Tab/Return through Wayland until the loopback
fixture's sync counter advances, and requires source-pinned SIGINT cancellation
plus teardown of observed Execute descendants. The host gate reads the separate
`graph-evidence.json` and verifies each scenario's counter increase and clean
teardown fields. These two gates remain scoped to the disposable Arch session;
they do not amount to full Omarchy desktop certification.

The existing harness leaves `native_desktop_certification` as `PENDING`. This
runner does not install Omarchy and does not certify the full Omarchy desktop,
monitor hotplug, login/logout cycles, compositor reload, whole-session
recovery, unattended deployment, physical mouse clicks, physical-GPU rendering
or Bash replacement. The key event is synthetic. The runner's guest is Arch
Linux with Hyprland and Quickshell installed from the dated Arch repository
snapshot; it is not a full Omarchy install. Hyprland's pinned package is
0.56.2-4 in the default 2026/10/14 snapshot; its upstream v0.56.2 source has
the Lua config APIs and the `--config` CLI option used here. The harness now
uses the documented Lua dispatcher object on that package and checks the active
window address before sending its key event. Unit-level command-order tests
cover the version selection and failure cases; execution against the actual
pinned package remains part of the first manual run.
