#!/usr/bin/env bash
set -Eeuo pipefail

BASE=/var/tmp/execute-desktop-ci
ARTIFACTS="$BASE/artifacts"
READY="$BASE/READY"
FAILED="$BASE/FAILED"
mkdir -p "$ARTIFACTS"
chmod 0700 "$ARTIFACTS"
cd /home/arch/execute

on_exit() {
    local status="$?"
    if [[ ! -e "$READY" && ! -e "$FAILED" ]]; then
        printf 'guest-session-exit=%s\n' "$status" > "$FAILED"
        chmod 0600 "$FAILED"
    fi
}
trap on_exit EXIT

capture_wayland_screen() {
    for _ in {1..120}; do
        if hyprctl clients -j 2>/dev/null \
            | jq -e 'any(.[]; .title == "Execute generic renderer")' >/dev/null 2>&1; then
            sleep 1
            output="$(hyprctl monitors -j 2>/dev/null | jq -r '.[0].name // empty' | head -n 1)"
            if [[ -n "$output" ]] && grim -o "$output" "$ARTIFACTS/quickshell-session.png" 2>"$ARTIFACTS/grim.stderr.log"; then
                printf 'captured_output=%s\n' "$output" > "$ARTIFACTS/screenshot-status.txt"
                return 0
            fi
        fi
        sleep 0.25
    done
    printf 'screenshot was not captured during the live renderer window\n' > "$ARTIFACTS/screenshot-status.txt"
    return 0
}

capture_wayland_screen &
screen_pid="$!"

set +e
python3 tools/omarchy_acceptance.py \
    --run-in-disposable-omarchy \
    --binary /home/arch/execute/target/release/execute \
    --evidence "$ARTIFACTS/evidence.json" \
    > "$ARTIFACTS/harness.stdout.json" \
    2> "$ARTIFACTS/harness.stderr.log"
harness_status="$?"
set -e

if ! wait "$screen_pid"; then
    printf 'screenshot watcher did not complete cleanly\n' >> "$ARTIFACTS/screenshot-status.txt"
fi

if [[ -x /usr/bin/qs ]]; then
    /usr/bin/qs --version > "$ARTIFACTS/quickshell-version.txt" 2>&1 || true
else
    command -v qs > "$ARTIFACTS/quickshell-version.txt" 2>&1 || true
    qs --version >> "$ARTIFACTS/quickshell-version.txt" 2>&1 || true
fi
hyprctl version > "$ARTIFACTS/hyprland-version.txt" 2>&1 || true
busctl --version > "$ARTIFACTS/busctl-version.txt" 2>&1 || true
pacman -Q > "$ARTIFACTS/package-versions.txt" 2>&1 || true
sha256sum /var/lib/pacman/sync/*.db > "$ARTIFACTS/repository-database-sha256.txt" 2>&1 || true
find /var/cache/pacman/pkg -maxdepth 1 -type f -name '*.pkg.tar.*' -print0 | xargs -0 -r sha256sum > "$ARTIFACTS/package-cache-sha256.txt" 2>&1 || true
cat /etc/os-release > "$ARTIFACTS/os-release.txt"
loginctl show-session "$XDG_SESSION_ID" --no-pager \
    --property=Type --property=Class --property=Remote --property=State --property=Seat --property=VTNr \
    > "$ARTIFACTS/login-session.txt" 2>&1 || true
hyprctl monitors -j > "$ARTIFACTS/hyprland-monitors-after.json" 2>&1 || true
{
    printf 'XDG_SESSION_TYPE=%s\n' "$XDG_SESSION_TYPE"
    printf 'QT_QPA_PLATFORM=%s\n' "${QT_QPA_PLATFORM:-unset}"
    printf 'QT_QUICK_BACKEND=%s\n' "${QT_QUICK_BACKEND:-unset}"
    printf 'EXECUTE_OMARCHY_SESSION_ID=%s\n' "$EXECUTE_OMARCHY_SESSION_ID"
    printf 'WAYLAND_DISPLAY_PRESENT=%s\n' "$([[ -n "$WAYLAND_DISPLAY" ]] && echo true || echo false)"
    printf 'HYPRLAND_INSTANCE_SIGNATURE_PRESENT=%s\n' "$([[ -n "$HYPRLAND_INSTANCE_SIGNATURE" ]] && echo true || echo false)"
    printf 'DBUS_SESSION_BUS_ADDRESS_PRESENT=%s\n' "$([[ -n "$DBUS_SESSION_BUS_ADDRESS" ]] && echo true || echo false)"
    printf 'harness_exit_status=%s\n' "$harness_status"
} > "$ARTIFACTS/session-environment.txt"

if [[ ! -s "$ARTIFACTS/evidence.json" ]]; then
    printf 'harness did not produce evidence.json\n' > "$FAILED"
    exit 1
fi
if (( harness_status != 0 )); then
    printf 'harness-exit-status=%s\n' "$harness_status" > "$FAILED"
    exit "$harness_status"
fi

chmod -R a+rX "$ARTIFACTS"
touch "$READY"
chmod 0600 "$READY"
while [[ ! -e "$BASE/shutdown.request" ]]; do
    sleep 1
done
