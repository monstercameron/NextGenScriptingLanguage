"""Offline contracts for the fixed hosted graph E2E build helper."""
from __future__ import annotations

import copy
from contextlib import redirect_stdout
import importlib.util
from io import StringIO
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
from unittest import mock
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

    def test_pair_prepare_consumes_the_helpers_complete_build_provenance_fixture(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            campaign = Path(temporary)
            evidence = campaign / "evidence/graph-transport-e2e"
            evidence.mkdir(parents=True)
            baseline_root = campaign / "work/baseline"
            candidate_root = campaign / "work/candidate"
            baseline_root.mkdir(parents=True)
            candidate_root.mkdir(parents=True)
            (campaign / "work/build_graph_e2e_pair.py").write_bytes(b"pinned build helper fixture\n")

            source_roots = {"baseline": baseline_root, "candidate": candidate_root}
            transport_hashes = {"baseline": "1" * 64, "candidate": "2" * 64}
            common_lib_hash = "3" * 64
            harness = (HERE / "graph_transport_e2e_bench.rs").read_bytes()
            metadata_by_side = {}
            payloads = {}
            log_names = {
                "clean": "{side}-release-clean-attempt-1.log",
                "build_json": "{side}-build-attempt-1.jsonl",
                "build_stderr": "{side}-build-attempt-1.log",
                "test_list": "{side}-test-list.txt",
            }
            for side, root in source_roots.items():
                (root / "Cargo.toml").write_text("[package]\nname='execute'\n", encoding="utf-8")
                harness_path = root / support.pair.HARNESS_RELATIVE
                harness_path.parent.mkdir(parents=True)
                harness_path.write_bytes(harness)
                target = campaign / "targets" / side
                target_binary = target / "release/deps/graph_transport_e2e_bench-fixture"
                target_binary.parent.mkdir(parents=True)
                target_binary.write_bytes(b"compiler-produced fixture executable\n")
                target_binary.chmod(0o755)
                selected_env = {
                    "PATH": "/toolchain/bin:/usr/bin",
                    "CARGO_HOME": "/home/runner/.cargo",
                    "RUSTUP_HOME": "/home/runner/.rustup",
                    "RUSTUP_TOOLCHAIN": "stable",
                    "CARGO_BUILD_JOBS": "4",
                    "CARGO_INCREMENTAL": "0",
                    "CARGO_TARGET_DIR": str(target),
                    support.CONFIG_FINGERPRINT_ENV_KEY: "a" * 64,
                }
                metadata_by_side[side] = {
                    "rustc_vv": "rustc 1.90.0\n",
                    "cargo_version": "cargo 1.90.0\n",
                    "selected_build_environment": selected_env,
                    "captured_before_clean_and_build": True,
                }
                artifact = {
                    "reason": "compiler-artifact",
                    "manifest_path": str(root / "Cargo.toml"),
                    "target": {
                        "name": "graph_transport_e2e_bench",
                        "kind": ["test"],
                        "test": True,
                        "src_path": str(harness_path),
                    },
                    "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
                    "fresh": False,
                    "executable": str(target_binary),
                    "filenames": [str(target_binary)],
                }
                saved_binary = evidence / f"{side}-test-binary"
                saved_binary.write_bytes(b"saved immutable test executable fixture\n")
                saved_binary.chmod(0o755)
                receipt = {
                    "artifact": artifact,
                    "saved_binary": str(saved_binary),
                    "sha256": support.sha256_file(saved_binary),
                    "source_sha256": transport_hashes[side],
                    "source_stable": True,
                    "attempt": 1,
                    "build_metadata": metadata_by_side[side],
                }
                support.write_json(evidence / f"{side}-binary-receipt.json", receipt, exclusive=True)
                clean = b"No cargo clean invoked; fresh target directory.\n"
                stderr = b"cargo build log fixture\n"
                test_list = (support.pair.TEST_NAME + ": test\n").encode()
                build_json = (json.dumps(artifact, sort_keys=True) + "\n").encode()
                payloads[side] = {
                    "clean": clean,
                    "build_json": build_json,
                    "build_stderr": stderr,
                    "test_list": test_list,
                }
                for label, payload in payloads[side].items():
                    (evidence / log_names[label].format(side=side)).write_bytes(payload)

            baseline_metadata = metadata_by_side["baseline"]
            candidate_metadata = metadata_by_side["candidate"]
            environment_path = support.write_build_environment_receipt(evidence, baseline_metadata)
            self.assertEqual(
                environment_path,
                campaign / support.pair.BUILD_ENVIRONMENT_RELATIVE,
            )
            self.assertTrue(environment_path.is_file())

            def fixture_source_identity(root, side, _harness):
                return {
                    "root": str(root.resolve()),
                    "commit": support.pair.BASELINE_COMMIT,
                    "transport_path": str(root / "src/graph/transport.rs"),
                    "transport_sha256": transport_hashes[side],
                    "harness_path": str(root / support.pair.HARNESS_RELATIVE),
                    "harness_sha256": support.pair.HARNESS_SHA256,
                    "source_fingerprint": {
                        "files": {
                            "src/graph/transport.rs": transport_hashes[side],
                            "src/lib.rs": common_lib_hash,
                        }
                    },
                }

            preregistration = campaign / "paired-preregistration.json"
            args = SimpleNamespace(
                harness_source=str(HERE / "graph_transport_e2e_bench.rs"),
                candidate_root=str(candidate_root),
                baseline_receipt=str(evidence / "baseline-binary-receipt.json"),
                candidate_receipt=str(evidence / "candidate-binary-receipt.json"),
                preregistration=str(preregistration),
            )
            with mock.patch.object(support.pair, "CAMPAIGN", campaign), \
                 mock.patch.object(support.pair, "BASELINE_ROOT", baseline_root), \
                 mock.patch.object(support.pair, "_inspect_source", side_effect=fixture_source_identity), \
                 redirect_stdout(StringIO()):
                support.pair.prepare(args)

            result = json.loads(preregistration.read_text(encoding="utf-8"))
            provenance = result["inputs"]["build_provenance"]
            self.assertEqual(
                provenance["environment"]["path"],
                str(environment_path.resolve()),
            )
            self.assertEqual(provenance["environment"]["sha256"], support.sha256_file(environment_path))
            self.assertEqual(provenance["driver"]["path"], str((campaign / "work/build_graph_e2e_pair.py").resolve()))
            for side, artifacts in payloads.items():
                self.assertEqual(set(provenance["logs"][side]), set(log_names))
                for label, payload in artifacts.items():
                    artifact_path = evidence / log_names[label].format(side=side)
                    self.assertEqual(provenance["logs"][side][label]["path"], str(artifact_path.resolve()))
                    self.assertEqual(provenance["logs"][side][label]["sha256"], support.sha256_file(artifact_path))

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
