"""Offline contracts for the fixed hosted build support; no Cargo or benchmark runs."""
from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
_SPEC = importlib.util.spec_from_file_location("hosted_support", HERE / "hosted_support.py")
assert _SPEC is not None and _SPEC.loader is not None
support = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = support
_SPEC.loader.exec_module(support)


class HostedBuildSupportTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.tmp = Path(self.temporary.name)

    def test_staged_frozen_inputs_match_all_pinned_hashes(self) -> None:
        observed = support.verify_repository_inputs(HERE.parents[1])
        self.assertEqual(observed["resolved_json_arguments_pair.py"], support.RUNNER_SHA256)
        self.assertEqual(observed["test_resolved_json_arguments_pair.py"], support.RUNNER_TEST_SHA256)
        self.assertEqual(observed["resolved_json_arguments_bench.rs"], support.HARNESS_SHA256)
        self.assertEqual(observed["resolved-json-arguments-repeat-1000.program"], support.WORKLOAD_SHA256)
        self.assertEqual(observed[support.PATCHES["baseline"]["archive"]], support.PATCHES["baseline"]["archive_sha256"])
        self.assertEqual(observed[support.PATCHES["candidate"]["archive"]], support.PATCHES["candidate"]["archive_sha256"])

    def test_input_verification_cli_writes_a_hash_receipt_without_building(self) -> None:
        evidence = self.tmp / "evidence"
        result = subprocess.run(
            [
                sys.executable,
                "-B",
                str(HERE / "hosted_support.py"),
                "verify-inputs",
                "--repo-root",
                str(HERE.parents[1]),
                "--evidence-root",
                str(evidence),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=10,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        stdout = json.loads(result.stdout)
        receipt = json.loads((evidence / "build/verified-inputs.json").read_text(encoding="utf-8"))
        self.assertEqual(stdout["hashes"]["resolved_json_arguments_pair.py"], support.RUNNER_SHA256)
        self.assertEqual(receipt["hashes"], stdout["hashes"])
        self.assertFalse((evidence / "build/baseline").exists())

    def test_rejects_a_changed_archive_pin(self) -> None:
        original = support.PATCHES["baseline"]["archive_sha256"]
        support.PATCHES["baseline"]["archive_sha256"] = "0" * 64
        try:
            with self.assertRaises(support.EvidenceError):
                support.verify_repository_inputs(HERE.parents[1])
        finally:
            support.PATCHES["baseline"]["archive_sha256"] = original

    def test_unpacks_only_the_frozen_patch_and_refuses_overwrite(self) -> None:
        for side in ("baseline", "candidate"):
            with self.subTest(side=side):
                output = self.tmp / f"{side}.patch"
                receipt = support.unpack_patch(side, HERE.parents[1], output)
                self.assertEqual(receipt["patch_sha256"], support.PATCHES[side]["patch_sha256"])
                self.assertEqual(hashlib.sha256(output.read_bytes()).hexdigest(), support.PATCHES[side]["patch_sha256"])
                with self.assertRaises(support.EvidenceError):
                    support.unpack_patch(side, HERE.parents[1], output)
        with self.assertRaises(support.EvidenceError):
            support.unpack_patch("other", HERE.parents[1], self.tmp / "bad.patch")

    def test_artifact_selector_accepts_only_the_release_library_test_target(self) -> None:
        root = self.tmp / "source"
        (root / "src").mkdir(parents=True)
        (root / "Cargo.toml").write_text("[package]\nname='execute'\n", encoding="utf-8")
        (root / "src/lib.rs").write_text("// lib\n", encoding="utf-8")
        target_dir = self.tmp / "target"
        executable = target_dir / "release/deps/flow-test"
        executable.parent.mkdir(parents=True)
        executable.write_bytes(b"immutable test binary")
        executable.chmod(0o755)
        row = {
            "reason": "compiler-artifact",
            "manifest_path": str(root / "Cargo.toml"),
            "package_id": f"path+{root.resolve().as_uri()}#execute@0.9.0",
            "target": {"kind": ["lib"], "crate_types": ["lib"], "name": "flow", "src_path": str(root / "src/lib.rs"), "test": True},
            "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
            "fresh": False,
            "executable": str(executable),
            "filenames": [str(executable)],
        }
        artifact, selected = support.select_library_test_artifact([row], root, target_dir)
        self.assertEqual(artifact, row)
        self.assertEqual(selected, executable.resolve())
        for change in (
            lambda copy_row: copy_row.update(fresh=True),
            lambda copy_row: copy_row["target"].update(test=False),
            lambda copy_row: copy_row["profile"].update(opt_level="0"),
            lambda copy_row: copy_row["profile"].update(debug_assertions=True),
            lambda copy_row: copy_row["target"].update(src_path=str(root / "src/other.rs")),
            lambda copy_row: copy_row.update(manifest_path=str(root / "other/Cargo.toml")),
            lambda copy_row: copy_row.update(executable=str(self.tmp / "outside")),
            lambda copy_row: copy_row.update(package_id="path+file:///wrong#execute@0.9.0"),
            lambda copy_row: copy_row.update(filenames=[]),
        ):
            invalid = copy.deepcopy(row)
            change(invalid)
            with self.subTest(change=change), self.assertRaises(support.EvidenceError):
                support.select_library_test_artifact([invalid], root, target_dir)

    def test_source_fingerprint_covers_tracked_and_untracked_frozen_inputs(self) -> None:
        root = self.tmp / "fingerprint-source"
        for relative, content in {
            "Cargo.toml": "[package]\nname='execute'\n",
            "Cargo.lock": "# lock\n",
            "src/runtime.rs": "// runtime\n",
            "tests/contract.rs": "// test\n",
            "vendor/dependency/lib.rs": "// vendor\n",
            ".cargo/config.toml": "[net]\noffline=true\n",
            "manifests/core.json": "{}\n",
        }.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        (root / support.pair.HARNESS_RELATIVE).parent.mkdir(parents=True, exist_ok=True)
        (root / support.pair.HARNESS_RELATIVE).write_bytes(b"harness")
        (root / support.pair.WORKLOAD_RELATIVE).parent.mkdir(parents=True, exist_ok=True)
        (root / support.pair.WORKLOAD_RELATIVE).write_bytes(b"workload")
        subprocess.run(["git", "init", str(root)], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        subprocess.run(["git", "-C", str(root), "add", "-A"], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        result = support.pair.source_fingerprint(root)
        self.assertEqual(result["files"]["src/runtime.rs"], hashlib.sha256(b"// runtime\n").hexdigest())
        self.assertIn("manifests/core.json", result["files"])
        self.assertIn(support.pair.HARNESS_RELATIVE, result["files"])
        self.assertIn(support.pair.WORKLOAD_RELATIVE, result["files"])
        self.assertEqual(len(result["sha256"]), 64)

    def test_workflow_is_manual_fixed_pinned_bounded_and_uploads_on_failure(self) -> None:
        workflow = (HERE.parents[1] / ".github/workflows/literal-json-arguments-hosted-replication.yml").read_text(encoding="utf-8")
        self.assertIn("workflow_dispatch:", workflow)
        self.assertNotIn("inputs:", workflow)
        self.assertIn("runs-on: ubuntu-24.04", workflow)
        self.assertIn("timeout-minutes: 300", workflow)
        self.assertIn("timeout-minutes: 100", workflow)
        self.assertIn("timeout-minutes: 185", workflow)
        self.assertIn("ref: b824aeaa7bccaa4d3d75d3fc6a363a99138287d4", workflow)
        self.assertIn("warmup_pairs=3", workflow)
        self.assertIn("measured_pairs=48", workflow)
        self.assertIn("invocation_timeout_seconds=120", workflow)
        self.assertIn("no_retries=true", workflow)
        self.assertIn("if: always()", workflow)
        self.assertIn("apply-patches", workflow)
        self.assertIn("build-pair", workflow)
        self.assertIn("--locked", (HERE / "hosted_support.py").read_text(encoding="utf-8"))
        self.assertIn('\"--release\", \"--lib\", \"--no-run\", \"--message-format=json\"', (HERE / "hosted_support.py").read_text(encoding="utf-8"))
        self.assertIn("prepare \\\n", workflow)
        self.assertIn(" run \\\n", workflow)
        self.assertEqual(workflow.count("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"), 3)
        self.assertIn("dtolnay/rust-toolchain@686976e191b89faba57d3206551f0f330d8cb249", workflow)
        self.assertIn("actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9", workflow)
        for filename in (
            "resolved_json_arguments_pair.py",
            "test_resolved_json_arguments_pair.py",
            "resolved_json_arguments_bench.rs",
            "resolved-json-arguments-repeat-1000.program",
            "hosted_support.py",
            "test_hosted_support.py",
        ):
            digest = hashlib.sha256((HERE / filename).read_bytes()).hexdigest()
            self.assertIn(digest, workflow, filename)


if __name__ == "__main__":
    unittest.main()
