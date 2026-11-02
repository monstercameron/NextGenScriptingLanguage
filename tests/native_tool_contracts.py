#!/usr/bin/env python3
"""Issue #460: static audit for the shipped native-tool v1 contracts.

No build required: this parses ``manifests/*.json`` plus the two Rust sources
that place effects. ``make check`` runs this audit before ``rust-check``, and
the Windows CI job runs it directly, so the taxonomy is guarded even when a
stale ``src/bundled_manifests/`` bundle keeps ``cargo test`` red; the deeper
``tests/native_tool_contracts.rs`` suite re-checks the same taxonomy in Rust.
The capability/effect table here intentionally mirrors that Rust table; keep
both in sync when the taxonomy changes. The bundled-index freshness test below
is read-only: it reports drift and names the sanctioned regeneration command,
and never rewrites ``src/bundled_manifests/`` itself.
"""

from __future__ import annotations

import hashlib
import json
import re
import unittest
from pathlib import Path

PROJECT = Path(__file__).resolve().parents[1]
MANIFESTS = PROJECT / "manifests"
BUNDLED = PROJECT / "src" / "bundled_manifests"

PURE = (["process.exec"], [])
READER = (["process.exec", "filesystem.read"], ["filesystem.read"])
WRITER = (["process.exec", "filesystem.write"], ["filesystem.write"])
READWRITE = (
    ["process.exec", "filesystem.read", "filesystem.write"],
    ["filesystem.read", "filesystem.write"],
)
# Child command execution beyond the spawn itself (awk system(), GNU sed e,
# git.commit hooks/GPG/editors) is a declared process.exec *effect*. The
# advisory gate maps that effect to `undeclared`, so untrusted use needs
# explicit approval; actual enforcement is the capability policy refusing
# grants that withhold process.exec, while the native sandbox still narrows
# the surrounding file grants (it does not separately confine the child
# command). The marking is conservative: platforms lacking a vector (e.g.
# BSD sed without e) still declare it and still require the same approval.
READWRITE_EXEC = (
    ["process.exec", "filesystem.read", "filesystem.write"],
    ["filesystem.read", "filesystem.write", "process.exec"],
)
MODEL = (
    ["process.exec", "filesystem.read", "network.connect"],
    ["filesystem.read", "network.connect"],
)

# Manifest file -> (capabilities, effects) contracted by issue #460.
#
# Free-form programs and flag spreads force honest file effects: awk programs
# read through getline and write through print redirections (both probed live);
# awk system(), GNU sed e, and git.commit hooks/GPG/editors execute child
# commands, which is a declared process.exec effect (gate: undeclared), not a
# documented residual; jq --rawfile reads
# (probed live) and -f, --slurpfile, and -L are the same documented
# file-loading options; sed r reads and w writes (probed live); sort/uniq
# flag operands name input files and sort -o / uniq's second operand names
# output files (probed live); grep -f/-r and grep/wc file operands read.
# jq.ps keeps PURE honestly: its argv is pinned literals plus one integer,
# and its fixed filter contains no $ENV, input, or file constructs. ps/pwd
# keep [] honestly: they take no file operands and their host-state output
# has no placed file effect, which their descriptions say. tail.follow keeps
# oneshot honestly: oneshot describes the spawn contract, while follow/stream
# behavior is composed with `stream from` (see tests/streaming.rs); the
# lifecycle vocabulary has no streaming-command kind.
EXPECTED = {
    "awk.json": READWRITE_EXEC,
    "count.lines.json": PURE,
    "cut.json": PURE,
    "false.json": PURE,
    "grep.json": READER,
    "head.json": PURE,
    "jq.json": READER,
    "jq.ps.json": PURE,
    "printf.json": PURE,
    "ps.json": PURE,
    "pwd.json": PURE,
    "tr.json": PURE,
    "upper.json": PURE,
    "wc.json": READER,
    "cat.json": READER,
    "comm.json": READER,
    "join.json": READER,
    "ls.json": READER,
    "paste.json": READER,
    "tail.json": READER,
    "tail.follow.json": READER,
    "sed.json": READWRITE_EXEC,
    "sort.json": READWRITE,
    "uniq.json": READWRITE,
    "mkdir.json": WRITER,
    "git.commit.json": READWRITE_EXEC,
    "gh.repo.json": MODEL,
    "jev.ask.json": MODEL,
    "jev.classify.json": MODEL,
    "jev.judge.json": MODEL,
    "jev.score.json": MODEL,
}

# Effects the Linux/macOS sandbox path enforces for native children.
ENFORCEABLE = {
    "filesystem.read",
    "filesystem.write",
    "network.connect",
    "process.exec",
}


def load(name):
    with open(MANIFESTS / name, encoding="utf-8") as handle:
        return json.load(handle)


def bundled_index_entries():
    """Parse ``src/bundled_manifests/index.rs`` + ``index.tsv`` read-only.

    Returns {file: (bytes, sha256)}. This mirrors the size + SHA-256 identity
    ``src/manifest.rs`` enforces per file; bodies.bin offsets are covered by
    the Rust ``checked_in_index_matches_the_manifests_directory`` test.
    """
    index_rs = (BUNDLED / "index.rs").read_text(encoding="utf-8")
    pattern = re.compile(
        r'file:\s*"([^"]+)",\s*\n\s*bytes:\s*(\d+),\s*\n\s*sha256:\s*"([0-9a-f]+)"'
    )
    entries = {name: (int(size), sha) for name, size, sha in pattern.findall(index_rs)}
    rows = (BUNDLED / "index.tsv").read_text(encoding="utf-8").splitlines()
    assert rows[0].startswith("execute.bundled-manifests"), rows[0]
    tsv_names = {row.split("\t")[0] for row in rows[1:] if row}
    assert set(entries) == tsv_names, "index.rs and index.tsv list different files"
    return entries


class NativeToolContractTests(unittest.TestCase):
    def test_native_tool_taxonomy_is_complete(self):
        self.assertEqual(len(EXPECTED), 31)

    def test_every_non_builtin_manifest_has_a_v1_contract(self):
        checked = 0
        for path in sorted(MANIFESTS.glob("*.json")):
            manifest = load(path.name)
            if manifest.get("builtin"):
                continue
            contract = manifest.get("contract")
            self.assertIsNotNone(contract, f"{path.name} lacks a contract")
            self.assertEqual(contract.get("version"), 1, path.name)
            checked += 1
        self.assertGreater(checked, 200)
        cd = load("cd.json")
        self.assertEqual(cd.get("builtin"), "cd")
        self.assertNotIn("contract", cd)

    def test_native_tool_contracts_match_the_taxonomy(self):
        for name, (capabilities, effects) in EXPECTED.items():
            manifest = load(name)
            contract = manifest.get("contract")
            self.assertIsNotNone(contract, name)
            self.assertEqual(contract.get("backend"), "command", name)
            self.assertEqual(contract.get("lifecycle"), {"kind": "oneshot"}, name)
            # The three jev model tools keep their pre-existing record/json
            # output schemas; only their capabilities/effects are audited here.
            if name in ("jev.classify.json", "jev.judge.json", "jev.score.json"):
                self.assertEqual(contract["output"]["encoding"], "json", name)
            else:
                self.assertEqual(
                    contract.get("output"), {"type": "text", "encoding": "raw"}, name
                )
                self.assertNotIn("result_envelope", contract, name)
                self.assertNotIn("outcomes", contract, name)
                self.assertNotIn("trim_final_lf", json.dumps(contract), name)
            # text/raw with no envelope and no trim preserves the legacy byte
            # content (including a trailing newline) as a string value. The
            # representation still changes: legacy contract-less decode
            # returned raw bytes while a text/raw contract returns a string
            # value, which the Rust decode test pins explicitly.
            self.assertEqual(contract.get("capabilities"), capabilities, name)
            self.assertEqual(contract.get("effects"), effects, name)
            self.assertLessEqual(set(effects), set(capabilities), name)
            for effect in effects:
                self.assertIn(effect, ENFORCEABLE, name)

    def test_cacheable_native_tools_declare_no_writes(self):
        for name in EXPECTED:
            manifest = load(name)
            if manifest.get("cacheable"):
                effects = manifest["contract"].get("effects", [])
                self.assertEqual(effects, [], f"{name} is cacheable but declares {effects}")

    def test_filesystem_read_maps_to_reads(self):
        sources = (PROJECT / "src/contracts.rs").read_text(encoding="utf-8")
        self.assertIn('"filesystem.read"', sources)
        self.assertIn('starts_with("filesystem.read")', sources)
        gate = (PROJECT / "src/agent/gate.rs").read_text(encoding="utf-8")
        self.assertIn("filesystem.read", gate)

    def test_exec_vectors_declare_process_effect_not_prose(self):
        """awk system(), GNU sed e, git hooks/GPG/editors gate as effects.

        A description sentence alone does not gate untrusted programs, so
        each vector must also declare ``process.exec`` as an effect. The
        gate maps that effect to ``undeclared`` (pinned below), while the
        native sandbox allowlist still covers it, so the surrounding file
        grants stay enforceable.
        """
        for name in ("awk.json", "sed.json", "git.commit.json"):
            manifest = load(name)
            contract = manifest["contract"]
            self.assertIn(
                "process.exec", contract.get("effects", []), name,
            )
            self.assertIn(
                "process.exec", contract.get("capabilities", []), name,
            )
            for effect in contract["effects"]:
                self.assertIn(effect, ENFORCEABLE, name)

    def test_process_exec_effect_gates_as_undeclared(self):
        """The declared exec effect must require explicit gate approval.

        ``effect_category("process.exec")`` has no placed mapping in
        ``src/contracts.rs`` (only ``process.spawn`` places as ``process``),
        so it falls through to ``undeclared`` and the gate refuses to pass
        it without ``--allow undeclared``.
        """
        sources = (PROJECT / "src/contracts.rs").read_text(encoding="utf-8")
        self.assertNotIn('("process.exec"', sources)
        self.assertIn('effect == "process.spawn"', sources)
        gate = (PROJECT / "src/agent/gate.rs").read_text(encoding="utf-8")
        self.assertIn('"undeclared"', gate)
        self.assertIn("Any other effect is `undeclared`", gate)

    def test_native_origin_restriction_remains_a_refusal(self):
        """Model endpoint origins stay unenforceable for native children.

        Per-endpoint ``network_origins`` cannot be confined for spawned
        commands, so a native grant carrying origins must keep failing
        closed; only host-managed remote applets may carry origins.
        """
        capabilities = (PROJECT / "src/capabilities.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "cannot enforce network origin restrictions for native children",
            capabilities,
        )
        self.assertIn(
            "network_origins is supported only for host-managed remote applets",
            capabilities,
        )

    def test_bundled_index_matches_manifests_directory(self):
        """The shipped bundle pins the same bytes ``cargo test`` ships.

        Read-only: compares every on-disk ``manifests/*.json`` file size and
        SHA-256 against the checked-in bundle index. A mismatch fails closed
        here first (before ``rust-check``) with the sanctioned fix; only the
        coordinator regenerates via ``EXECUTE_UPDATE_BUNDLED_INDEX=1 cargo
        test``. Never hand-edit ``src/bundled_manifests/`` to satisfy this.
        """
        entries = bundled_index_entries()
        on_disk = {path.name for path in MANIFESTS.glob("*.json")}
        self.assertEqual(
            set(entries),
            on_disk,
            "bundle index and manifests/ list different files",
        )
        stale = []
        for name in sorted(on_disk):
            raw = (MANIFESTS / name).read_bytes()
            size, sha = entries[name]
            if len(raw) != size or hashlib.sha256(raw).hexdigest() != sha:
                stale.append(name)
        self.assertEqual(
            stale,
            [],
            f"bundled index is stale for {stale}; regenerate only with "
            "EXECUTE_UPDATE_BUNDLED_INDEX=1 cargo test",
        )


if __name__ == "__main__":
    unittest.main()
