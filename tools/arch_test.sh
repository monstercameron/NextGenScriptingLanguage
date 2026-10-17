#!/bin/sh
# Builds the rendered AUR packages in an Arch container, installs each with pacman, and
# smoke-tests the installed tree. Run as root inside archlinux:latest with the repository
# mounted at /work and tools/aur.py output in /work/pkgs.
set -eu

pacman -Syu --noconfirm --needed base-devel rust python >/dev/null
id builder >/dev/null 2>&1 || useradd -m builder
want=$(python3 /work/tools/release.py version)

build() {
    # $1: package directory under /work/pkgs. makepkg runs as an unprivileged user in a
    # clean copy, so it cannot write into the checkout.
    rm -rf /home/builder/build
    mkdir /home/builder/build
    cp -r "/work/pkgs/$1/." /home/builder/build/
    chown -R builder: /home/builder/build
    su - builder -c 'cd /home/builder/build && makepkg --printsrcinfo > .SRCINFO && makepkg -f --noconfirm'
}

smoke() {
    # The installed tree must be complete and the binary must find its manifests in
    # /usr/share/execute/manifests, with no --manifest-dir.
    test "$(execute --version)" = "execute $want"
    test -f /usr/share/execute/manifests/printf.json
    test -f /usr/share/bash-completion/completions/execute
    test -f /usr/share/zsh/site-functions/_execute
    test -f /usr/share/fish/vendor_completions.d/execute.fish
    test -f /usr/share/man/man1/execute.1
    test -f /usr/share/execute/editors/vscode/syntaxes/execute.tmLanguage.json
    test -f /usr/share/execute/docs/language.md
    command -v execute-applet >/dev/null
    workdir=$(mktemp -d)
    cd "$workdir"
    printf '%s\n' 'run printf with message "installed"' > smoke.program
    test "$(execute run smoke.program)" = installed
    execute check smoke.program >/dev/null
    cd /
    rm -rf "$workdir"
}

build execute-bin
pacman -U --noconfirm /home/builder/build/execute-bin-*.pkg.tar.*
smoke
pacman -Rns --noconfirm execute-bin

build execute
pacman -U --noconfirm /home/builder/build/execute-[0-9]*.pkg.tar.*
smoke
pacman -Rns --noconfirm execute

echo "Arch packages built, installed, and smoke-tested."
