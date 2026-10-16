"""Offline contracts for the fixed hosted graph E2E build helper."""
from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
_SPEC = importlib.util.spec_from_file_location("build_graph_e2e_pair", HERE.parent / "build_graph_e2e_pair.py")
assert _SPEC is not None and _SPEC.loader is not None
support = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = support
_SPEC.loader.exec_module(support)


class HostedGraphBuildTests(unittest.TestCase):
    def test_pinned_inputs_and_xz_patch_are_exact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary) / "evidence"
            receipt = support.verify_inputs(evidence)
            self.assertEqual(receipt["hashes"]["graph_transport_e2e_pair.py"], support.RUNNER_SHA256)
            self.assertEqual(receipt["hashes"]["test_graph_transport_e2e_pair.py"], support.RUNNER_TEST_SHA256)
            self.assertEqual(receipt["canonical_harness_sha256"], support.HARNESS_SHA256)
            self.assertEqual(receipt["decoded_patch_hashes"][support.CANDIDATE_PATCH["archive"]], support.CANDIDATE_PATCH["patch_sha256"])
            self.assertTrue((evidence / "build").is_dir())
            self.assertFalse((evidence / support.CANDIDATE_PATCH["archive"].removesuffix(".xz")).exists())

    def test_artifact_selector_requires_fresh_release_integration_test_at_exact_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "source"
            (root / "tests").mkdir(parents=True)
            (root / "Cargo.toml").write_text("[package]\nname='execute'\n", encoding="utf-8")
            harness = root / support.HARNESS_RELATIVE
            harness.write_bytes(b"frozen harness\n")
            target_dir = Path(temporary) / "target"
            binary = target_dir / "release/deps/graph_transport_e2e_bench-1234"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"test executable")
            binary.chmod(0o755)
            row = {
                "reason": "compiler-artifact",
                "manifest_path": str(root / "Cargo.toml"),
                "target": {
                    "name": "graph_transport_e2e_bench",
                    "kind": ["test"],
                    "test": True,
                    "src_path": str(harness),
                },
                "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
                "fresh": False,
                "executable": str(binary),
                "filenames": [str(binary)],
            }
            artifact, selected = support.select_test_artifact([row], root, target_dir)
            self.assertEqual(artifact, row)
            self.assertEqual(selected, binary.resolve())
            for mutate in (
                lambda item: item.update(fresh=True),
                lambda item: item["target"].update(src_path=str(root / "tests/other.rs")),
                lambda item: item["profile"].update(opt_level="0"),
                lambda item: item["profile"].update(debug_assertions=True),
                lambda item: item.update(executable=str(Path(temporary) / "outside")),
                lambda item: item.update(filenames=[]),
            ):
                invalid = copy.deepcopy(row)
                mutate(invalid)
                with self.subTest(mutate=mutate), self.assertRaises(support.EvidenceError):
                    support.select_test_artifact([invalid], root, target_dir)

    def test_environment_capture_keeps_target_separate_and_records_build_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary) / "home"
            cargo_home = home / ".cargo"
            (cargo_home).mkdir(parents=True)
            (home / ".rustup").mkdir()
            roots = {side: home / "work" / side for side in ("baseline", "candidate")}
            for root in roots.values():
                root.mkdir(parents=True)
                (root / ".cargo").mkdir()
                (root / ".cargo/config.toml").write_text("[net]\nretry = 2\n", encoding="utf-8")
            (cargo_home / "config.toml").write_text("[net]\noffline = false\n", encoding="utf-8")
            (home / "work/.cargo").mkdir()
            (home / "work/.cargo/config").write_text("[http]\ntimeout = 30\n", encoding="utf-8")
            (home.parent / ".cargo").mkdir()
            (home.parent / ".cargo/config.toml").write_text("[net]\nretry = 1\n", encoding="utf-8")

            host = {
                "HOME": str(home),
                "PATH": "/toolchain/bin:/usr/bin",
                "TMPDIR": "/tmp",
                "CARGO_HOME": "/attacker/cargo",
                "RUSTUP_HOME": "/attacker/rustup",
                "RUSTUP_TOOLCHAIN": "nightly",
                "CARGO_BUILD_JOBS": "99",
                "CARGO_INCREMENTAL": "1",
                "CARGO_PROFILE_RELEASE_DEBUG": "true",
                "CARGO_ENCODED_RUSTFLAGS": "--cfg=unreviewed",
                "RUSTFLAGS": "-C link-arg=unreviewed",
                "RUSTC_WRAPPER": "/tmp/wrapper",
                "CC": "/tmp/cc",
                "AR": "/tmp/ar",
                "UNRELATED_TOKEN": "do not record",
            }
            base = support._base_build_environment(Path(temporary) / "targets/base", host)
            candidate = support._base_build_environment(Path(temporary) / "targets/candidate", host)
            self.assertEqual(base["CARGO_HOME"], str(cargo_home.resolve()))
            self.assertEqual(base["RUSTUP_HOME"], str((home / ".rustup").resolve()))
            self.assertEqual(base["RUSTUP_TOOLCHAIN"], "stable")
            self.assertEqual(base["CARGO_BUILD_JOBS"], "4")
            self.assertEqual(base["CARGO_INCREMENTAL"], "0")
            for hostile_key in (
                "CARGO_PROFILE_RELEASE_DEBUG", "CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS",
                "RUSTC_WRAPPER", "CC", "AR", "UNRELATED_TOKEN",
            ):
                self.assertNotIn(hostile_key, base)
            fingerprint = support.cargo_configuration_fingerprint(roots, base)
            common_hash = support.paired_cargo_configuration_sha256(fingerprint)
            self.assertEqual(len(common_hash), 64)
            self.assertIn("sha256", fingerprint["cargo_home"]["config_files"][1])
            self.assertTrue(fingerprint["source_roots"]["baseline"]["home_is_ancestor"])
            ancestors, home_is_ancestor = support._source_ancestor_directories(roots["baseline"], home)
            self.assertTrue(home_is_ancestor)
            self.assertEqual(ancestors[-1], Path(ancestors[-1].anchor))
            recorded_paths = {
                Path(item["path"])
                for item in fingerprint["source_roots"]["baseline"]["config_files"]
            }
            self.assertIn((home.parent / ".cargo/config.toml").resolve(), recorded_paths)
            self.assertNotIn("offline = false", json.dumps(fingerprint))
            support.verify_cargo_configuration_fingerprint(fingerprint, roots, base, "before test")
            (roots["candidate"] / ".cargo/config.toml").write_text("[net]\nretry = 3\n", encoding="utf-8")
            with self.assertRaises(support.EvidenceError):
                support.paired_cargo_configuration_sha256(
                    support.cargo_configuration_fingerprint(roots, base)
                )

            base[support.CONFIG_FINGERPRINT_ENV_KEY] = common_hash
            candidate[support.CONFIG_FINGERPRINT_ENV_KEY] = common_hash
            selected = support.selected_build_environment(base)
            self.assertEqual(selected["CARGO_TARGET_DIR"], str(Path(temporary) / "targets/base"))
            self.assertEqual(selected[support.CONFIG_FINGERPRINT_ENV_KEY], common_hash)
            for hostile_key in (
                "CARGO_PROFILE_RELEASE_DEBUG", "CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS",
                "RUSTC_WRAPPER", "CC", "AR", "UNRELATED_TOKEN",
            ):
                self.assertNotIn(hostile_key, selected)
            normalized_base = {key: value for key, value in selected.items() if key != "CARGO_TARGET_DIR"}
            normalized_candidate = {
                key: value for key, value in support.selected_build_environment(candidate).items()
                if key != "CARGO_TARGET_DIR"
            }
            self.assertEqual(normalized_base, normalized_candidate)

    def test_candidate_metadata_uses_the_runner_validation_contract(self) -> None:
        baseline = {
            "rustc_vv": "rustc 1.90.0\n",
            "cargo_version": "cargo 1.90.0\n",
            "selected_build_environment": {
                "PATH": "/toolchain/bin:/usr/bin",
                "CARGO_HOME": "/home/runner/.cargo",
                "RUSTUP_HOME": "/home/runner/.rustup",
                "RUSTUP_TOOLCHAIN": "stable",
                "CARGO_BUILD_JOBS": "4",
                "CARGO_TARGET_DIR": "/tmp/targets/baseline",
                "CARGO_INCREMENTAL": "0",
            },
            "captured_before_clean_and_build": True,
        }
        candidate = copy.deepcopy(baseline)
        candidate["selected_build_environment"]["CARGO_TARGET_DIR"] = "/tmp/targets/candidate"
        support.validate_build_metadata_pair(baseline, candidate)

        invalid = copy.deepcopy(candidate)
        del invalid["selected_build_environment"]["PATH"]
        with self.assertRaises(support.pair.PairError):
            support.validate_build_metadata_pair(baseline, invalid)

    def test_hosted_workflow_is_manual_fixed_and_preserves_failure_artifacts(self) -> None:
        workflow = (HERE.parents[1] / ".github/workflows/graph-collector-e2e-hosted-replication.yml").read_text(encoding="utf-8")
        self.assertIn("workflow_dispatch:", workflow)
        self.assertNotIn("inputs:", workflow)
        self.assertIn("runs-on: ubuntu-24.04", workflow)
        self.assertIn("timeout-minutes: 180", workflow)
        self.assertIn("timeout-minutes: 95", workflow)
        self.assertIn("timeout-minutes: 75", workflow)
        self.assertIn(support.BASE_COMMIT, workflow)
        self.assertIn(str(support.CANDIDATE_PATCH["archive_sha256"]), workflow)
        self.assertIn("if: always()", workflow)
        self.assertIn("compression-level: 6", workflow)
        self.assertIn("work/build_graph_e2e_pair.py", workflow)
        self.assertIn("graph_transport_e2e_pair.py", workflow)
        self.assertIn(support.sha256_file(HERE.parents[0] / "build_graph_e2e_pair.py"), workflow)
        self.assertIn(support.sha256_file(HERE / "test_build_graph_e2e_pair.py"), workflow)


if __name__ == "__main__":
    unittest.main()
