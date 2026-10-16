#!/usr/bin/env bash
set -Eeuo pipefail

: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
ARTIFACT_DIR="$RUNNER_TEMP/desktop-ci-artifacts"
mkdir -p "$ARTIFACT_DIR"

{
    date -u +'%Y-%m-%dT%H:%M:%SZ'
    uname -a
    lscpu
    free -h
    df -h "$RUNNER_TEMP"
    printf '\nQEMU version:\n'
    qemu-system-x86_64 --version
    printf '\nRelevant package versions:\n'
    dpkg-query -W -f='${Package} ${Version}\n' \
        qemu-system-gui qemu-system-modules-opengl qemu-system-x86 \
        libvirglrenderer1 xvfb libegl1 libgl1-mesa-dri libglx-mesa0 mesa-utils
    printf '\nQEMU display backends:\n'
    qemu-system-x86_64 -display help
    printf '\nQEMU virtual GPU device models:\n'
    qemu-system-x86_64 -device help
    printf '\nHost render nodes (inventory only):\n'
    find /dev/dri -maxdepth 1 -name 'renderD*' -printf '%p\n' 2>/dev/null || true
} > "$ARTIFACT_DIR/host-preflight.log" 2>&1

xvfb_args='-screen 0 1280x800x24 +extension GLX -nolisten tcp'
LIBGL_ALWAYS_SOFTWARE=true GALLIUM_DRIVER=llvmpipe LP_NUM_THREADS=2 \
    xvfb-run --auto-servernum --server-args="$xvfb_args" \
    glxinfo -B > "$ARTIFACT_DIR/llvmpipe-glx.txt" 2>&1
grep -qi llvmpipe "$ARTIFACT_DIR/llvmpipe-glx.txt"
LIBGL_ALWAYS_SOFTWARE=true GALLIUM_DRIVER=llvmpipe LP_NUM_THREADS=2 \
    xvfb-run --auto-servernum --server-args="$xvfb_args" \
    eglinfo -p x11 -B > "$ARTIFACT_DIR/llvmpipe-egl-x11.txt" 2>&1

set +e
LIBGL_ALWAYS_SOFTWARE=true GALLIUM_DRIVER=llvmpipe LP_NUM_THREADS=2 \
timeout --signal=TERM --kill-after=2s 8s \
    xvfb-run --auto-servernum --server-args="$xvfb_args" \
    qemu-system-x86_64 \
        -name desktop-ci-virgl-preflight \
        -machine q35 \
        -accel tcg,thread=multi \
        -cpu max \
        -smp 1 \
        -m 256 \
        -nodefaults \
        -S \
        -device virtio-vga-gl \
        -display gtk,gl=on,show-menubar=off \
        -monitor none \
        -serial none \
        -no-reboot \
        -no-shutdown \
        > "$ARTIFACT_DIR/qemu-gtk-virgl-preflight.log" 2>&1
probe_status="$?"
set -e
printf 'qemu_preflight_timeout_status=%s\n' "$probe_status" \
    > "$ARTIFACT_DIR/qemu-gtk-virgl-preflight-status.txt"
if [[ "$probe_status" != 124 ]]; then
    cat "$ARTIFACT_DIR/qemu-gtk-virgl-preflight.log" >&2
    echo "QEMU GTK/virgl preflight did not stay alive until its 8-second timeout (status $probe_status)" >&2
    exit 1
fi

printf 'Xvfb GLX llvmpipe and QEMU GTK/OpenGL virtio-vga-gl startup passed.\n' \
    > "$ARTIFACT_DIR/graphics-host-gate.txt"
