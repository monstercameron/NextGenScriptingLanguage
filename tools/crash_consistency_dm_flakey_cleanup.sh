# Sourced by the dm-flakey harness and its nonprivileged cleanup regression.
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

cleanup() {
  local exit_status=$?
  set +e
  if [[ -n "$RUN_PID" ]]; then
    kill "$RUN_PID" >/dev/null 2>&1
    wait "$RUN_PID" >/dev/null 2>&1
    RUN_PID=""
  fi
  if (( MOUNTED )); then
    if ! sudo umount "$MOUNT_DIR"; then
      echo "could not unmount the private test filesystem; preserving $WORK for runner cleanup" >&2
      (( exit_status != 0 )) || exit_status=1
      exit "$exit_status"
    fi
    MOUNTED=0
  fi
  if (( DM_CREATED )); then
    if ! sudo dmsetup remove "$DM_NAME"; then
      echo "could not remove the private device-mapper target; preserving $WORK" >&2
      (( exit_status != 0 )) || exit_status=1
      exit "$exit_status"
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
      (( exit_status != 0 )) || exit_status=1
      exit "$exit_status"
    fi
  fi
  if [[ -n "$LOOP_DEVICE" ]]; then
    if ! sudo losetup --detach "$LOOP_DEVICE"; then
      echo "could not detach the private loop device; preserving $WORK" >&2
      (( exit_status != 0 )) || exit_status=1
      exit "$exit_status"
    fi
  fi
  if ! rm -rf -- "$WORK"; then
    echo "could not remove the private test directory: $WORK" >&2
    (( exit_status != 0 )) || exit_status=1
  fi
  exit "$exit_status"
}
