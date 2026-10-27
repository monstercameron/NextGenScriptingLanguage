#!/usr/bin/env bash
set -Eeuo pipefail

# Linux-only nightly writeback-drop model. All test-filesystem block I/O goes through a
# loop device backed by a private image under RUNNER_TEMP; no host disk is formatted.
if [[ ${RUNNER_ENVIRONMENT:-} != github-hosted ]]; then
  echo "dm-flakey harness is restricted to a GitHub-hosted ephemeral runner" >&2
  exit 1
fi
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEMP_BASE=${RUNNER_TEMP:-/tmp}
WORK=$(mktemp -d "$TEMP_BASE/execute-crash-dm-flakey.XXXXXX")
MOUNT_DIR="$WORK/mnt"
IMAGE="$WORK/checkpoint.img"
DM_NAME="execute-crash-${WORK##*.}"
LOOP_DEVICE=""
LOOP_SETUP_ATTEMPTED=0
MOUNTED=0
DM_CREATED=0
RUN_PID=""
HOLD_DIR="$WORK/failpoint-hold"

associated_loop_device() {
  local listing candidate selected=""
  listing=$(sudo losetup --list --noheadings --raw --output NAME --associated "$IMAGE") || return 1
  while IFS= read -r candidate; do
    [[ -z "$candidate" ]] && continue
    [[ "$candidate" =~ ^/dev/loop[0-9]+$ ]] || return 1
    [[ -z "$selected" ]] || return 1
    selected="$candidate"
  done <<< "$listing"
  if [[ -n "$selected" ]]; then
    printf '%s\n' "$selected"
  fi
}

case "$WORK" in
  "$TEMP_BASE"/execute-crash-dm-flakey.*) ;;
  *) echo "refusing unexpected scratch directory: $WORK" >&2; exit 1 ;;
esac
if [[ ! "$DM_NAME" =~ ^execute-crash-[[:alnum:]]+$ || ! -d "$WORK" || -L "$WORK" ]]; then
  echo "refusing unexpected private test path or device-mapper name" >&2
  exit 1
fi

cleanup() {
  set +e
  if [[ -n "$RUN_PID" ]]; then
    kill "$RUN_PID" >/dev/null 2>&1
    wait "$RUN_PID" >/dev/null 2>&1
    RUN_PID=""
  fi
  if (( MOUNTED )); then
    if ! sudo umount "$MOUNT_DIR"; then
      echo "could not unmount the private test filesystem; preserving $WORK for runner cleanup" >&2
      return
    fi
    MOUNTED=0
  fi
  if (( DM_CREATED )); then
    if ! sudo dmsetup remove "$DM_NAME"; then
      echo "could not remove the private device-mapper target; preserving $WORK" >&2
      return
    fi
    DM_CREATED=0
  fi
  # `losetup --find --show` should print the device it attached. If it returned
  # malformed output or failed after attaching, recover the device by querying
  # associations with this unique private image before removing the scratch tree.
  if [[ -z "$LOOP_DEVICE" ]] && (( LOOP_SETUP_ATTEMPTED )); then
    if discovered_loop=$(associated_loop_device); then
      if [[ -n "$discovered_loop" ]]; then
        LOOP_DEVICE="$discovered_loop"
      fi
    else
      echo "could not identify the loop device associated with $IMAGE; preserving $WORK" >&2
      return
    fi
  fi
  if [[ -n "$LOOP_DEVICE" ]]; then
    if ! sudo losetup --detach "$LOOP_DEVICE"; then
      echo "could not detach the private loop device; preserving $WORK" >&2
      return
    fi
  fi
  rm -rf -- "$WORK"
}
trap cleanup EXIT

if ! sudo -n true; then
  echo "dm-flakey test requires passwordless sudo on its disposable Linux runner" >&2
  exit 1
fi
if ! sudo dmsetup targets | awk '$1 == "flakey" { found = 1 } END { exit !found }'; then
  echo "the runner kernel does not provide the dm-flakey target" >&2
  exit 1
fi

mkdir -p "$MOUNT_DIR" "$WORK/manifests"
truncate -s 128M "$IMAGE"
if [[ ! -f "$IMAGE" || -L "$IMAGE" ]]; then
  echo "refusing unexpected backing image path" >&2
  exit 1
fi
LOOP_SETUP_ATTEMPTED=1
LOOP_CANDIDATE=$(sudo losetup --find --show "$IMAGE")
if [[ ! "$LOOP_CANDIDATE" =~ ^/dev/loop[0-9]+$ ]]; then
  if discovered_loop=$(associated_loop_device); then
    if [[ -n "$discovered_loop" ]]; then
      LOOP_DEVICE="$discovered_loop"
    fi
  fi
  echo "refusing unexpected loop-device path: $LOOP_CANDIDATE" >&2
  exit 1
fi
LOOP_DEVICE="$LOOP_CANDIDATE"
SECTORS=$(sudo blockdev --getsz "$LOOP_DEVICE")
if [[ ! "$SECTORS" =~ ^[0-9]+$ ]] || (( SECTORS < 1 )); then
  echo "could not determine the private loop device size" >&2
  exit 1
fi

sudo dmsetup create "$DM_NAME" --table "0 $SECTORS linear $LOOP_DEVICE 0"
DM_CREATED=1
sudo mkfs.ext4 -F -q "/dev/mapper/$DM_NAME"
sudo mount -o noatime,commit=600 "/dev/mapper/$DM_NAME" "$MOUNT_DIR"
MOUNTED=1
sudo chown "$(id -u):$(id -g)" "$MOUNT_DIR"

python3 - "$WORK/manifests" "$WORK/count.sh" "$WORK/effects.log" <<'PY'
import json
import pathlib
import shlex
import sys

manifests = pathlib.Path(sys.argv[1])
script = pathlib.Path(sys.argv[2])
effects = pathlib.Path(sys.argv[3])
script.write_text(
    "#!/bin/sh\n"
    f"printf '%s\\n' \"$1\" >> {shlex.quote(str(effects))}\n"
    "printf '%s\\n' \"$1\"\n",
    encoding="utf-8",
)
script.chmod(0o700)
manifest = {
    "name": "ctrl.count",
    "binary": "/bin/sh",
    "accepts_stdin": False,
    "cacheable": False,
    "parameters": {"text": {"type": "string", "required": True}},
    "arguments": [
        {"literal": str(script)},
        {"parameter": "text"},
    ],
    "contract": {
        "version": 1,
        "backend": "command",
        "lifecycle": {"kind": "oneshot"},
        "output": {"type": "text", "encoding": "raw", "trim_final_lf": True},
        "outcomes": [{"status": 0, "name": "success", "kind": "success"}],
        "capabilities": [],
        "effects": [],
        "ports": {
            "inputs": {"text": {"schema": {"type": "text"}, "delivery": "single"}},
            "outputs": {"value": {"schema": {"type": "text"}, "delivery": "single"}},
        },
    },
}
(manifests / "ctrl.count.json").write_text(json.dumps(manifest), encoding="utf-8")
PY

PROGRAM="$WORK/workflow.program"
CHECKPOINT="$MOUNT_DIR/checkpoint"
MANIFESTS="$WORK/manifests"
EFFECTS="$WORK/effects.log"
EXECUTE_BIN="$ROOT/target/debug/execute"
printf 'run ctrl.count with text "committed-before-torn-append"\n' > "$PROGRAM"
ulimit -c 0

# The selected log append writes a prefix into the guest filesystem's page cache, then
# pauses before sync_data. The harness switches only the private mapped device to its
# drop-writes target before allowing the process to abort.
set +e
EXECUTE_FAILPOINT=checkpoint.append.write:1:partial-wait:3 \
EXECUTE_FAILPOINT_HOLD_DIR="$HOLD_DIR" \
  "$EXECUTE_BIN" run "$PROGRAM" --checkpoint "$CHECKPOINT" --manifest-dir "$MANIFESTS" \
  >"$WORK/run.stdout" 2>"$WORK/run.stderr" &
RUN_PID=$!
set -e
for _ in {1..3000}; do
  if [[ -e "$HOLD_DIR/ready" ]]; then
    break
  fi
  if ! kill -0 "$RUN_PID" >/dev/null 2>&1; then
    set +e
    wait "$RUN_PID"
    run_status=$?
    set -e
    RUN_PID=""
    cat "$WORK/run.stderr" >&2
    echo "process exited before reaching the partial-write hold (status $run_status)" >&2
    exit 1
  fi
  sleep 0.01
done
if [[ ! -e "$HOLD_DIR/ready" ]]; then
  echo "timed out waiting for process to reach the partial-write hold" >&2
  exit 1
fi
if [[ $(wc -l < "$EFFECTS") -ne 1 ]]; then
  echo "the external effect did not run exactly once before the injected abort" >&2
  exit 1
fi

# Reconfigure only the private loop-backed mapping. During the drop interval every write
# bio emitted through this mapping, including writeback and filesystem journal traffic, is
# silently dropped. Writes that reached the backing image before the switch are retained.
sudo dmsetup suspend "$DM_NAME"
sudo dmsetup reload "$DM_NAME" \
  --table "0 $SECTORS flakey $LOOP_DEVICE 0 0 3600 1 drop_writes"
sudo dmsetup resume "$DM_NAME"

touch "$HOLD_DIR/release"
set +e
wait "$RUN_PID"
run_status=$?
set -e
RUN_PID=""
if (( run_status != 134 )); then
  cat "$WORK/run.stderr" >&2
  echo "expected the partial append failpoint to terminate with SIGABRT (status 134); exit status was $run_status" >&2
  exit 1
fi

# Force every remaining dirty page and filesystem metadata write through dm-flakey.
sync -f "$MOUNT_DIR"
sudo umount "$MOUNT_DIR"
MOUNTED=0

# Remount the same private image through a healthy linear mapping. This tests recovery from
# the retained image after all writes emitted during the drop interval were discarded.
sudo dmsetup suspend "$DM_NAME"
sudo dmsetup reload "$DM_NAME" --table "0 $SECTORS linear $LOOP_DEVICE 0"
sudo dmsetup resume "$DM_NAME"
sudo mount -o noatime,commit=600 "/dev/mapper/$DM_NAME" "$MOUNT_DIR"
MOUNTED=1
sudo chown "$(id -u):$(id -g)" "$MOUNT_DIR"

if "$EXECUTE_BIN" resume "$PROGRAM" --checkpoint "$CHECKPOINT" --manifest-dir "$MANIFESTS" \
  >"$WORK/resume.stdout" 2>"$WORK/resume.stderr"; then
  resume_refused=0
else
  resume_status=$?
  resume_refused=1
  if (( resume_status != 1 )) \
    || grep -qi 'panicked' "$WORK/resume.stderr" \
    || ! grep -Fq 'checkpoint may contain an interrupted side effect; pass --retry-uncertain to retry' "$WORK/resume.stderr"; then
    cat "$WORK/resume.stderr" >&2
    echo "resume did not refuse with the specific interrupted-side-effect uncertainty diagnostic (status $resume_status)" >&2
    exit 1
  fi
fi

if [[ $(wc -l < "$EFFECTS") -ne 1 ]]; then
  cat "$WORK/resume.stderr" >&2
  if (( resume_refused )); then
    echo "resume replayed the external effect before refusing" >&2
  else
    echo "resume replayed an external effect after the checkpoint writeback drop" >&2
  fi
  exit 1
fi

echo "dm-flakey dropped all writes emitted during the checkpoint writeback interval; resume preserved or explicitly refused the in-flight effect"
