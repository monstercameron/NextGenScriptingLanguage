from __future__ import annotations

import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest


DRIVER_PATH = Path(__file__).with_name("build_collector_cpu_diag_pair.py")
SPEC = importlib.util.spec_from_file_location("collector_cpu_build_driver", DRIVER_PATH)
assert SPEC is not None and SPEC.loader is not None
driver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(driver)


class CollectorCpuBuildEnvironmentTests(unittest.TestCase):
    def test_controlled_environment_drops_unlisted_cargo_and_rust_overrides(self) -> None:
        host = {
            "PATH": "/usr/bin:/bin",
            "HOME": "/tmp/cpu-diag-home",
            "TMPDIR": "/tmp/cpu-diag-tmp",
            "SDKROOT": "/sdk",
            "RUSTFLAGS": "--cfg injected",
            "CARGO_ENCODED_RUSTFLAGS": "--cfg=injected",
            "RUSTC": "/tmp/custom-rustc",
            "RUSTC_WRAPPER": "/tmp/wrapper",
            "CARGO_BUILD_TARGET": "aarch64-unknown-linux-gnu",
            "CARGO_PROFILE_RELEASE_OPT_LEVEL": "0",
            "AWS_SECRET_ACCESS_KEY": "must-not-be-forwarded-or-recorded",
        }

        controlled = driver.controlled_build_environment(host)

        self.assertEqual(
            set(controlled),
            set(driver.INHERITED_BUILD_ENV_KEYS) & set(host) | set(driver.FIXED_BUILD_ENV),
        )
        self.assertTrue(controlled["PATH"].startswith(f"{driver.CARGO_BIN}:/usr/bin:/bin"))
        self.assertEqual(controlled["CARGO_HOME"], str(driver.CARGO_HOME_PIN))
        self.assertEqual(controlled["RUSTUP_HOME"], str(driver.RUSTUP_HOME_PIN))
        self.assertEqual(controlled["RUSTUP_TOOLCHAIN"], driver.TOOLCHAIN)
        self.assertEqual(controlled["CARGO_TARGET_DIR"], str(driver.TARGET))
        self.assertEqual(controlled["CARGO_BUILD_JOBS"], "4")
        self.assertNotIn("RUSTFLAGS", controlled)
        self.assertNotIn("RUSTC", controlled)
        self.assertNotIn("RUSTC_WRAPPER", controlled)
        self.assertNotIn("CARGO_BUILD_TARGET", controlled)
        self.assertNotIn("AWS_SECRET_ACCESS_KEY", controlled)

    def test_fingerprint_records_config_file_presence_and_detects_changes_without_contents(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            cargo_home = root / "cargo-home"
            baseline = home / "project" / "baseline"
            candidate = home / "project" / "candidate"
            for path in (home, cargo_home, baseline, candidate):
                path.mkdir(parents=True, exist_ok=True)

            configs = {
                home / ".cargo" / "config.toml": "[net]\noffline = true\n",
                home / "project" / ".cargo" / "config": "[build]\njobs = 2\n",
                baseline / ".cargo" / "config.toml": "[alias]\ncheckfast = 'check'\n",
                cargo_home / "config.toml": "[net]\nretry = 0\n",
            }
            for path, content in configs.items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")

            env = {"HOME": str(home), "CARGO_HOME": str(cargo_home)}
            roots = {"baseline": baseline, "candidate": candidate}
            fingerprint = driver.cargo_configuration_fingerprint(roots, env)

            self.assertEqual(fingerprint["schema"], driver.CONFIG_FINGERPRINT_SCHEMA)
            self.assertTrue(fingerprint["source_roots"]["baseline"]["home_is_ancestor"])
            self.assertTrue(fingerprint["source_roots"]["candidate"]["home_is_ancestor"])
            baseline_paths = {
                item["path"]: item for item in fingerprint["source_roots"]["baseline"]["config_files"]
            }
            self.assertTrue(baseline_paths[str(home.resolve() / ".cargo" / "config.toml")]["present"])
            self.assertTrue(baseline_paths[str(baseline.resolve() / ".cargo" / "config.toml")]["present"])
            cargo_home_entry = fingerprint["cargo_home"]["config_files"][1]
            self.assertEqual(cargo_home_entry["sha256"], hashlib.sha256(configs[cargo_home / "config.toml"].encode()).hexdigest())
            self.assertNotIn(configs[cargo_home / "config.toml"], repr(fingerprint))
            driver.verify_cargo_configuration_fingerprint(fingerprint, roots, env, "before")

            changed_config = home / "project" / ".cargo" / "config"
            changed_config.write_text("[build]\njobs = 3\n", encoding="utf-8")
            with self.assertRaisesRegex(driver.BuildError, "configuration inputs changed"):
                driver.verify_cargo_configuration_fingerprint(fingerprint, roots, env, "during")


if __name__ == "__main__":
    unittest.main()
