#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEST_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TEST_ROOT"' EXIT
FAKE_BIN="$TEST_ROOT/bin"
mkdir -p "$FAKE_BIN"
cat > "$FAKE_BIN/sudo" <<'SH'
#!/usr/bin/env bash
set -u
printf '%s\n' "$*" >> "$CLEANUP_LOG"
if [[ "$1" == losetup && "$2" == --list ]]; then
  printf '%s\n' "${CLEANUP_ASSOCIATED_DEVICES:-}"
  exit 0
fi
if [[ -n "${CLEANUP_FAIL_PREFIX:-}" && "$*" == "$CLEANUP_FAIL_PREFIX"* ]]; then
  exit 23
fi
SH
chmod +x "$FAKE_BIN/sudo"
source "$ROOT/tools/crash_consistency_dm_flakey_cleanup.sh"

run_case() {
  local name=$1 prior_status=$2 fail_prefix=$3 mounted=$4 dm_created=$5
  local loop_device=$6 setup_attempted=$7 associated=$8 expected_status=$9 preserve=${10}
  local work="$TEST_ROOT/$name"
  mkdir -p "$work/mnt"
  local log="$TEST_ROOT/$name.commands"
  : > "$log"

  local actual_status=0
  set +e
  (
    PATH="$FAKE_BIN:$PATH"
    export PATH
    CLEANUP_LOG="$log"
    CLEANUP_FAIL_PREFIX="$fail_prefix"
    CLEANUP_ASSOCIATED_DEVICES="$associated"
    export CLEANUP_LOG CLEANUP_FAIL_PREFIX CLEANUP_ASSOCIATED_DEVICES
    WORK="$work"
    MOUNT_DIR="$work/mnt"
    IMAGE="$work/checkpoint.img"
    DM_NAME="test-$name"
    LOOP_DEVICE="$loop_device"
    LOOP_SETUP_ATTEMPTED="$setup_attempted"
    MOUNTED="$mounted"
    DM_CREATED="$dm_created"
    RUN_PID=""
    trap cleanup EXIT
    exit "$prior_status"
  )
  actual_status=$?
  set -e

  if (( actual_status != expected_status )); then
    echo "$name: expected exit $expected_status, got $actual_status" >&2
    return 1
  fi
  if [[ "$preserve" == yes && ! -d "$work" ]]; then
    echo "$name: cleanup failure removed the scratch directory" >&2
    return 1
  fi
  if [[ "$preserve" == no && -e "$work" ]]; then
    echo "$name: successful cleanup left the scratch directory" >&2
    return 1
  fi
}

run_case successful-cleanup 0 "" 1 1 /dev/loop9 1 "" 0 no
run_case unmount-failure 0 umount 1 1 /dev/loop9 1 "" 1 yes
run_case mapping-failure 0 "dmsetup remove" 0 1 /dev/loop9 1 "" 1 yes
run_case loop-failure 0 "losetup --detach" 0 0 /dev/loop9 1 "" 1 yes
run_case preserve-original-failure-successful-cleanup 7 "" 0 0 "" 1 "" 7 no
run_case preserve-original-failure 7 "dmsetup remove" 0 1 /dev/loop9 1 "" 7 yes
run_case ambiguous-associated-loop 0 "" 0 0 "" 1 $'/dev/loop2\n/dev/loop3' 1 yes
run_case no-associated-loop 0 "" 0 0 "" 1 "" 0 no

echo "dm-flakey cleanup status and scratch-preservation checks passed"
