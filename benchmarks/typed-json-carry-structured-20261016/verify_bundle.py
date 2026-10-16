#!/usr/bin/env python3
"""Verify the frozen typed-JSON structured-trial publication bundle."""

from __future__ import annotations

import hashlib
import json
import re
import stat
import sys
from pathlib import Path, PurePosixPath

HEX64 = re.compile(r"^[0-9a-f]{64}$")
ROOT = Path(__file__).resolve().parent
MANIFEST_PATH = ROOT / "bundle-manifest.json"
SUMS_PATH = ROOT / "bundle-sha256sums.txt"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def local_path(relative: str) -> Path:
    pure = PurePosixPath(relative)
    require(not pure.is_absolute() and ".." not in pure.parts, f"unsafe bundle path: {relative}")
    result = ROOT.joinpath(*pure.parts)
    require(result.is_file() and not result.is_symlink(), f"missing/non-regular bundle file: {relative}")
    return result


def check_source_map_digest_values(value: object, prefix: str = "source-map") -> int:
    checked = 0
    if isinstance(value, dict):
        for key, child in value.items():
            name = f"{prefix}.{key}"
            if (key == "sha256" or key.endswith("_sha256")) and child is not None:
                require(isinstance(child, str) and HEX64.fullmatch(child) is not None, f"invalid SHA-256 at {name}")
                checked += 1
            checked += check_source_map_digest_values(child, name)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            checked += check_source_map_digest_values(child, f"{prefix}[{index}]")
    return checked


def verify_manifest_and_checksums() -> int:
    manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    require(manifest.get("schema") == "typed-json-carry-structured-publication-bundle/1", "unexpected bundle manifest schema")
    entries = manifest.get("files")
    require(isinstance(entries, list), "manifest files must be a list")
    expected: dict[str, dict[str, object]] = {}
    for entry in entries:
        require(isinstance(entry, dict), "manifest entry must be an object")
        name = entry.get("path")
        digest = entry.get("sha256")
        size = entry.get("size_bytes")
        mode = entry.get("mode")
        require(isinstance(name, str) and name not in expected, f"invalid/duplicate manifest path: {name}")
        require(isinstance(digest, str) and HEX64.fullmatch(digest) is not None, f"invalid manifest digest: {name}")
        require(isinstance(size, int) and size >= 0, f"invalid size in manifest: {name}")
        require(isinstance(mode, int), f"invalid mode in manifest: {name}")
        expected[name] = entry
        path = local_path(name)
        info = path.stat()
        require(info.st_size == size, f"size mismatch: {name}")
        require(stat.S_IMODE(info.st_mode) == mode, f"mode mismatch: {name}")
        require(sha256(path) == digest, f"SHA-256 mismatch: {name}")

    actual: set[str] = set()
    for path in ROOT.rglob("*"):
        if path.is_symlink():
            raise ValueError(f"symlink in publication bundle: {path.relative_to(ROOT)}")
        if path.is_file() and path.name != SUMS_PATH.name:
            actual.add(path.relative_to(ROOT).as_posix())
    require("bundle-manifest.json" in actual, "manifest missing from package inventory")
    require(actual == set(expected) | {"bundle-manifest.json"}, "package inventory differs from manifest")

    sums: dict[str, str] = {}
    for line_number, line in enumerate(SUMS_PATH.read_text(encoding="ascii").splitlines(), start=1):
        if not line:
            continue
        parts = line.split("  ", 1)
        require(len(parts) == 2 and HEX64.fullmatch(parts[0]) is not None, f"malformed checksum line {line_number}")
        digest, name = parts
        require(name not in sums, f"duplicate checksum path: {name}")
        sums[name] = digest
    expected_sum_paths = actual
    require(set(sums) == expected_sum_paths, "checksum inventory differs from package files")
    for name, digest in sums.items():
        require(sha256(local_path(name)) == digest, f"checksum mismatch: {name}")
    return len(actual)


def verify_integration_followup(source_map: dict[str, object], readme: str) -> int:
    followup = source_map.get("integration_followup")
    require(isinstance(followup, dict), "integration follow-up provenance is missing")
    commits = followup["commits"]
    require(isinstance(commits, dict), "integration commit map is malformed")
    for key in (
        "integration_base_commit",
        "typed_json_carry_integration_commit",
        "platform_cfg_followup_commit",
        "preexisting_literal_json_argument_borrow_commit",
    ):
        value = commits[key]
        require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) is not None, f"malformed integration commit: {key}")

    measurement = followup["measurement_scope"]
    require(measurement["base_commit"] == source_map["base_commit"], "integration metadata changes the measured base")
    require(measurement["candidate_product_patch_sha256"] == source_map["product_patch"]["sha256"], "integration metadata changes the measured product patch")
    require("not the measured benchmark arm" in measurement["warning"], "integration scope warning is missing")

    comparisons = followup["source_file_comparison"]
    measured = comparisons["measured_candidate_source_hashes"]
    candidate_files = {
        item["source_path"]: item["sha256"]
        for item in source_map["arms"]["candidate"]["files"]
        if item["source_path"] in measured
    }
    require(candidate_files == measured, "measured source comparison differs from the frozen candidate snapshots")
    expected_paths = set(measured)
    for label, key, path_key in (
        ("97df", "at_97df", "evidence_snapshot_path"),
        ("c719653", "at_c719653", "snapshot_path"),
    ):
        entries = comparisons[key]
        require(len(entries) == len(expected_paths), f"{label} source snapshot count mismatch")
        observed_paths: set[str] = set()
        matches = 0
        observed_hashes: dict[str, str] = {}
        for entry in entries:
            source = entry["source_path"]
            require(source in expected_paths and source not in observed_paths, f"unexpected/duplicate {label} source: {source}")
            observed_paths.add(source)
            digest = entry["sha256"]
            snapshot = local_path(entry[path_key])
            require(sha256(snapshot) == digest, f"{label} source snapshot digest mismatch: {source}")
            same = digest == measured[source]
            require(entry["matches_measured_candidate"] is same, f"{label} measured-source comparison mismatch: {source}")
            observed_hashes[source] = digest
            matches += int(same)
        require(observed_paths == expected_paths, f"{label} source inventory is incomplete")
        require(matches == (4 if label == "97df" else 2), f"unexpected {label} exact-source match count")

    first = {item["source_path"]: item["sha256"] for item in comparisons["at_97df"]}
    later = {item["source_path"]: item["sha256"] for item in comparisons["at_c719653"]}
    require(first["src/runtime.rs"] != measured["src/runtime.rs"], "97df runtime unexpectedly matches measured runtime")
    require(later["src/runtime.rs"] == first["src/runtime.rs"], "cfg follow-up unexpectedly changed runtime.rs")
    base_runtime = comparisons["base_7ce_runtime_rs"]
    base_digest = sha256(local_path(base_runtime["path"]))
    require(base_digest == base_runtime["sha256"] == base_runtime["a884_runtime_sha256"], "7ce/a884 literal-argument runtime snapshots differ")
    require(base_runtime["same_as_a884_literal_argument_commit_file"] is True, "literal-argument source comparison is not recorded")
    require(base_runtime["literal_argument_change_preexisted_typed_carry_integration"] is True, "preexisting literal-argument source fact is missing")
    require("97df did not introduce" in comparisons["interpretation"], "integration source interpretation misattributes literal argument borrowing")
    require("did not introduce the argument-borrow path" in readme, "README misattributes literal argument borrowing")
    require("../resolved-json-arguments-positive/README.md" in readme, "separate literal-argument benchmark link is missing")
    prior_trial = followup["prior_literal_argument_benchmark"]
    require(prior_trial["r_benchmarks_relative_path"] == "../resolved-json-arguments-positive/README.md", "literal-argument benchmark link path is incorrect")

    fixture = followup["fixture_only_followup"]
    fixture_patch = local_path(fixture["patch"]["path"])
    fixture_receipt_path = local_path(fixture["receipt"]["path"])
    fixture_receipt = json.loads(fixture_receipt_path.read_text(encoding="utf-8"))
    fixture_hash = sha256(fixture_patch)
    require(fixture_hash == fixture["patch"]["sha256"] == fixture["patch_sha256_expected"], "fixture-only patch hash mismatch")
    require(fixture_receipt["fixture_only_patch_sha256"] == fixture_hash, "fixture receipt does not bind its patch")
    require(fixture_receipt["measured_product_patch_sha256"] == measurement["candidate_product_patch_sha256"], "fixture receipt changes measured product identity")
    pre_snapshot = local_path(fixture["pre_fixture_snapshot_path"])
    new_snapshot = local_path(fixture["updated_test_snapshot_path"])
    pre_hash = sha256(pre_snapshot)
    new_hash = sha256(new_snapshot)
    require(pre_hash == fixture["pre_fixture_snapshot_sha256"] == fixture["pre_fixture_snapshot_sha256_actual"] == fixture_receipt["pre_fixture_snapshot_sha256"], "pre-fixture test source pin mismatch")
    require(new_hash == fixture["updated_test_sha256"] == fixture["updated_test_sha256_actual"] == fixture_receipt["updated_test_sha256"], "corrected fixture test source pin mismatch")
    fixture_source = new_snapshot.read_text(encoding="utf-8")
    require("CARGO_BIN_EXE_execute" in fixture_source and "cache_store" in fixture_source and "cache_hit" in fixture_source, "corrected CLI fixture does not retain its intended assertions")

    cfg = followup["cfg_only_followup"]
    cfg_patch_path = local_path(cfg["patch"]["path"])
    cfg_receipt_path = local_path(cfg["receipt"]["path"])
    cfg_receipt = json.loads(cfg_receipt_path.read_text(encoding="utf-8"))
    cfg_hash = sha256(cfg_patch_path)
    require(cfg_hash == cfg["patch"]["sha256"] == cfg_receipt["patch"]["sha256"], "cfg-only patch pin mismatch")
    require(cfg["scope"] == cfg_receipt["scope"], "cfg-only patch scope differs from receipt")
    for filename, source in (("contracts.rs", "src/contracts.rs"), ("manifest.rs", "src/manifest.rs")):
        record = cfg["changed_files"][filename]
        receipt_record = cfg_receipt["files"][filename]
        require(record["before_sha256"] == receipt_record["before_sha256"] == first[source], f"cfg before-source pin mismatch: {filename}")
        require(record["after_sha256"] == receipt_record["after_sha256"] == later[source], f"cfg after-source pin mismatch: {filename}")

    validation = followup["validation"]
    results = validation["reported_results"]
    require(results["library_tests_passed"] == 313 and results["selected_integration_tests_passed"] == 35, "integration test totals changed")
    require(results["typed_contract_tests_after_cli_fixture_fix"] == "9 passed, 0 failed", "corrected typed-contract result mismatch")
    validation_text: dict[str, str] = {}
    for artifact in validation["artifacts"]:
        artifact_path = local_path(artifact["path"])
        require(sha256(artifact_path) == artifact["sha256"], f"integration validation log hash mismatch: {artifact['path']}")
        validation_text[Path(artifact["path"]).name] = artifact_path.read_text(encoding="utf-8", errors="replace")
    require("deep_in_process_json_binding_matches_cache_hit_byte_decoding ... FAILED" in validation_text["pre-fixture-full-test.log"], "preserved pre-fixture failure is not disclosed by its log")
    require("bundled execute-applet helper missing" in validation_text["pre-fixture-full-test.log"], "pre-fixture helper-discovery issue is absent")
    require("test result: ok. 9 passed; 0 failed" in validation_text["typed-contracts-after-fixture-fix.log"], "corrected typed-contract run did not pass")
    return 1 + len(first) + len(later) + len(validation["artifacts"])


def verify_payload() -> tuple[int, int, int]:
    attributes = (ROOT / ".gitattributes").read_text(encoding="utf-8")
    require("evidence/** -text" in attributes and "*.bin -text -diff" in attributes, "Git byte-preservation rules are missing")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    index = (ROOT / "INDEX.snippet.md").read_text(encoding="utf-8")
    require("../typed-json-carry-scalar-mixed-20261016/README.md" in readme, "README companion-trial link does not match the R/benchmarks layout")
    require("typed-json-carry-structured-20261016/README.md" in index and "typed-json-carry-scalar-mixed-20261016/README.md" in index, "index snippet links do not match the R/benchmarks layout")
    source_map = json.loads((ROOT / "source-map.json").read_text(encoding="utf-8"))
    require(source_map.get("scalar_context", {}).get("bundle_relative_path") == "../typed-json-carry-scalar-mixed-20261016", "source-map companion-trial path does not match the R/benchmarks layout")
    require(source_map.get("publication_state", "").startswith("frozen;"), "source map is not marked frozen")
    source_freeze = json.loads(local_path(source_map["source_freeze"]["path"]).read_text(encoding="utf-8"))
    expected_commit = source_freeze["base_commit"]
    require(re.fullmatch(r"[0-9a-f]{40}", expected_commit) is not None, "source-freeze base commit is malformed")
    require(source_map["base_commit"] == expected_commit, "source-map base commit differs from source freeze")
    require(all(source_map["arms"][arm]["head"] == expected_commit for arm in ("baseline", "candidate")), "arm commit identity differs from source freeze")
    known_hashes: set[str] = set()

    def collect_hashes(value: object) -> None:
        if isinstance(value, dict):
            for child in value.values():
                collect_hashes(child)
        elif isinstance(value, list):
            for child in value:
                collect_hashes(child)
        elif isinstance(value, str) and len(value) in (40, 64) and re.fullmatch(r"[0-9a-f]+", value.lower()):
            known_hashes.add(value.lower())

    collect_hashes(source_map)
    collect_hashes(source_freeze)
    for document in ("README.md", "INDEX.snippet.md"):
        document_text = (ROOT / document).read_text(encoding="utf-8")
        tokens = re.findall(r"(?i)(?<![0-9a-f])[0-9a-f]{32,64}(?![0-9a-f])", document_text)
        for token in tokens:
            require(len(token) in (40, 64), f"malformed hash-like token in {document}: {token}")
            require(token.lower() in known_hashes, f"unbound hash-like token in {document}: {token}")
    digest_count = check_source_map_digest_values(source_map)
    integration_pin_count = verify_integration_followup(source_map, readme)

    # Validate every declared pin whose path and SHA-256 are explicitly paired.
    def check_paired_paths(value: object, prefix: str = "source-map") -> int:
        total = 0
        if isinstance(value, dict):
            if isinstance(value.get("path"), str) and isinstance(value.get("sha256"), str):
                path = local_path(value["path"])
                require(sha256(path) == value["sha256"], f"source-map path/hash mismatch at {prefix}")
                total += 1
            for key, child in value.items():
                total += check_paired_paths(child, f"{prefix}.{key}")
        elif isinstance(value, list):
            for index, child in enumerate(value):
                total += check_paired_paths(child, f"{prefix}[{index}]")
        return total

    paired_path_pins = check_paired_paths(source_map)
    fingerprints_path = local_path(source_map["source_fingerprints"]["path"])
    fingerprints = json.loads(fingerprints_path.read_text(encoding="utf-8"))
    require(fingerprints["schema"] == "typed-json-carry-structured-source-fingerprints/1", "unexpected source fingerprint schema")
    for arm_name in ("baseline", "candidate"):
        source_arm = fingerprints["arms"][arm_name]
        map_arm = source_map["arms"][arm_name]
        tree = source_arm["tree"]
        require(tree["file_count"] == map_arm["file_count"] == 2197, f"{arm_name} full-source count mismatch")
        require(tree["sha256"] == map_arm["tree_sha256"], f"{arm_name} source tree pin mismatch")
        require(source_arm["tracked_diff_sha256"] == map_arm["tracked_diff_sha256"], f"{arm_name} tracked-diff pin mismatch")
        require(len(tree["files"]) == 2197, f"{arm_name} source fingerprint inventory is incomplete")
        require(all(HEX64.fullmatch(digest) is not None for digest in tree["files"].values()), f"invalid full source digest in {arm_name}")
        for source in map_arm["files"]:
            source_path = source["source_path"]
            snapshot_path = local_path(source["snapshot_path"])
            expected_digest = source["sha256"]
            require(tree["files"].get(source_path) == expected_digest, f"snapshot is not bound to full map: {arm_name}/{source_path}")
            require(sha256(snapshot_path) == expected_digest, f"snapshot hash mismatch: {arm_name}/{source_path}")
        receipt_path = local_path(map_arm["build_receipt_path"])
        binary_path = local_path(map_arm["saved_binary_path"])
        require(sha256(receipt_path) == map_arm["build_receipt_sha256"], f"{arm_name} build-receipt hash mismatch")
        require(sha256(binary_path) == map_arm["saved_binary_sha256"], f"{arm_name} binary hash mismatch")
        require(stat.S_IMODE(binary_path.stat().st_mode) & 0o111, f"{arm_name} saved binary lacks executable bit")

    sums = ROOT / "evidence/typed-json-carry-structured/SHA256SUMS.txt"
    for line_number, line in enumerate(sums.read_text(encoding="ascii").splitlines(), start=1):
        digest, relative = line.split("  ", 1)
        require(HEX64.fullmatch(digest) is not None, f"invalid historical sum at line {line_number}")
        require(sha256(local_path(relative)) == digest, f"historical evidence checksum mismatch: {relative}")

    audit1 = json.loads(local_path(source_map["independent_audit_attempt_1"]["path"]).read_text(encoding="utf-8"))
    audit2 = json.loads(local_path(source_map["independent_audit_attempt_2"]["path"]).read_text(encoding="utf-8"))
    require(audit1["result"] == audit2["result"] == "pass", "independent audit did not pass")
    require(audit1["audit_script_sha256"] == source_map["audit_script_attempt_1"]["sha256"], "audit attempt 1 script pin mismatch")
    require(len(audit2["invocations"]) == 102, "audit invocation inventory mismatch")
    require(audit1["checks_passed"] == audit2["checks_passed"] == 3916, "audit check count mismatch")
    require(audit2["audit_script_sha256"] == source_map["audit_script_attempt_2"]["sha256"], "final audit script pin mismatch")
    require(audit2["evidence"]["report_sha256"] == source_map["independent_audit_attempt_2"]["paired_report_sha256"], "audit report digest differs from source map")
    summary = audit2["summary"]
    require(summary["attempt_count"] == summary["successful_attempt_count"] == 102, "attempt count mismatch")
    require(summary["measurement_pairs"] == 48 and summary["candidate_wins"] == 48, "measurement outcome mismatch")
    require(summary["candidate_losses"] == summary["ties"] == 0, "unexpected losses/ties")
    require(summary["baseline_invocation_median_ns_exact"] == "54809833.5", "baseline exact median mismatch")
    require(summary["candidate_invocation_median_ns_exact"] == "40102312.5", "candidate exact median mismatch")
    require(summary["median_candidate_minus_baseline_ns_exact"] == "-14221270.5", "exact paired delta mismatch")
    require(source_map["independent_audit_attempt_2"]["sha256"] == "36b8eec7645b98f836e1f00c9655135f6ecd9e653f7097de0c42b71b31d89b0f", "audit attempt 2 checksum pin mismatch")

    review1 = json.loads(local_path(source_map["adversarial_review_attempt_1"]["path"]).read_text(encoding="utf-8"))
    review2 = json.loads(local_path(source_map["adversarial_review_attempt_2"]["path"]).read_text(encoding="utf-8"))
    require(review1["review"] != "pass" and source_map["adversarial_review_attempt_1"]["superseded"] is True, "prior adversarial receipt not identified as superseded")
    require(review2["review"] == "pass; no material blocker", "final adversarial review did not pass")
    require(review2["check_count"] == len(review2["checks"]) == 10 and all(review2["checks"].values()), "final adversarial checks incomplete")
    require(review1["inputs"]["audit_attempt_1_sha256"] == source_map["independent_audit_attempt_1"]["sha256"], "first review does not bind first audit")
    require(review2["inputs"]["attempt_2_sha256"] == source_map["independent_audit_attempt_2"]["sha256"], "review does not bind final audit")

    attempt_root = ROOT / "evidence/typed-json-carry-structured/paired-run-attempt-1/attempts"
    attempt_dirs = [path for path in attempt_root.iterdir() if path.is_dir()]
    require(len(attempt_dirs) == 102, "raw attempt directory count mismatch")
    for directory in attempt_dirs:
        for filename in ("raw.json", "stdout.log", "stderr.log", "started.json", "result.json"):
            require((directory / filename).is_file(), f"missing raw attempt artifact: {directory.name}/{filename}")
    return digest_count, paired_path_pins, integration_pin_count


def main() -> int:
    try:
        file_count = verify_manifest_and_checksums()
        digest_count, path_pin_count, integration_check_count = verify_payload()
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        print(f"bundle verification failed: {error}", file=sys.stderr)
        return 1
    print(f"bundle verification passed: {file_count} files; {digest_count} source-map SHA-256 fields; {path_pin_count} explicit path/hash pins; {integration_check_count} integration-provenance checks; source snapshots, both audits, both reviews, build binaries, and 102 raw attempts checked")
    print("No product executable was run and no build was performed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
