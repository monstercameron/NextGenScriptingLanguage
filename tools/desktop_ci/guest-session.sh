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

direct_acceptance_passed=0
if (( harness_status == 0 )) && [[ -s "$ARTIFACTS/evidence.json" ]] && jq -e '
    .status == "completed_with_pending_gates" and
    .native_desktop_certification == "PENDING" and
    .checks.target_preflight.status == "pass" and
    .checks.quickshell_wayland_smoke.status == "pass" and
    .checks.button_signal.status == "pass" and
    .results.renderer_restart_replay.status == "pass" and
    .results.hyprland_window_focus.status == "pass" and
    .results.hyprland_window_focus.active_window_verified == true and
    .cleanup.temporary_directory.status == "removed"
' "$ARTIFACTS/evidence.json" >/dev/null; then
    direct_acceptance_passed=1
fi

graph_harness_status=125
if (( direct_acceptance_passed == 1 )); then
    set +e
    EXECUTE_OMARCHY_DISPOSABLE=1 \
    EXECUTE_OMARCHY_SESSION_ID="$EXECUTE_OMARCHY_SESSION_ID" \
        python3 tools/desktop_graph_acceptance.py \
            --run-in-disposable-arch \
            --execute /home/arch/execute/target/release/execute \
            --evidence "$ARTIFACTS/graph-evidence.json" \
            > "$ARTIFACTS/graph-harness.stdout.json" \
            2> "$ARTIFACTS/graph-harness.stderr.log"
    graph_harness_status="$?"
    set -e
else
    : > "$ARTIFACTS/graph-harness.stdout.json"
    printf 'graph harness skipped because direct acceptance evidence failed (harness exit status %s)\n' \
        "$harness_status" > "$ARTIFACTS/graph-harness.stderr.log"
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
    printf 'direct_acceptance_passed=%s\n' "$([[ "$direct_acceptance_passed" == 1 ]] && echo true || echo false)"
    printf 'graph_harness_exit_status=%s\n' "$graph_harness_status"
} > "$ARTIFACTS/session-environment.txt"

if [[ ! -s "$ARTIFACTS/evidence.json" ]]; then
    printf 'harness did not produce evidence.json\n' > "$FAILED"
    exit 1
fi
if (( harness_status != 0 )); then
    printf 'harness-exit-status=%s\n' "$harness_status" > "$FAILED"
    exit "$harness_status"
fi
if (( direct_acceptance_passed != 1 )); then
    printf 'direct harness evidence did not prove the existing required smoke checks\n' > "$FAILED"
    exit 1
fi
if [[ ! -s "$ARTIFACTS/graph-evidence.json" ]]; then
    printf 'graph harness did not produce graph-evidence.json\n' > "$FAILED"
    exit 1
fi
if (( graph_harness_status != 0 )); then
    printf 'graph-harness-exit-status=%s\n' "$graph_harness_status" > "$FAILED"
    exit "$graph_harness_status"
fi
if ! jq -e '
    .status == "passed" and
    .checks.local_clock_graph_button_route == "pass" and
    .checks.remote_clock_graph_button_route == "pass" and
    .checks.natural_cancellation_and_reaping.status == "pass" and
    ([.scenarios[].producer] | sort) == ["local", "remote"] and
    all(.scenarios[];
        .status == "pass" and
        (.graph_button_route as $route |
            $route.status == "pass" and
            ($route.sync_count_before | type) == "number" and
            ($route.sync_count_after | type) == "number" and
            $route.sync_count_after > $route.sync_count_before) and
        .cancellation.status == "pass" and
        .forced_cleanup.status == "not_used" and
        .process_teardown.status == "pass" and
        (.process_teardown.observed_identity_count | type) == "number" and
        .process_teardown.observed_identity_count > 0 and
        .process_teardown.surviving_identities == []
    )
' "$ARTIFACTS/graph-evidence.json" > "$ARTIFACTS/graph-acceptance-gate.json"; then
    printf 'graph evidence did not prove local and remote button routing plus clean cancellation teardown\n' > "$FAILED"
    exit 1
fi

chmod -R a+rX "$ARTIFACTS"
touch "$READY"
chmod 0600 "$READY"
while [[ ! -e "$BASE/shutdown.request" ]]; do
    sleep 1
done
