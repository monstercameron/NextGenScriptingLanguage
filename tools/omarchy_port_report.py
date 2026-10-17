#!/usr/bin/env python3
"""Classify Omarchy Bash commands by the Execute features they need.

The lexer follows Bash tokenization closely enough for port planning: single,
double and ANSI quotes; backslash escapes; `$(...)`, backticks and `${...}`
nesting; here-documents with their bodies; process substitution; `((...))`;
array assignments; and `[[ ... ]]` conditions. A light parser then attributes
features to commands. It is not a complete Bash parser: `case` patterns inside a
`$(...)` with unbalanced parentheses are approximated, and case patterns are
seen as command names. Standard library only.

Inputs:
  --inventory  the pinned inventory (docs/omarchy-inventory.json)
  --fixtures   directory of upstream scripts (default: the v4.0.4 fixture bin/)
  --checkout   optional local Omarchy checkout; its bin/ scripts are analyzed too
  --manifests  Execute manifest directory, used to mark commands that have one

Outputs a JSON report (--json PATH) and a summary table on stdout.
"""

from __future__ import annotations

import argparse
import bisect
import hashlib
import json
import os
import re
import sys
from dataclasses import dataclass, field

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_INVENTORY = os.path.join(ROOT, "docs", "omarchy-inventory.json")
DEFAULT_FIXTURES = os.path.join(ROOT, "tests", "fixtures", "omarchy-v4.0.4", "bin")
DEFAULT_MANIFESTS = os.path.join(ROOT, "manifests")

# Feature -> (category, what Execute offers instead). "unsupported" blocks a direct
# port, "partial" needs an applet or a rewrite, "supported" maps onto language forms.
FEATURES = {
    "heredoc": ("unsupported", "no here-document syntax; use a file or a program input"),
    "process_substitution": ("unsupported", "no process substitution; use separate programs"),
    "trap": ("partial", "on exit and on interrupt cover EXIT and INT cleanup; other signals have no equivalent"),
    "eval": ("unsupported", "no string evaluation; write the command as a program"),
    "source": ("partial", "run program file runs another file in process with explicit inputs; shared variables do not cross, and KEY=VALUE env files load with env.load"),
    "exec_builtin": ("partial", "exec replaces the shell; a final exec ports to hand over to, which must be the last statement"),
    "coproc": ("unsupported", "no coprocesses; use listen or stream forms"),
    "background_job": ("partial", "spawn runs owned named jobs; launch detached starts an unowned process; & has no direct equivalent"),
    "associative_array": ("unsupported", "declare -A has no equivalent"),
    "regex_match": ("partial", "needs a text regex applet; Execute compares text exactly"),
    "read_builtin": ("partial", "needs accept or listen input parsing"),
    "getopts": ("partial", "needs an argument parser; use accept with arguments"),
    "array_assign": ("partial", "needs list values: accept as list of text or a list literal"),
    "array_expansion": ("partial", "needs list values: accept as list of text"),
    "parameter_expansion": ("partial", "needs text applets (default, strip, replace)"),
    "arithmetic": ("partial", "needs number applets"),
    "local_variable": ("partial", "locals are per reusable program, not per shell function"),
    "ansi_quote": ("partial", "needs literal text; ANSI escape rules differ"),
    "command_substitution": ("supported", "capture with as name"),
    "function_definition": ("supported", "reusable programs"),
    "loop": ("supported", "for each, repeat or while"),
    "conditional": ("supported", "if and otherwise"),
    "pipeline": ("supported", "pipe to or then"),
    "redirection": ("supported", "then save, discard stderr or direct argv"),
    "positional_arguments": ("supported", "accept arguments as list of text from arguments"),
    "environment_variable": ("supported", "accept from environment"),
    "privileged_command": ("supported", "declare effects and capabilities in a manifest"),
}

PRIVILEGED = {
    "sudo", "pkexec", "doas", "systemctl", "pacman", "yay", "paru", "apt", "apt-get",
    "dnf", "flatpak", "snap", "mount", "umount", "chown", "chmod",
}
SHELL_BUILTIN_FEATURES = {
    "read": "read_builtin",
    "getopts": "getopts",
    "local": "local_variable",
    "trap": "trap",
    "eval": "eval",
    "source": "source",
    ".": "source",
    "exec": "exec_builtin",
    "coproc": "coproc",
}
NON_EXTERNAL = {
    "declare", "typeset", "printf", "echo", "test", "[", "true", "false", "cd", "set",
    "shift", "export", "unset", "return", "exit", "break", "continue", "wait", "jobs",
    "kill", "pwd", "let", "umask", "alias", "unalias", "readonly", "shopt",
}
# Longest first, so `<<<` and `<<-` are not read as `<<`.
OPERATORS = [
    "&>>", "<<<", "<<-", ";;", "&&", "||", "|&", ">>", "<<", "<&", ">&", "<>", ">|",
    "&>", "|", "&", ";", "(", ")", "<", ">",
]
REDIRECTIONS = {"&>>", "<<<", "<<-", ">>", "<<", "<&", ">&", "<>", ">|", "&>", "<", ">"}
COMMAND_SEPARATORS = {";", ";;", "&&", "||", "|", "|&", "&", "(", ")"}
COMMAND_OPENERS = {"then", "do", "else", "elif", "if", "while", "until", "!", "{", "time"}
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?\+?=")


class LexError(Exception):
    def __init__(self, line: int, message: str):
        super().__init__(f"line {line}: {message}")
        self.line = line


@dataclass
class Token:
    kind: str  # WORD, OP, NEWLINE, PROCSUB, ARITH, EOF
    text: str
    line: int
    features: set = field(default_factory=set)
    details: list = field(default_factory=list)


class Lexer:
    def __init__(self, source: str):
        self.s = source
        self.i = 0
        self.tokens: list[Token] = []
        self.pending: list[tuple[Token, str, bool, bool]] = []
        self._newlines = [i for i, c in enumerate(source) if c == "\n"]

    def _line(self, pos: int) -> int:
        """One-based line of a character position, independent of scan order."""
        return bisect.bisect_left(self._newlines, pos) + 1

    def run(self) -> list[Token]:
        while True:
            self._skip_blanks()
            if self.i >= len(self.s):
                break
            c = self.s[self.i]
            if c == "\n":
                self._newline()
                continue
            if c == "#":
                self._comment()
                continue
            if c in "<>" and self.s.startswith("(", self.i + 1):
                self._procsub()
                continue
            if self.s.startswith("((", self.i) and self._command_start():
                start = self.i
                end = self._balanced(self.i, "(", ")")
                tok = Token("ARITH", self.s[start:end], self._line(start))
                tok.features.add("arithmetic")
                self.tokens.append(tok)
                self.i = end
                continue
            op = self._match_operator()
            if op:
                self.tokens.append(Token("OP", op, self._line(self.i)))
                self.i += len(op)
                continue
            self._word()
        self.tokens.append(Token("EOF", "", self._line(len(self.s))))
        for tok, delimiter, _strip, quoted in self.pending:
            tok.details.append({"heredoc": delimiter, "quoted": quoted, "body_lines": 0, "unterminated": True})
        self.pending = []
        return self.tokens

    def _command_start(self) -> bool:
        """True where a command or an arithmetic command may begin."""
        if not self.tokens:
            return True
        last = self.tokens[-1]
        if last.kind in ("NEWLINE", "EOF"):
            return True
        if last.kind == "OP" and last.text in COMMAND_SEPARATORS:
            return True
        # `if ((...))`, `while ((...))` and `then ((...))` open an arithmetic command.
        return last.kind == "WORD" and last.text in ("if", "elif", "while", "until", "then", "do", "else")

    def _skip_blanks(self) -> None:
        while self.i < len(self.s):
            c = self.s[self.i]
            if c in " \t":
                self.i += 1
            elif c == "\\" and self.s.startswith("\n", self.i + 1):
                self.i += 2
            else:
                break

    def _comment(self) -> None:
        end = self.s.find("\n", self.i)
        self.i = len(self.s) if end < 0 else end

    def _match_operator(self) -> str | None:
        for op in OPERATORS:
            if self.s.startswith(op, self.i):
                return op
        return None

    def _newline(self) -> None:
        self.tokens.append(Token("NEWLINE", "\n", self._line(self.i)))
        self.i += 1
        # Here-document bodies start on the line after the operator and end at a
        # line equal to the delimiter (after tab stripping for `<<-`).
        for tok, delimiter, strip_tabs, quoted in self.pending:
            body_lines = 0
            while self.i < len(self.s):
                end = self.s.find("\n", self.i)
                end = len(self.s) if end < 0 else end
                text = self.s[self.i:end]
                self.i = min(end + 1, len(self.s))
                body_lines += 1
                candidate = text.lstrip("\t") if strip_tabs else text
                if candidate == delimiter:
                    break
            tok.details.append({"heredoc": delimiter, "quoted": quoted, "body_lines": body_lines})
        self.pending = []

    def _procsub(self) -> None:
        start = self.i
        end = self._balanced(self.i + 1, "(", ")")
        tok = Token("PROCSUB", self.s[start:end], self._line(start))
        tok.features.add("process_substitution")
        self.tokens.append(tok)
        self.i = end

    def _balanced(self, start: int, open_ch: str, close_ch: str) -> int:
        """Return the index just past the group that opens at or after `start`."""
        i = start
        while i < len(self.s) and self.s[i] != open_ch:
            i += 1
        if i >= len(self.s):
            raise LexError(self._line(start), f"expected {open_ch!r}")
        depth = 0
        while i < len(self.s):
            c = self.s[i]
            if c == "\\":
                i += 2
                continue
            if c == "'":
                end = self.s.find("'", i + 1)
                if end < 0:
                    raise LexError(self._line(i), "unterminated single quote")
                i = end + 1
                continue
            if c == '"':
                i = self._double_end(i)
                continue
            if c == "#" and (i == 0 or self.s[i - 1] in " \t\n;"):
                end = self.s.find("\n", i)
                i = len(self.s) if end < 0 else end
                continue
            if c == open_ch:
                depth += 1
            elif c == close_ch:
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        raise LexError(self._line(start), f"unterminated {open_ch!r} group")

    def _double_end(self, start: int) -> int:
        i = start + 1
        while i < len(self.s):
            c = self.s[i]
            if c == "\\":
                i += 2
                continue
            if c == '"':
                return i + 1
            if c == "$" and self.s[i + 1:i + 2] == "(":
                i = self._balanced(i + 1, "(", ")")
                continue
            if c == "`":
                i = self._backtick_end(i)
                continue
            i += 1
        raise LexError(self._line(start), "unterminated double quote")

    def _backtick_end(self, start: int) -> int:
        i = start + 1
        while i < len(self.s):
            if self.s[i] == "\\":
                i += 2
                continue
            if self.s[i] == "`":
                return i + 1
            i += 1
        raise LexError(self._line(start), "unterminated backtick")

    def _word(self) -> None:
        start = self.i
        tok = Token("WORD", "", self._line(start))
        chars: list[str] = []
        quoted = False
        while self.i < len(self.s):
            c = self.s[self.i]
            if c in " \t\n;&|()<>":
                break
            if c == "\\":
                if self.s.startswith("\n", self.i + 1):
                    self.i += 2
                    continue
                chars.append(self.s[self.i:self.i + 2])
                self.i += 2
                continue
            if c == "'":
                end = self.s.find("'", self.i + 1)
                if end < 0:
                    raise LexError(self._line(self.i), "unterminated single quote")
                chars.append(self.s[self.i:end + 1])
                self.i = end + 1
                quoted = True
                continue
            if c == '"':
                end = self._double_end(self.i)
                self._scan_dollars(self.i + 1, end - 1, tok)
                chars.append(self.s[self.i:end])
                self.i = end
                quoted = True
                continue
            if c == "`":
                end = self._backtick_end(self.i)
                tok.features.add("command_substitution")
                chars.append(self.s[self.i:end])
                self.i = end
                continue
            if c == "$":
                end = self._dollar_end(self.i, tok)
                if self.s[self.i + 1:self.i + 2] in ("'", '"'):
                    quoted = True
                chars.append(self.s[self.i:end])
                self.i = end
                continue
            if c == "=" and self.s.startswith("(", self.i + 1):
                prefix = "".join(chars)
                if ASSIGNMENT.fullmatch(prefix + "=") and not prefix.endswith("="):
                    end = self._balanced(self.i + 1, "(", ")")
                    tok.features.add("array_assign")
                    chars.append(self.s[self.i:end])
                    self.i = end
                    break
            chars.append(c)
            self.i += 1
        if self.i == start:
            # Defensive: never loop on a character the word rules do not consume.
            chars = [self.s[start]]
            self.i = start + 1
        tok.text = "".join(chars)
        prev = self.tokens[-1] if self.tokens else None
        if prev is not None and prev.kind == "OP" and prev.text in ("<<", "<<-"):
            delimiter = re.sub(r"[\"'\\]", "", tok.text)
            self.pending.append((tok, delimiter, prev.text == "<<-", quoted or "\\" in tok.text))
            tok.features.add("heredoc")
        self.tokens.append(tok)

    def _scan_dollars(self, start: int, stop: int, tok: Token) -> None:
        """Record expansions inside a double-quoted segment without changing its text."""
        i = start
        while i < stop:
            if self.s[i] == "$":
                i = min(self._dollar_end(i, tok), stop)
            else:
                i += 1

    def _dollar_end(self, start: int, tok: Token) -> int:
        """End index of a `$` expansion that begins at `start`."""
        nxt = self.s[start + 1:start + 2]
        if nxt == "(":
            if self.s[start + 2:start + 3] == "(":
                tok.features.add("arithmetic")
            else:
                tok.features.add("command_substitution")
            end = self._balanced(start + 1, "(", ")")
            # The body is shell text too, so its own expansions are features.
            self._scan_dollars(start + 2, end - 1, tok)
            return end
        if nxt == "{":
            tok.features.add("parameter_expansion")
            end = self._balanced(start + 1, "{", "}")
            if re.search(r"\[[@*]\]", self.s[start + 2:end - 1]):
                tok.features.add("array_expansion")
            return end
        if nxt == "'":
            end = start + 2
            while end < len(self.s) and self.s[end] != "'":
                end += 2 if self.s[end] == "\\" else 1
            tok.features.add("ansi_quote")
            return min(end + 1, len(self.s))
        if nxt == '"':
            return self._double_end(start + 1)
        match = NAME.match(self.s, start + 1)
        if match:
            name = match.group(0)
            if re.search(r"[A-Z]", name):
                tok.details.append({"environment": name})
                tok.features.add("environment_variable")
            return match.end()
        if nxt and (nxt.isdigit() or nxt in "@*"):
            tok.features.add("positional_arguments")
            return start + 2
        if nxt == "#":
            tok.features.add("positional_arguments")
            return start + 2
        if nxt:
            return start + 2
        return start + 1


def tokenize(source: str) -> list[Token]:
    return Lexer(source).run()


def analyze_source(source: str) -> dict:
    """Return features, commands and a verdict for one shell script."""
    tokens = tokenize(source)
    features: list[dict] = []

    def note(feature: str, line: int, detail: str = "") -> None:
        features.append({"feature": feature, "line": line, "detail": detail})

    for tok in tokens:
        for feature in sorted(tok.features):
            note(feature, tok.line, tok.text[:60])
        if tok.kind == "OP" and tok.text in REDIRECTIONS:
            note("redirection", tok.line, tok.text)
        if tok.kind == "OP" and tok.text == "&":
            note("background_job", tok.line, "&")
        if tok.kind == "OP" and tok.text == "|":
            note("pipeline", tok.line, "|")

    commands: list[dict] = []
    command_start = True
    index = 0
    n = len(tokens)
    while index < n:
        tok = tokens[index]
        if tok.kind in ("NEWLINE", "EOF"):
            command_start = True
            index += 1
            continue
        if tok.kind == "OP":
            command_start = tok.text in COMMAND_SEPARATORS
            index += 1
            continue
        if tok.kind in ("PROCSUB", "ARITH"):
            command_start = False
            index += 1
            continue
        word = tok.text
        if command_start and word in ("if", "elif", "while", "until", "case"):
            note("conditional" if word in ("if", "elif", "case") else "loop", tok.line, word)
            command_start = word != "case"
            index += 1
            continue
        if command_start and word in ("for", "select"):
            note("loop", tok.line, word)
            command_start = False
            index += 1
            continue
        if command_start and word == "[[":
            note("conditional", tok.line, "[[")
            j = index + 1
            while j < n and tokens[j].kind != "EOF" and not (tokens[j].kind == "WORD" and tokens[j].text == "]]"):
                if tokens[j].kind == "WORD" and tokens[j].text == "=~":
                    note("regex_match", tokens[j].line, "=~")
                j += 1
            index = j + 1
            command_start = False
            continue
        if command_start and word == "function":
            note("function_definition", tok.line, "function keyword")
            command_start = False
            index += 1
            continue
        if command_start and word in COMMAND_OPENERS:
            command_start = True
            index += 1
            continue
        if command_start and word == "exec":
            # `exec CMD ARGS` replaces the shell with CMD, so CMD is the command.
            note("exec_builtin", tok.line, "exec")
            command_start = True
            index += 1
            continue
        if command_start and word in ("esac", "in", "fi", "done", "}"):
            command_start = word in ("in", "}")
            index += 1
            continue
        if (
            command_start
            and index + 2 < n
            and tokens[index + 1].kind == "OP"
            and tokens[index + 1].text == "("
            and tokens[index + 2].kind == "OP"
            and tokens[index + 2].text == ")"
            and NAME.fullmatch(word)
        ):
            note("function_definition", tok.line, word)
            index += 3
            command_start = True
            continue
        if command_start and ASSIGNMENT.match(word):
            index += 1
            continue
        if command_start and tok.kind == "WORD":
            commands.append({"name": word, "line": tok.line})
            if word in PRIVILEGED:
                note("privileged_command", tok.line, word)
            if word in SHELL_BUILTIN_FEATURES:
                note(SHELL_BUILTIN_FEATURES[word], tok.line, word)
            if word in ("declare", "typeset"):
                j = index + 1
                flags = []
                while j < n and tokens[j].kind == "WORD" and tokens[j].text.startswith("-"):
                    flags.append(tokens[j].text)
                    j += 1
                if any("A" in flag for flag in flags):
                    note("associative_array", tok.line, " ".join(flags))
                if any("a" in flag for flag in flags):
                    note("array_assign", tok.line, "declare -a")
            command_start = False
            index += 1
            continue
        command_start = False
        index += 1

    external = sorted({c["name"] for c in commands
                       if c["name"] not in SHELL_BUILTIN_FEATURES and c["name"] not in NON_EXTERNAL})
    categories = {"unsupported": 0, "partial": 0, "supported": 0}
    for item in features:
        categories[FEATURES[item["feature"]][0]] += 1
    if categories["unsupported"]:
        verdict = "blocked"
    elif categories["partial"]:
        verdict = "partial"
    else:
        verdict = "portable"
    return {
        "verdict": verdict,
        "counts": categories,
        "features": features,
        "feature_names": sorted({item["feature"] for item in features}),
        "external_commands": external,
    }


def manifest_binaries(manifest_dir: str) -> set[str]:
    binaries: set[str] = set()
    if not os.path.isdir(manifest_dir):
        return binaries
    for name in os.listdir(manifest_dir):
        if not name.endswith(".json"):
            continue
        try:
            with open(os.path.join(manifest_dir, name), encoding="utf-8") as handle:
                data = json.load(handle)
        except (OSError, ValueError):
            continue
        binary = data.get("binary") if isinstance(data, dict) else None
        if isinstance(binary, str):
            binaries.add(os.path.basename(binary))
    return binaries


def analyze_file(path: str, expected_sha: str | None, manifests: set[str]) -> dict:
    with open(path, "rb") as handle:
        raw = handle.read()
    digest = hashlib.sha256(raw).hexdigest()
    decoded = raw.decode("utf-8", errors="replace")
    # Checked-out Windows fixtures carry CRLF. Bash on Linux would see the CR as
    # part of every word, so normalize before lexing and say so in the report.
    crlf = "\r\n" in decoded
    source = decoded.replace("\r\n", "\n")
    result: dict = {
        "sha256": digest,
        "sha256_matches_inventory": (expected_sha == digest) if expected_sha else None,
        "line_endings": "crlf_normalized" if crlf else "lf",
        "lines": source.count("\n") + (0 if source.endswith("\n") or not source else 1),
    }
    try:
        result.update(analyze_source(source))
        result["analysis"] = "lexed"
    except LexError as error:
        result.update({
            "analysis": "lex_error",
            "error": str(error),
            "verdict": "blocked",
            "counts": {},
            "features": [],
            "feature_names": [],
            "external_commands": [],
        })
    result["external_commands"] = [
        {"name": name, "manifest": name in manifests} for name in result.get("external_commands", [])
    ]
    return result


def build_report(args: argparse.Namespace) -> dict:
    with open(args.inventory, encoding="utf-8") as handle:
        inventory = json.load(handle)
    manifests = manifest_binaries(args.manifests)
    inventory_paths = {entry["path"]: entry for entry in inventory["commands"]}
    scripts: dict[str, dict] = {}

    def add(directory: str, label: str) -> None:
        if not os.path.isdir(directory):
            return
        for name in sorted(os.listdir(directory)):
            full = os.path.join(directory, name)
            if not os.path.isfile(full):
                continue
            key = f"bin/{name}"
            expected = inventory_paths.get(key, {}).get("sha256")
            analyzed = analyze_file(full, expected, manifests)
            analyzed["source"] = label
            analyzed["in_inventory"] = key in inventory_paths
            scripts[key] = analyzed

    add(args.fixtures, "fixture")
    if args.checkout:
        add(os.path.join(args.checkout, "bin"), "checkout")

    rows = []
    for entry in inventory["commands"]:
        path = entry["path"]
        if path in scripts:
            rows.append({"path": path, "status": "analyzed", "verdict": scripts[path]["verdict"]})
        else:
            rows.append({"path": path, "status": "no_source_available", "verdict": None})

    verdicts = {"portable": 0, "partial": 0, "blocked": 0}
    feature_scripts: dict[str, int] = {}
    for item in scripts.values():
        verdicts[item["verdict"]] = verdicts.get(item["verdict"], 0) + 1
        for feature in item.get("feature_names", []):
            feature_scripts[feature] = feature_scripts.get(feature, 0) + 1

    analyzed_inventory = sum(1 for row in rows if row["status"] == "analyzed")
    return {
        "tool": "tools/omarchy_port_report.py",
        "inventory": {
            "release_tag": inventory["release_tag"],
            "commit": inventory["commit"],
            "command_count": inventory["command_count"],
        },
        "method": "Bash-style tokenization with a light command parser; approximate for nested case patterns.",
        "coverage": {
            "inventory_commands": len(inventory["commands"]),
            "analyzed": analyzed_inventory,
            "no_source_available": len(inventory["commands"]) - analyzed_inventory,
            "extra_sources": sum(1 for item in scripts.values() if not item["in_inventory"]),
        },
        "verdicts": verdicts,
        "feature_scripts": dict(sorted(feature_scripts.items())),
        "feature_categories": {name: FEATURES[name][0] for name in sorted(FEATURES)},
        "scripts": dict(sorted(scripts.items())),
        "inventory_rows": rows,
    }


def summary_table(report: dict) -> str:
    lines = [
        f"Omarchy port report for {report['inventory']['release_tag']} ({report['inventory']['commit'][:12]})",
        f"inventory commands: {report['coverage']['inventory_commands']}, analyzed: {report['coverage']['analyzed']}, "
        f"no source available: {report['coverage']['no_source_available']}",
        "",
        f"{'script':<42} {'verdict':<9} {'unsup':>5} {'part':>5} {'supp':>5}  external commands",
        "-" * 100,
    ]
    for path, item in report["scripts"].items():
        counts = item.get("counts", {})
        externals = ", ".join(c["name"] for c in item.get("external_commands", [])[:6])
        lines.append(
            f"{path:<42} {item['verdict']:<9} {counts.get('unsupported', 0):>5} "
            f"{counts.get('partial', 0):>5} {counts.get('supported', 0):>5}  {externals}"
        )
    lines.append("-" * 100)
    lines.append("verdicts: " + ", ".join(f"{k}={v}" for k, v in report["verdicts"].items()))
    if report["feature_scripts"]:
        lines.append("features (scripts): " + ", ".join(f"{k}={v}" for k, v in report["feature_scripts"].items()))
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--inventory", default=DEFAULT_INVENTORY)
    parser.add_argument("--fixtures", default=DEFAULT_FIXTURES)
    parser.add_argument("--checkout", default=None)
    parser.add_argument("--manifests", default=DEFAULT_MANIFESTS)
    parser.add_argument("--json", default=None, help="write the full JSON report to this path")
    args = parser.parse_args(argv)
    report = build_report(args)
    if args.json:
        with open(args.json, "w", encoding="utf-8", newline="\n") as handle:
            json.dump(report, handle, indent=2, sort_keys=True)
            handle.write("\n")
    print(summary_table(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
