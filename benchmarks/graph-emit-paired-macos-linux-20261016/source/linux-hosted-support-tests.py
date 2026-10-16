import copy
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import hosted_emit_support as support  # noqa: E402


class HostedEmitSupportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "source"
        (self.root / "src").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text("[package]\nname='flow'\n", encoding="utf-8")
        (self.root / "src/lib.rs").write_text("// lib\n", encoding="utf-8")
        self.target_dir = Path(self.temporary.name) / "target"
        self.executable = self.target_dir / "release/deps/flow-test"
        self.executable.parent.mkdir(parents=True)
        self.executable.write_bytes(b"test executable")
        self.executable.chmod(0o755)
        self.artifact = {
            "reason": "compiler-artifact",
            "manifest_path": str(self.root / "Cargo.toml"),
            "target": {
                "kind": ["lib"],
                "name": "flow",
                "src_path": str(self.root / "src/lib.rs"),
                "test": True,
            },
            "profile": {
                "test": True,
                "opt_level": "3",
                "debug_assertions": False,
            },
            "fresh": False,
            "executable": str(self.executable),
        }

    def test_selects_only_the_exact_fresh_release_library_test_artifact(self):
        selected, executable = support.select_test_binary(
            [self.artifact], self.root, self.target_dir
        )
        self.assertEqual(selected, self.artifact)
        self.assertEqual(executable, self.executable.resolve())

    def test_rejects_wrong_target_profile_manifest_freshness_and_path(self):
        mutations = [
            lambda row: row["target"].update(test=False),
            lambda row: row["profile"].update(test=False),
            lambda row: row["profile"].update(opt_level="0"),
            lambda row: row["profile"].update(debug_assertions=True),
            lambda row: row.update(fresh=True),
            lambda row: row.update(manifest_path=str(self.root / "other.toml")),
            lambda row: row["target"].update(src_path=str(self.root / "src/other.rs")),
            lambda row: row.update(executable=str(self.root / "outside-test-target")),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                invalid = copy.deepcopy(self.artifact)
                mutate(invalid)
                with self.assertRaises(support.EvidenceError):
                    support.select_test_binary([invalid], self.root, self.target_dir)

    def test_extracts_only_the_hash_pinned_complete_harness_bytes(self):
        harness = (
            HERE / "emit-perf-harness-final.rs"
        ).read_bytes()
        self.assertEqual(hashlib.sha256(harness).hexdigest(), support.HARNESS_SHA256)
        self.assertEqual(
            support.extract_harness(b"production source\n" + harness + b"\nother module\n", harness),
            harness,
        )
        with self.assertRaises(support.EvidenceError):
            support.extract_harness(harness + b"\n" + harness, harness)
        with self.assertRaises(support.EvidenceError):
            support.extract_harness(b"no harness source", harness)

    def test_cargo_json_reader_selects_compiler_artifacts_from_jsonl(self):
        cargo_json = Path(self.temporary.name) / "cargo.jsonl"
        cargo_json.write_text(
            json.dumps({"reason": "compiler-message"}) + "\n"
            + json.dumps(self.artifact) + "\n",
            encoding="utf-8",
        )
        records = list(support._cargo_artifact_lines(cargo_json))
        self.assertEqual(records, [self.artifact])

    def test_unpacks_exact_compressed_patch_bytes_without_overwrite(self):
        for side, filename, expected_hash in [
            (
                "baseline",
                "baseline-patch.xz",
                support.BASELINE_PATCH_SHA256,
            ),
            (
                "candidate",
                "candidate-patch.xz",
                support.CANDIDATE_PATCH_SHA256,
            ),
        ]:
            with self.subTest(side=side):
                output = Path(self.temporary.name) / f"{side}.patch"
                archive = HERE / filename
                receipt = support.unpack_frozen_patch(side, archive, output)
                self.assertEqual(receipt["patch_sha256"], expected_hash)
                self.assertEqual(hashlib.sha256(output.read_bytes()).hexdigest(), expected_hash)
                with self.assertRaises(support.EvidenceError):
                    support.unpack_frozen_patch(side, archive, output)

    def test_workflow_is_fixed_pinned_and_uploads_evidence_after_failure(self):
        repo = HERE.parents[1]
        workflow = (repo / ".github/workflows/emit-perf-hosted-replication.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("workflow_dispatch:", workflow)
        self.assertNotIn("inputs:", workflow)
        self.assertIn("measured_pairs=48", workflow)
        self.assertIn("warmup_pairs=3", workflow)
        self.assertIn('INVOCATION_TIMEOUT_SECONDS: "60"', workflow)
        self.assertIn("timeout-minutes: 150", workflow)
        self.assertIn("if: always()", workflow)
        self.assertIn("no_result_dependent_retries=true", workflow)
        self.assertIn(
            "BASELINE_PATCH_SHA256: 3c7b691f8b0b88c4393f58105fea8d29a566dbbdf958d4eb4e063d20f93eb60e",
            workflow,
        )
        self.assertIn(
            "CANDIDATE_PATCH_SHA256: 2c9bcd8ec874fed6f4798224f9d94b4fa5b59f208db16e7d8f60b81192f8d877",
            workflow,
        )
        self.assertIn(
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
            workflow,
        )
        self.assertIn(
            "dtolnay/rust-toolchain@686976e191b89faba57d3206551f0f330d8cb249",
            workflow,
        )
        self.assertIn(
            "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9",
            workflow,
        )


if __name__ == "__main__":
    unittest.main()
