#!/usr/bin/env bash
set -Eeuo pipefail

: "$ARCH_ARCHIVE_DATE"
: "$ARCH_BOXES_RELEASE"
: "$GH_RUN_ID"
: "$GH_SOURCE_SHA"

if [[ ! "$ARCH_ARCHIVE_DATE" =~ ^[0-9]{4}/[0-9]{2}/[0-9]{2}$ ]]; then
    echo "Invalid Arch Archive date" >&2
    exit 2
fi
python3 - "$ARCH_BOXES_RELEASE" "$ARCH_ARCHIVE_DATE" <<'PY'
from datetime import datetime
import sys
image_date = datetime.strptime(sys.argv[1].split(".", 1)[0], "%Y%m%d").date()
archive_date = datetime.strptime(sys.argv[2], "%Y/%m/%d").date()
if not 0 <= (image_date - archive_date).days <= 1:
    raise SystemExit("Arch Archive date must be the image date or the preceding daily snapshot")
PY

BASE=/var/tmp/execute-desktop-ci
ARTIFACTS="$BASE/artifacts"
mkdir -p "$ARTIFACTS"
chmod 0700 "$BASE" "$ARTIFACTS"
chown -R arch:users "$BASE"
cp /etc/pacman.d/mirrorlist "$ARTIFACTS/mirrorlist.before" 2>/dev/null || true
printf 'arch_boxes_release=%s\narch_archive_date=%s\ngithub_run_id=%s\ngithub_source_sha=%s\n' \
    "$ARCH_BOXES_RELEASE" "$ARCH_ARCHIVE_DATE" "$GH_RUN_ID" "$GH_SOURCE_SHA" \
    > "$ARTIFACTS/guest-provision-receipt.txt"

cloud_init_status="$ARTIFACTS/cloud-init-status.txt"
cloud_init_wait_status=complete
if ! timeout --signal=TERM --kill-after=5s 300s cloud-init status --wait > "$cloud_init_status" 2>&1; then
    cloud_init_wait_status=failed
    cat "$cloud_init_status" >&2 || true
fi
cloud-init status --long >> "$cloud_init_status" 2>&1 || true

growpart_path="$(command -v growpart || true)"
root_size_bytes="$(df --output=size -B1 / | tail -n 1 | tr -d ' ')"
root_available_bytes="$(df --output=avail -B1 / | tail -n 1 | tr -d ' ')"
{
    printf 'cloud_init_wait_status=%s\n' "$cloud_init_wait_status"
    printf 'growpart_path=%s\n' "$growpart_path"
    printf 'root_source=%s\n' "$(findmnt --noheadings --output SOURCE /)"
    printf 'root_fstype=%s\n' "$(findmnt --noheadings --output FSTYPE /)"
    printf 'root_size_bytes=%s\n' "$root_size_bytes"
    printf 'root_available_bytes=%s\n' "$root_available_bytes"
    printf '\nlsblk:\n'
    lsblk --bytes --output NAME,PATH,SIZE,TYPE,FSTYPE,MOUNTPOINTS
    printf '\ndf:\n'
    df --block-size=1 --portability /
} > "$ARTIFACTS/disk-preflight.txt"

if [[ "$cloud_init_wait_status" != complete ]]; then
    echo "cloud-init did not finish successfully before desktop provisioning" >&2
    exit 1
fi
if [[ -z "$growpart_path" ]]; then
    echo "growpart is not installed in the Arch cloud image" >&2
    exit 1
fi
if [[ ! "$root_size_bytes" =~ ^[0-9]+$ || ! "$root_available_bytes" =~ ^[0-9]+$ ]]; then
    echo "could not determine root filesystem capacity" >&2
    exit 1
fi
if (( root_size_bytes < 4294967296 )); then
    echo "root filesystem was not expanded to at least 4 GiB" >&2
    exit 1
fi
if (( root_available_bytes < 2147483648 )); then
    echo "root filesystem has less than 2 GiB free before desktop package installation" >&2
    exit 1
fi

cat > /etc/pacman.d/mirrorlist <<MIRROR
Server = https://archive.archlinux.org/repos/$ARCH_ARCHIVE_DATE/\$repo/os/\$arch
MIRROR

pacman -Syyuu --noconfirm --needed \
    hyprland quickshell grim wtype jq mesa qt6-wayland sudo dbus openssh python \
    2>&1 | tee "$ARTIFACTS/pacman-install.log"

hyprland_version="$(Hyprland --version 2>&1)"
printf '%s\n' "$hyprland_version" > "$ARTIFACTS/hyprland-installed-version.txt"
python3 - "$hyprland_version" <<'PY'
import re
import sys

match = re.search(r"Hyprland\s+v?(\d+)\.(\d+)\.(\d+)", sys.argv[1])
if match is None:
    raise SystemExit("could not determine the installed Hyprland version")
version = tuple(int(part) for part in match.groups())
if version < (0, 55, 0):
    raise SystemExit("the dated Arch snapshot must provide Hyprland >= 0.55.0 for the Lua config")
PY

id arch
usermod -aG wheel arch
chsh -s /bin/bash arch
install -d -m 0755 /etc/sudoers.d
printf 'arch ALL=(ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/desktop-ci
chmod 0440 /etc/sudoers.d/desktop-ci
visudo -cf /etc/sudoers.d/desktop-ci > "$ARTIFACTS/sudoers-check.log"

pam_file=/etc/pam.d/system-login
if ! grep -Eq '^[[:space:]]*-?session[[:space:]].*pam_systemd\.so' "$pam_file"; then
    pam_file="$(grep -El '^[[:space:]]*-?session[[:space:]].*pam_systemd\.so' /etc/pam.d/* | head -n 1 || true)"
fi
if [[ -z "$pam_file" || ! -f "$pam_file" ]]; then
    echo "No local PAM session entry for pam_systemd.so was found" >&2
    exit 1
fi
python3 /home/arch/execute/tools/desktop_ci/pam_transform.py --self-test
python3 /home/arch/execute/tools/desktop_ci/pam_transform.py "$pam_file"
cp "$pam_file" "$ARTIFACTS/pam-session-config.txt"

install -d -m 0755 /etc/systemd/system/getty@tty1.service.d
cat > /etc/systemd/system/getty@tty1.service.d/desktop-ci.conf <<'GETTY'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin arch --noclear - linux
GETTY

cat > /home/arch/.bash_profile <<'PROFILE'
if [[ "$(tty 2>/dev/null || true)" == "/dev/tty1" && -z "$SSH_TTY" ]]; then
    export XDG_SESSION_TYPE=wayland
    export XDG_SESSION_DESKTOP=Hyprland
    export XDG_CURRENT_DESKTOP=Hyprland
    export QT_QPA_PLATFORM=wayland
    export QT_QUICK_BACKEND=software
    export QSG_INFO=1
    export EXECUTE_OMARCHY_DISPOSABLE=1
    export EXECUTE_OMARCHY_SESSION_ID="__EXECUTE_SESSION_ID__"
    exec dbus-run-session -- start-hyprland -- --config /home/arch/execute/tools/desktop_ci/hyprland.lua
fi
PROFILE
sed -i "s/__EXECUTE_SESSION_ID__/disposable-${GH_RUN_ID}/" /home/arch/.bash_profile
chown arch:users /home/arch/.bash_profile
chmod 0644 /home/arch/.bash_profile

install -d -o arch -g users -m 0700 "$ARTIFACTS"
systemctl daemon-reload
systemctl enable getty@tty1.service
systemctl restart getty@tty1.service
