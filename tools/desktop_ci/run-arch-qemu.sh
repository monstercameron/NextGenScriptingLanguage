#!/usr/bin/env bash
set -Eeuo pipefail

: "$GITHUB_WORKSPACE"
: "$RUNNER_TEMP"
: "$ARCH_BOXES_RELEASE"
: "$ARCH_ARCHIVE_DATE"
: "$GH_RUN_ID"
: "$GH_RUN_ATTEMPT"
: "$GH_SOURCE_SHA"

if [[ ! "$ARCH_BOXES_RELEASE" =~ ^[0-9]{8}\.[0-9]+$ ]]; then
    echo "ARCH_BOXES_RELEASE must look like YYYYMMDD.NNNNNN" >&2
    exit 2
fi
if [[ ! "$ARCH_ARCHIVE_DATE" =~ ^[0-9]{4}/[0-9]{2}/[0-9]{2}$ ]]; then
    echo "ARCH_ARCHIVE_DATE must look like YYYY/MM/DD" >&2
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
if [[ ! "$GH_RUN_ID" =~ ^[0-9]+$ || ! "$GH_RUN_ATTEMPT" =~ ^[0-9]+$ ]]; then
    echo "GitHub run id and attempt must be numeric" >&2
    exit 2
fi
if [[ ! "$GH_SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]; then
    echo "GH_SOURCE_SHA must be a full Git commit id" >&2
    exit 2
fi

RUN_ID="$GH_RUN_ID-$GH_RUN_ATTEMPT"
SSH_PORT=22222
VM_CPUS=2
VM_MEMORY_MIB=4096
VM_OVERLAY_LIMIT_BYTES=5368709120
VM_VIRTUAL_DISK_SIZE=8G
VM_VIRTUAL_DISK_SIZE_BYTES=8589934592
VM_ACCEPTANCE_SECONDS=900
IMAGE_NAME="Arch-Linux-x86_64-cloudimg-$ARCH_BOXES_RELEASE.qcow2"
IMAGE_BASE_URL="https://fastly.mirror.pkgbuild.com/images/v$ARCH_BOXES_RELEASE"
IMAGE_URL="$IMAGE_BASE_URL/$IMAGE_NAME"
ARTIFACT_DIR="$RUNNER_TEMP/desktop-ci-artifacts"
RUN_DIR=""
SOURCE_SHA="$(git -C "$GITHUB_WORKSPACE" rev-parse HEAD)"

QEMU_PID=""
XVFB_PID=""
QEMU_EXIT_STATUS="not_started"
GUEST_SHUTDOWN_STATUS="not_requested"
OVERLAY_REMOVED="false"
SSH_READY="false"
GUEST_ARTIFACT_CAPTURE_STATUS="not_attempted"
GUEST_JOURNAL_CAPTURE_STATUS="not_attempted"
GUEST_ARTIFACT_EXTRACT_STATUS="not_attempted"
GUEST_CAPTURE_ERRORS=()
MAX_GUEST_ARTIFACT_BYTES=33554432
MAX_GUEST_JOURNAL_BYTES=8388608

capture_limited_ssh_stream() {
    local name="$1"
    local byte_limit="$2"
    local remote_command="$3"
    local destination="$ARTIFACT_DIR/$name"
    local stderr_path="$ARTIFACT_DIR/$name.ssh-stderr.log"
    local status_path="$ARTIFACT_DIR/$name.status"
    local byte_count=0
    local stream_status
    local -a pipeline_status

    set +e
    timeout --signal=TERM --kill-after=2s 18s \
        ssh -p "$SSH_PORT" \
            -o BatchMode=yes \
            -o ConnectTimeout=4 \
            -o ConnectionAttempts=1 \
            -o StrictHostKeyChecking=no \
            -o UserKnownHostsFile=/dev/null \
            -o IdentitiesOnly=yes \
            -i "$RUN_DIR/ci_ed25519" \
            "arch@127.0.0.1" "$remote_command" \
            2> "$stderr_path" \
        | head -c "$byte_limit" > "$destination"
    pipeline_status=("${PIPESTATUS[@]}")
    set -e

    if [[ -f "$destination" ]]; then
        byte_count="$(stat -c '%s' "$destination" 2>/dev/null || printf '0')"
    fi
    if [[ "${pipeline_status[0]}" == "0" && "${pipeline_status[1]}" == "0" && "$byte_count" -lt "$byte_limit" ]]; then
        stream_status="complete"
    elif [[ "$byte_count" =~ ^[0-9]+$ ]] && (( byte_count >= byte_limit )); then
        stream_status="truncated_at_limit"
    else
        stream_status="failed"
    fi
    printf 'status=%s\nbytes=%s\nlimit=%s\nssh_status=%s\ncap_status=%s\n' \
        "$stream_status" "$byte_count" "$byte_limit" "${pipeline_status[0]}" "${pipeline_status[1]}" \
        > "$status_path"
    printf '%s' "$stream_status"
    [[ "$stream_status" == "complete" ]]
}

capture_guest_evidence() {
    local artifact_status journal_status
    local artifact_archive="$ARTIFACT_DIR/guest-artifacts.tar.gz"
    local guest_directory="$ARTIFACT_DIR/guest"

    if [[ ! -s "$RUN_DIR/ci_ed25519" ]]; then
        GUEST_ARTIFACT_CAPTURE_STATUS="unavailable_no_ssh_key"
        GUEST_JOURNAL_CAPTURE_STATUS="unavailable_no_ssh_key"
        GUEST_CAPTURE_ERRORS+=("guest evidence collection skipped: SSH key is unavailable")
        return 1
    fi

    if [[ "$GUEST_ARTIFACT_CAPTURE_STATUS" != "complete" ]]; then
        if artifact_status="$(capture_limited_ssh_stream \
            guest-artifacts.tar.gz "$MAX_GUEST_ARTIFACT_BYTES" \
            "if command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then sudo tar -C /var/tmp/execute-desktop-ci/artifacts -czf - .; else tar -C /var/tmp/execute-desktop-ci/artifacts -czf - .; fi")"; then
            GUEST_ARTIFACT_CAPTURE_STATUS="$artifact_status"
        else
            GUEST_ARTIFACT_CAPTURE_STATUS="$artifact_status"
            GUEST_CAPTURE_ERRORS+=("guest artifact archive collection: $artifact_status")
        fi
    fi

    if [[ "$GUEST_JOURNAL_CAPTURE_STATUS" != "complete" ]]; then
        if journal_status="$(capture_limited_ssh_stream \
            guest-system-journal.log "$MAX_GUEST_JOURNAL_BYTES" \
            "if command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then sudo journalctl -b --no-pager -n 800; else journalctl --user --no-pager -n 800; fi")"; then
            GUEST_JOURNAL_CAPTURE_STATUS="$journal_status"
        else
            GUEST_JOURNAL_CAPTURE_STATUS="$journal_status"
            GUEST_CAPTURE_ERRORS+=("guest journal collection: $journal_status")
        fi
    fi

    if [[ "$GUEST_ARTIFACT_CAPTURE_STATUS" == "complete" && "$GUEST_ARTIFACT_EXTRACT_STATUS" != "complete" ]]; then
        mkdir -p "$guest_directory"
        if tar -tzf "$artifact_archive" > "$ARTIFACT_DIR/guest-artifacts-tar-list.txt" 2> "$ARTIFACT_DIR/guest-artifacts-tar-test.stderr.log" \
            && tar -xzf "$artifact_archive" -C "$guest_directory" 2> "$ARTIFACT_DIR/guest-artifacts-tar-extract.stderr.log"; then
            GUEST_ARTIFACT_EXTRACT_STATUS="complete"
        else
            GUEST_ARTIFACT_EXTRACT_STATUS="failed"
            GUEST_CAPTURE_ERRORS+=("guest artifact archive validation or extraction failed")
        fi
    fi
    [[ "$GUEST_ARTIFACT_CAPTURE_STATUS" == "complete" && "$GUEST_ARTIFACT_EXTRACT_STATUS" == "complete" ]]
}

cleanup() {
    local exit_status="$?"
    local cleanup_status="clean"
    local poweroff_status="not_attempted"

    if [[ -n "$QEMU_PID" ]] && kill -0 "$QEMU_PID" 2>/dev/null && [[ -s "$RUN_DIR/ci_ed25519" ]]; then
        capture_guest_evidence || true
        set +e
        timeout --signal=TERM --kill-after=2s 8s \
            ssh -p "$SSH_PORT" \
                -o BatchMode=yes \
                -o ConnectTimeout=4 \
                -o ConnectionAttempts=1 \
                -o StrictHostKeyChecking=no \
                -o UserKnownHostsFile=/dev/null \
                -o IdentitiesOnly=yes \
                -i "$RUN_DIR/ci_ed25519" \
                "arch@127.0.0.1" "sudo systemctl poweroff" \
                > "$ARTIFACT_DIR/cleanup-poweroff.stdout.log" \
                2> "$ARTIFACT_DIR/cleanup-poweroff.stderr.log"
        poweroff_status="$?"
        set -e
        if (( poweroff_status == 0 )); then
            GUEST_SHUTDOWN_STATUS="cleanup_poweroff_requested"
        else
            GUEST_CAPTURE_ERRORS+=("cleanup poweroff request failed with status $poweroff_status")
            if [[ "$GUEST_SHUTDOWN_STATUS" == "not_requested" ]]; then
                GUEST_SHUTDOWN_STATUS="cleanup_poweroff_failed"
            fi
        fi
    fi

    if [[ -n "$QEMU_PID" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
        for _ in {1..10}; do
            kill -0 "$QEMU_PID" 2>/dev/null || break
            sleep 1
        done
        if kill -0 "$QEMU_PID" 2>/dev/null; then
            kill -TERM "$QEMU_PID" 2>/dev/null || true
            for _ in {1..10}; do
                kill -0 "$QEMU_PID" 2>/dev/null || break
                sleep 1
            done
        fi
        if kill -0 "$QEMU_PID" 2>/dev/null; then
            kill -KILL "$QEMU_PID" 2>/dev/null || true
            cleanup_status="qemu_killed_after_timeout"
            GUEST_SHUTDOWN_STATUS="host_terminated_after_timeout"
        elif [[ "$GUEST_SHUTDOWN_STATUS" == "cleanup_poweroff_requested" ]]; then
            GUEST_SHUTDOWN_STATUS="cleanup_poweroff_completed"
        fi
    fi
    if [[ -n "$QEMU_PID" ]]; then
        set +e
        wait "$QEMU_PID"
        QEMU_EXIT_STATUS="$?"
        set -e
    fi

    if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill -TERM "$XVFB_PID" 2>/dev/null || true
        set +e
        wait "$XVFB_PID"
        set -e
    fi

    if [[ -n "$RUN_DIR" && -d "$RUN_DIR" && "$RUN_DIR" == "$RUNNER_TEMP"/desktop-ci-run.* ]]; then
        rm -rf -- "$RUN_DIR"
        OVERLAY_REMOVED="true"
    elif [[ -n "$RUN_DIR" ]]; then
        cleanup_status="run_directory_check_failed"
    fi

    local capture_errors
    capture_errors="$(IFS='|'; printf '%s' "${GUEST_CAPTURE_ERRORS[*]}")"
    python3 - "$ARTIFACT_DIR/host-teardown.json" "$RUN_ID" "$QEMU_EXIT_STATUS" "$GUEST_SHUTDOWN_STATUS" "$OVERLAY_REMOVED" "$cleanup_status" "$exit_status" "$SSH_READY" "$GUEST_ARTIFACT_CAPTURE_STATUS" "$GUEST_JOURNAL_CAPTURE_STATUS" "$GUEST_ARTIFACT_EXTRACT_STATUS" "$capture_errors" <<'PY'
import json
import sys
from pathlib import Path

destination = Path(sys.argv[1])
destination.write_text(
    json.dumps(
        {
            "run_id": sys.argv[2],
            "qemu_exit_status": sys.argv[3],
            "guest_shutdown": sys.argv[4],
            "overlay_and_temp_removed": sys.argv[5] == "true",
            "cleanup_status": sys.argv[6],
            "workflow_exit_status": sys.argv[7],
            "ssh_became_ready": sys.argv[8] == "true",
            "guest_artifact_capture": sys.argv[9],
            "guest_journal_capture": sys.argv[10],
            "guest_artifact_extract": sys.argv[11],
            "guest_collection_errors": [error for error in sys.argv[12].split("|") if error],
        },
        indent=2,
        sort_keys=True,
    )
    + "\n",
    encoding="utf-8",
)
PY
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$ARTIFACT_DIR"
RUN_DIR="$(mktemp -d "$RUNNER_TEMP/desktop-ci-run.XXXXXX")"

available_bytes="$(df -B1 --output=avail "$RUNNER_TEMP" | tail -n 1 | tr -d ' ')"
if [[ ! "$available_bytes" =~ ^[0-9]+$ ]] || (( available_bytes < 7000000000 )); then
    echo "At least 7 GB of runner temp space is required; found $available_bytes" >&2
    exit 1
fi

{
    printf 'source_commit=%s\n' "$SOURCE_SHA"
    printf 'requested_source_commit=%s\n' "$GH_SOURCE_SHA"
    printf 'arch_boxes_release=%s\n' "$ARCH_BOXES_RELEASE"
    printf 'arch_archive_date=%s\n' "$ARCH_ARCHIVE_DATE"
    printf 'qemu_accelerator=tcg\n'
    printf 'qemu_cpus=%s\n' "$VM_CPUS"
    printf 'qemu_memory_mib=%s\n' "$VM_MEMORY_MIB"
    printf 'qemu_overlay_file_limit_bytes=%s\n' "$VM_OVERLAY_LIMIT_BYTES"
    printf 'qemu_virtual_disk_size=%s\n' "$VM_VIRTUAL_DISK_SIZE"
    printf 'qemu_virtual_disk_size_bytes=%s\n' "$VM_VIRTUAL_DISK_SIZE_BYTES"
    printf 'qemu_display=gtk_x11_egl_xvfb_llvmpipe\n'
    printf 'host_rendernode_required=false\n'
    printf 'github_run_id=%s\n' "$RUN_ID"
    printf 'runner_image_os=%s\n' "$(printenv ImageOS || printf unknown)"
    printf 'runner_image_version=%s\n' "$(printenv ImageVersion || printf unknown)"
} > "$ARTIFACT_DIR/host-receipt.txt"

if [[ "$SOURCE_SHA" != "$GH_SOURCE_SHA" ]]; then
    echo "Checked-out source commit does not match the workflow event commit" >&2
    exit 1
fi
if [[ ! -x "$GITHUB_WORKSPACE/target/release/execute" ]]; then
    echo "Build output target/release/execute is missing" >&2
    exit 1
fi

Xvfb -displayfd 3 -screen 0 1280x800x24 +extension GLX -nolisten tcp \
    3> "$RUN_DIR/xvfb-display-number" \
    > "$ARTIFACT_DIR/xvfb.log" 2>&1 &
XVFB_PID="$!"
for _ in {1..40}; do
    if [[ -s "$RUN_DIR/xvfb-display-number" ]]; then
        break
    fi
    if ! kill -0 "$XVFB_PID" 2>/dev/null; then
        cat "$ARTIFACT_DIR/xvfb.log" >&2
        echo "Xvfb exited before it allocated a display" >&2
        exit 1
    fi
    sleep 0.25
done
if [[ ! -s "$RUN_DIR/xvfb-display-number" ]]; then
    echo "Xvfb did not allocate a display within 10 seconds" >&2
    exit 1
fi
DISPLAY=":$(tr -d '[:space:]' < "$RUN_DIR/xvfb-display-number")"
export DISPLAY
LIBGL_ALWAYS_SOFTWARE=true GALLIUM_DRIVER=llvmpipe LP_NUM_THREADS=2 \
    glxinfo -B > "$ARTIFACT_DIR/llvmpipe-glx.txt" 2>&1
grep -qi llvmpipe "$ARTIFACT_DIR/llvmpipe-glx.txt"
LIBGL_ALWAYS_SOFTWARE=true GALLIUM_DRIVER=llvmpipe LP_NUM_THREADS=2 \
    eglinfo -p x11 -B > "$ARTIFACT_DIR/llvmpipe-egl-x11.txt" 2>&1

curl --fail --location --retry 3 --retry-all-errors \
    --output "$RUN_DIR/$IMAGE_NAME" "$IMAGE_URL"
python3 "$ROOT/tools/desktop_ci/verify_image_sha256.py" \
    "$RUN_DIR/$IMAGE_NAME" "${ARCH_IMAGE_SHA256:-}"
printf '%s  %s\n' "$ARCH_IMAGE_SHA256" "$IMAGE_NAME" \
    > "$ARTIFACT_DIR/arch-cloud-image.sha256"
qemu-img info --output=json "$RUN_DIR/$IMAGE_NAME" > "$ARTIFACT_DIR/arch-cloud-image-info.json"

ssh-keygen -q -t ed25519 -N "" -C "desktop-acceptance-$RUN_ID" -f "$RUN_DIR/ci_ed25519"
pubkey="$(cat "$RUN_DIR/ci_ed25519.pub")"
cat > "$RUN_DIR/user-data" <<CLOUD
#cloud-config
growpart:
  mode: auto
  devices: ["/"]
resize_rootfs: true
users:
  - default
ssh_authorized_keys:
  - $pubkey
disable_root: true
ssh_pwauth: false
CLOUD
cat > "$RUN_DIR/meta-data" <<META
instance-id: iid-desktop-$RUN_ID
local-hostname: execute-desktop-ci
META
cloud-localds "$RUN_DIR/seed.iso" "$RUN_DIR/user-data" "$RUN_DIR/meta-data"

qemu-img create -f qcow2 -F qcow2 \
    -b "$RUN_DIR/$IMAGE_NAME" "$RUN_DIR/arch-overlay.qcow2" "$VM_VIRTUAL_DISK_SIZE" \
    > "$ARTIFACT_DIR/qemu-overlay.log" 2>&1
qemu-img info --output=json "$RUN_DIR/arch-overlay.qcow2" > "$ARTIFACT_DIR/arch-overlay-info.json"
python3 - "$ARTIFACT_DIR/arch-overlay-info.json" "$VM_VIRTUAL_DISK_SIZE_BYTES" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    actual_bytes = json.load(source)["virtual-size"]
expected_bytes = int(sys.argv[2])
if actual_bytes != expected_bytes:
    raise SystemExit(
        f"qcow2 virtual size is {actual_bytes} bytes, expected {expected_bytes}"
    )
PY

git -C "$GITHUB_WORKSPACE" archive --format=tar --prefix=execute/ "$SOURCE_SHA" \
    | gzip -1 > "$RUN_DIR/source.tar.gz"
sha256sum "$RUN_DIR/source.tar.gz" > "$ARTIFACT_DIR/source-bundle.sha256"
sha256sum "$GITHUB_WORKSPACE/target/release/execute" > "$ARTIFACT_DIR/execute-binary.sha256"

LIBGL_ALWAYS_SOFTWARE=true GALLIUM_DRIVER=llvmpipe LP_NUM_THREADS=2 DISPLAY="$DISPLAY" \
prlimit --fsize="$VM_OVERLAY_LIMIT_BYTES:$VM_OVERLAY_LIMIT_BYTES" -- \
qemu-system-x86_64 \
    -name "execute-desktop-$RUN_ID" \
    -machine q35 \
    -accel tcg,thread=multi \
    -cpu max \
    -smp "$VM_CPUS" \
    -m "$VM_MEMORY_MIB" \
    -vga none \
    -device virtio-vga-gl \
    -display gtk,gl=on,show-menubar=off \
    -drive "file=$RUN_DIR/arch-overlay.qcow2,if=virtio,format=qcow2,cache=none,discard=unmap" \
    -drive "file=$RUN_DIR/seed.iso,media=cdrom,readonly=on" \
    -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22" \
    -device virtio-net-pci,netdev=net0 \
    -serial "file:$ARTIFACT_DIR/guest-serial.log" \
    -monitor none \
    > "$ARTIFACT_DIR/qemu-stderr.log" 2>&1 &
QEMU_PID="$!"

ssh_guest() {
    ssh -p "$SSH_PORT" \
        -o BatchMode=yes \
        -o ConnectTimeout=5 \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o IdentitiesOnly=yes \
        -i "$RUN_DIR/ci_ed25519" \
        "arch@127.0.0.1" "$@"
}

ssh_ready=0
ssh_deadline=$((SECONDS + 600))
while (( SECONDS < ssh_deadline )); do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "QEMU exited before the guest SSH service became ready" >&2
        exit 1
    fi
    if ssh_guest true >/dev/null 2>&1; then
        ssh_ready=1
        break
    fi
    sleep 5
done
if (( ssh_ready == 0 )); then
    echo "Guest SSH did not become ready within 10 minutes" >&2
    exit 1
fi
SSH_READY="true"

ssh_guest "mkdir -p /home/arch/execute/target/release"
ssh -p "$SSH_PORT" \
    -o BatchMode=yes \
    -o ConnectTimeout=5 \
    -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o IdentitiesOnly=yes \
    -i "$RUN_DIR/ci_ed25519" \
    "arch@127.0.0.1" \
    "mkdir -p /home/arch/execute && tar -xzf - -C /home/arch/execute --strip-components=1" \
    < "$RUN_DIR/source.tar.gz"
scp -P "$SSH_PORT" \
    -o BatchMode=yes \
    -o ConnectTimeout=5 \
    -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o IdentitiesOnly=yes \
    -i "$RUN_DIR/ci_ed25519" \
    "$GITHUB_WORKSPACE/target/release/execute" \
    "arch@127.0.0.1:/home/arch/execute/target/release/execute"

ssh_guest "sudo env ARCH_ARCHIVE_DATE='$ARCH_ARCHIVE_DATE' ARCH_BOXES_RELEASE='$ARCH_BOXES_RELEASE' GH_RUN_ID='$RUN_ID' GH_SOURCE_SHA='$SOURCE_SHA' bash /home/arch/execute/tools/desktop_ci/guest-provision.sh" \
    > "$ARTIFACT_DIR/guest-provision.log" 2>&1 || {
        status="$?"
        cat "$ARTIFACT_DIR/guest-provision.log" >&2
        exit "$status"
    }

deadline=$((SECONDS + VM_ACCEPTANCE_SECONDS))
guest_ready=0
while (( SECONDS < deadline )); do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "QEMU exited before the live harness finished" >&2
        exit 1
    fi
    if ssh_guest "test -f /var/tmp/execute-desktop-ci/READY" >/dev/null 2>&1; then
        guest_ready=1
        break
    fi
    if ssh_guest "test -f /var/tmp/execute-desktop-ci/FAILED" >/dev/null 2>&1; then
        ssh_guest "cat /var/tmp/execute-desktop-ci/FAILED" >&2 || true
        echo "Guest desktop session failed before producing an acceptance report" >&2
        exit 1
    fi
    sleep 5
done
if (( guest_ready == 0 )); then
    echo "Live guest acceptance exceeded the 15 minute deadline" >&2
    exit 1
fi

if ! capture_guest_evidence; then
    echo "Guest artifacts could not be collected or validated" >&2
    exit 1
fi
if [[ "$GUEST_JOURNAL_CAPTURE_STATUS" != "complete" ]]; then
    echo "Guest journal capture was incomplete: $GUEST_JOURNAL_CAPTURE_STATUS" >&2
fi
cp "$ARTIFACT_DIR/guest-system-journal.log" "$ARTIFACT_DIR/guest/system-journal.log" 2>/dev/null || true

python3 - "$ARTIFACT_DIR/guest/evidence.json" "$ARTIFACT_DIR/guest/quickshell-session.png" "$ARTIFACT_DIR/guest/login-session.txt" "$ARTIFACT_DIR/guest/graph-evidence.json" <<'PY'
import json
import sys
from pathlib import Path

report_path = Path(sys.argv[1])
screenshot_path = Path(sys.argv[2])
login_session_path = Path(sys.argv[3])
graph_report_path = Path(sys.argv[4])
report = json.loads(report_path.read_text(encoding="utf-8"))
graph_report = json.loads(graph_report_path.read_text(encoding="utf-8"))
graph_checks = graph_report.get("checks")
if not isinstance(graph_checks, dict):
    graph_checks = {}
graph_scenarios = graph_report.get("scenarios")
if not isinstance(graph_scenarios, list):
    graph_scenarios = []
login_session = {}
for line in login_session_path.read_text(encoding="utf-8").splitlines():
    if "=" in line:
        key, value = line.split("=", 1)
        login_session[key] = value

def status_at(mapping, *keys):
    value = mapping
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value.get("status") if isinstance(value, dict) else None

def graph_scenario_pass(producer):
    matches = [item for item in graph_scenarios if isinstance(item, dict) and item.get("producer") == producer]
    if len(matches) != 1:
        return False
    scenario = matches[0]
    route = scenario.get("graph_button_route")
    cancellation = scenario.get("cancellation")
    forced_cleanup = scenario.get("forced_cleanup")
    teardown = scenario.get("process_teardown")
    if not all(isinstance(item, dict) for item in (route, cancellation, forced_cleanup, teardown)):
        return False
    before, after = route.get("sync_count_before"), route.get("sync_count_after")
    observed = teardown.get("observed_identity_count")
    return (
        scenario.get("status") == "pass"
        and route.get("status") == "pass"
        and isinstance(before, int) and not isinstance(before, bool)
        and isinstance(after, int) and not isinstance(after, bool) and after > before
        and cancellation.get("status") == "pass"
        and forced_cleanup.get("status") == "not_used"
        and teardown.get("status") == "pass"
        and isinstance(observed, int) and not isinstance(observed, bool) and observed > 0
        and teardown.get("surviving_identities") == []
    )

producers = [item.get("producer") for item in graph_scenarios if isinstance(item, dict)]

required = {
    "report_status": report.get("status") == "completed_with_pending_gates",
    "native_certification_remains_pending": report.get("native_desktop_certification") == "PENDING",
    "target_preflight": status_at(report, "checks", "target_preflight") == "pass",
    "wayland_smoke": status_at(report, "checks", "quickshell_wayland_smoke") == "pass",
    "direct_quickshell_typed_clicked_event": status_at(report, "checks", "button_signal") == "pass",
    "restart_replay": status_at(report, "results", "renderer_restart_replay") == "pass",
    "hyprland_focus_verified_before_input": (
        status_at(report, "results", "hyprland_window_focus") == "pass"
        and report.get("results", {}).get("hyprland_window_focus", {}).get("active_window_verified") is True
        and report.get("results", {}).get("hyprland_window_focus", {}).get("focus_form")
        in {"lua_dispatcher_object", "legacy_named_dispatcher"}
    ),
    "runtime_temp_removed": status_at(report, "cleanup", "temporary_directory") == "removed",
    "screenshot_captured": screenshot_path.is_file() and screenshot_path.stat().st_size > 0,
    "graph_report_passed": graph_report.get("status") == "passed",
    "graph_preflight_passed": status_at(graph_report, "checks", "target_preflight") == "pass",
    "graph_scenarios_exactly_local_and_remote": sorted(producers) == ["local", "remote"],
    "local_graph_button_sync_and_clean_teardown": (
        graph_checks.get("local_clock_graph_button_route") == "pass"
        and graph_scenario_pass("local")
    ),
    "remote_graph_button_sync_and_clean_teardown": (
        graph_checks.get("remote_clock_graph_button_route") == "pass"
        and graph_scenario_pass("remote")
    ),
    "graph_natural_cancellation_and_reaping": status_at(graph_report, "checks", "natural_cancellation_and_reaping") == "pass",
    "active_local_seat0_vt1": (
        login_session.get("Seat") == "seat0"
        and login_session.get("State") == "active"
        and login_session.get("VTNr") == "1"
    ),
}
Path(sys.argv[1]).with_name("acceptance-gate.json").write_text(
    json.dumps(
        {
            "required_smoke_passed": all(required.values()),
            "required_checks": required,
            "scope": "live Arch/Hyprland/Quickshell protocol smoke plus RuntimeGraphExecutor-to-Quickshell button routing; full Omarchy desktop certification remains pending",
            "proof_boundary": "The direct harness proves renderer protocol behavior; the separate graph harness proves only the two local/remote button routes and observed cancellation teardown on this disposable Arch session.",
            "next_gate": "Run the remaining Omarchy-specific desktop scenarios before claiming native desktop certification.",
        },
        indent=2,
        sort_keys=True,
    )
    + "\n",
    encoding="utf-8",
)
print(json.dumps(required, sort_keys=True))
if not all(required.values()):
    raise SystemExit("one or more required live smoke checks did not pass")
PY

ssh_guest "sudo systemctl poweroff" >/dev/null 2>&1 || true
for _ in {1..60}; do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        GUEST_SHUTDOWN_STATUS="guest_poweroff_completed"
        break
    fi
    sleep 1
done
if [[ "$GUEST_SHUTDOWN_STATUS" != "guest_poweroff_completed" ]]; then
    GUEST_SHUTDOWN_STATUS="guest_poweroff_timed_out"
    echo "Guest did not power off in 60 seconds" >&2
    exit 1
fi

echo "Live Arch/Hyprland/Quickshell smoke completed; full Omarchy certification remains PENDING."
