import copy
import hashlib
import json
from pathlib import Path
import subprocess
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
        for profile, config in support.PROFILE_CONFIGS.items():
            for side, patch_config in config["patches"].items():
                with self.subTest(profile=profile, side=side):
                    output = Path(self.temporary.name) / f"{profile}-{side}.patch"
                    archive = HERE / patch_config["archive"]
                    receipt = support.unpack_frozen_patch(
                        side, archive, output, profile
                    )
                    self.assertEqual(receipt["profile"], profile)
                    self.assertEqual(receipt["patch_sha256"], patch_config["sha256"])
                    self.assertEqual(
                        hashlib.sha256(output.read_bytes()).hexdigest(),
                        patch_config["sha256"],
                    )
                    with self.assertRaises(support.EvidenceError):
                        support.unpack_frozen_patch(side, archive, output, profile)

    def test_rejects_tampered_profile_archive_and_unknown_profile(self):
        archive = HERE / support.PROFILE_CONFIGS["payload-size"]["patches"]["baseline"]["archive"]
        altered = bytearray(archive.read_bytes())
        altered[-1] ^= 1
        altered_path = Path(self.temporary.name) / "altered-payload-size.xz"
        altered_path.write_bytes(altered)
        with self.assertRaises(support.EvidenceError):
            support.unpack_frozen_patch(
                "baseline", altered_path, Path(self.temporary.name) / "bad.patch", "payload-size"
            )
        with self.assertRaises(support.EvidenceError):
            support.profile_environment_values("unlisted-profile")

    def test_profile_environment_has_literal_base_source_patch_and_harness_pins(self):
        self.assertEqual(support.DEFAULT_PROFILE, "emit-sync")
        sync = support.profile_environment_values("emit-sync")
        size = support.profile_environment_values("payload-size")
        self.assertEqual(sync["BASE_COMMIT"], "b78693294169c22892bb4f78539348a5755992ad")
        self.assertEqual(size["BASE_COMMIT"], "69a9e74abcdf09efd8ecb83e4b7f1a8d5f30488a")
        self.assertEqual(size["BASELINE_SOURCE_SHA256"], "36e59989fc8abe3afeeab0922792cfb4fff1110ef234b6e8946c9930c54dc056")
        self.assertEqual(size["CANDIDATE_SOURCE_SHA256"], "5975e969cf68e8b5d7cc5c11548d0b2234477f397697da81760c499d3ccb3927")
        self.assertEqual(size["BASELINE_PATCH_SHA256"], "63aae53a633ed39de9b18bc10c47166463de521231ca81f45a4d1f1fd424ba26")
        self.assertEqual(size["CANDIDATE_PATCH_SHA256"], "ba25e336def3d22920ecca23b070b959d4968a98098cd8fa0466c93acaf01c59")
        self.assertEqual(sync["HARNESS_SHA256"], size["HARNESS_SHA256"])
        self.assertEqual(sync["DRIVER_SHA256"], size["DRIVER_SHA256"])
        self.assertEqual(sync["INVOCATION_TIMEOUT_SECONDS"], "60")
        self.assertEqual(size["INVOCATION_TIMEOUT_SECONDS"], "60")

    def test_profile_selection_writes_only_known_literal_environment_values(self):
        env_file = Path(self.temporary.name) / "github-env"
        output_file = Path(self.temporary.name) / "github-output"
        values = support.select_profile("payload-size", env_file, output_file)
        self.assertEqual(
            env_file.read_text(encoding="utf-8").splitlines(),
            [f"{name}={value}" for name, value in values.items()],
        )
        self.assertEqual(
            output_file.read_text(encoding="utf-8").splitlines(),
            [f"{name.lower()}={value}" for name, value in values.items()],
        )
        self.assertEqual(values["EXPERIMENT_PROFILE"], "payload-size")

    def test_profile_cli_exports_the_selected_profile(self):
        env_file = Path(self.temporary.name) / "cli-github-env"
        output_file = Path(self.temporary.name) / "cli-github-output"
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "hosted_emit_support.py"),
                "select-profile",
                "--profile",
                "payload-size",
                "--github-env",
                str(env_file),
                "--github-output",
                str(output_file),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=5,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), support.profile_environment_values("payload-size"))
        self.assertIn("BASE_COMMIT=69a9e74abcdf09efd8ecb83e4b7f1a8d5f30488a", env_file.read_text())
        self.assertIn("base_commit=69a9e74abcdf09efd8ecb83e4b7f1a8d5f30488a", output_file.read_text())

    def test_workflow_is_fixed_pinned_and_uploads_evidence_after_failure(self):
        repo = HERE.parents[1]
        workflow = (repo / ".github/workflows/emit-perf-hosted-replication.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("experiment_profile:", workflow)
        self.assertIn("default: emit-sync", workflow)
        self.assertIn("- payload-size", workflow)
        self.assertIn("measured_pairs=48", workflow)
        self.assertIn("warmup_pairs=3", workflow)
        self.assertIn('[[ "$INVOCATION_TIMEOUT_SECONDS" == "60" ]]', workflow)
        self.assertIn("timeout-minutes: 150", workflow)
        self.assertIn("if: always()", workflow)
        self.assertIn("no_result_dependent_retries=true", workflow)
        self.assertIn("$BASELINE_PATCH_ARCHIVE", workflow)
        self.assertIn("$CANDIDATE_PATCH_ARCHIVE", workflow)
        self.assertIn("select-profile \\\n            --profile \"$EXPERIMENT_PROFILE\"", workflow)
        self.assertIn("--profile \"$EXPERIMENT_PROFILE\"", workflow)
        self.assertEqual(workflow.count("steps.profile.outputs.base_commit"), 2)
        self.assertEqual(
            support.PROFILE_CONFIGS["payload-size"]["patches"]["baseline"]["archive"],
            "baseline-payload-size-patch.xz",
        )
        self.assertEqual(
            support.PROFILE_CONFIGS["payload-size"]["patches"]["candidate"]["archive"],
            "candidate-payload-size-patch.xz",
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
