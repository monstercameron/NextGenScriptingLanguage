#!/usr/bin/env python3
"""Deterministic tag scanner for Omarchy migration scripts (issue #384, stage 1).

Reads shell migration scripts without running them and emits a closed set of
token-level tags per file, plus a risk floor derived from those tags. Tags are
facts about what a script's commands name (command words, flags, write targets);
they are not judgments. A later model stage may raise a risk class above the
floor but must never lower it.

Closed tag set:
  sudo, pam, boot, systemd-unit, reboot-required, package-install,
  package-remove, rm-recursive, network, writes:/etc, writes:/boot,
  writes:/usr, writes:/var, writes:/opt, user-config:~/<path>

Standard library only. Output is JSON keyed by each file's SHA-256.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shlex
import sys

TOP_DIRS = ("/etc", "/boot", "/usr", "/var", "/opt")
BOOT_COMMANDS = {
    "mkinitcpio", "dracut", "limine", "limine-update", "grub-mkconfig",
    "grub-install", "update-grub", "efibootmgr", "bootctl", "ukify",
}
WRAPPERS = {"sudo", "doas", "env", "exec", "nohup", "time", "command", "xargs"}
PACKAGE_INSTALL_COMMANDS = {"omarchy-pkg-add", "yay", "paru"}
PACKAGE_REMOVE_COMMANDS = {"omarchy-pkg-drop"}
NETWORK_COMMANDS = {"curl", "wget"}
GIT_NETWORK_SUBCOMMANDS = {"clone", "fetch", "pull"}
# Every non-option operand is written.
ALL_ARG_WRITERS = {"tee", "touch", "mkdir", "rm", "rmdir", "truncate"}
# The last non-option operand is written.
LAST_ARG_WRITERS = {"cp", "mv", "install", "ln"}
REDIRECT_WRITE = {">", ">>", ">|", "&>", "&>>"}
REDIRECT_OTHER = {"<", "<<", "<<<", "<<-", ">&", "<&"}
OPERATOR_TOKENS = {";", ";;", "&&", "||", "|", "&", "(", ")"}
HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")
ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


def _logical_lines(text):
    """Join lines that end in a backslash continuation."""
    buf = ""
    for raw in text.splitlines():
        if raw.rstrip().endswith("\\"):
            buf += raw.rstrip()[:-1] + " "
            continue
        yield buf + raw
        buf = ""
    if buf:
        yield buf


def _tokenize(line):
    lex = shlex.shlex(line, posix=True, punctuation_chars=True)
    lex.whitespace_split = True
    lex.commenters = ""
    try:
        tokens = list(lex)
    except ValueError:
        tokens = line.split()
    for index, token in enumerate(tokens):
        if token.startswith("#"):
            return tokens[:index]
    return tokens


def _segments(tokens):
    segment = []
    for token in tokens:
        if token in OPERATOR_TOKENS:
            if segment:
                yield segment
            segment = []
        else:
            segment.append(token)
    if segment:
        yield segment


def _commands(text):
    """Yield token segments for each simple command, skipping comments and heredoc bodies."""
    delimiter = None
    for line in _logical_lines(text):
        if delimiter is not None:
            if line.strip() == delimiter:
                delimiter = None
            continue
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        heredoc = HEREDOC.search(line)
        if heredoc:
            delimiter = heredoc.group(2)
        yield from _segments(_tokenize(line))


def _path_tags(raw, tags):
    """Add write-target and user-config tags for a path a command writes."""
    path = raw.replace("${HOME}", "~").replace("$HOME", "~")
    if "$" in path:
        return
    for top in TOP_DIRS:
        if path == top or path.startswith(top + "/"):
            tags.add("writes:" + top)
            break
    if path == "/boot" or path.startswith("/boot/") or path == "/efi" or path.startswith("/efi/"):
        tags.add("boot")
    if path.startswith("~/"):
        parts = [part for part in path[2:].split("/") if part]
        if parts:
            depth = 2 if parts[0] in (".config", ".local") else 1
            tags.add("user-config:~/" + "/".join(parts[:depth]))


def _sed_inplace(options):
    return any(
        (opt.startswith("-") and not opt.startswith("--") and "i" in opt)
        or opt.startswith("--in-place")
        for opt in options
    )


def _analyze(segment, tags):
    words = []
    index = 0
    while index < len(segment):
        token = segment[index]
        if token in REDIRECT_WRITE and index + 1 < len(segment):
            _path_tags(segment[index + 1], tags)
            index += 2
            continue
        if token in REDIRECT_OTHER:
            index += 2
            continue
        words.append(token)
        index += 1

    if any(
        "pam.d" in word or re.search(r"\bpam_", word) for word in words
    ):
        tags.add("pam")
    while words and ASSIGNMENT.match(words[0]):
        words.pop(0)
    if "sudo" in words:
        tags.add("sudo")
    while words and words[0] in WRAPPERS:
        words.pop(0)
        while words and words[0].startswith("-"):
            words.pop(0)
    if not words:
        return

    cmd, args = words[0], words[1:]
    options = [arg for arg in args if arg.startswith("-") and arg != "-"]
    operands = [arg for arg in args if not (arg.startswith("-") and arg != "-")]

    if cmd in BOOT_COMMANDS:
        tags.add("boot")
    if cmd == "systemctl":
        tags.add("systemd-unit")
        if "reboot" in operands:
            tags.add("reboot-required")
    if cmd == "reboot":
        tags.add("reboot-required")
    if cmd == "pacman":
        if any(opt.startswith("--remove") or (not opt.startswith("--") and "R" in opt) for opt in options):
            tags.add("package-remove")
        if any(opt.startswith("--sync") or (not opt.startswith("--") and "S" in opt) for opt in options):
            tags.add("package-install")
    if cmd in PACKAGE_REMOVE_COMMANDS:
        tags.add("package-remove")
    if cmd in PACKAGE_INSTALL_COMMANDS:
        tags.add("package-install")
    if cmd == "rm" and any(
        opt == "--recursive" or (not opt.startswith("--") and ("r" in opt or "R" in opt))
        for opt in options
    ):
        tags.add("rm-recursive")
    if cmd in NETWORK_COMMANDS or (cmd == "git" and operands and operands[0] in GIT_NETWORK_SUBCOMMANDS):
        tags.add("network")

    if cmd in ALL_ARG_WRITERS:
        targets = operands
    elif cmd == "sed" and _sed_inplace(options):
        targets = operands[1:]
    elif cmd in ("chmod", "chown"):
        targets = operands[1:]
    elif cmd in LAST_ARG_WRITERS:
        targets = operands[-1:]
    elif cmd == "dd":
        targets = [op[3:] for op in operands if op.startswith("of=")]
    else:
        targets = []
    for target in targets:
        _path_tags(target, tags)


def scan_text(text):
    """Return the sorted tag list for one migration script's text."""
    tags = set()
    for segment in _commands(text):
        _analyze(segment, tags)
    return sorted(tags)


def risk_floor(tags):
    """Deterministic risk class implied by tags. A model stage may raise it, never lower it."""
    if "boot" in tags or "pam" in tags:
        return "boot-or-auth-sensitive"
    if "package-remove" in tags or "package-install" in tags:
        return "package-change"
    if any(tag.startswith("writes:") for tag in tags):
        return "system-config"
    if any(tag.startswith("user-config:") for tag in tags):
        return "user-config-only"
    return "none"


def review_record(path):
    data = Path(path).read_bytes()
    tags = scan_text(data.decode("utf-8", errors="replace"))
    return {
        "name": Path(path).name,
        "sha256": hashlib.sha256(data).hexdigest(),
        "tags": tags,
        "risk_floor": risk_floor(tags),
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit deterministic migration tags as JSON keyed by SHA-256.")
    parser.add_argument("paths", nargs="+", type=Path, help="migration .sh files or directories of them")
    args = parser.parse_args(argv)
    records = {}
    for path in args.paths:
        files = sorted(path.glob("*.sh")) if path.is_dir() else [path]
        for file_path in files:
            record = review_record(file_path)
            records[record["sha256"]] = record
    json.dump(records, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
