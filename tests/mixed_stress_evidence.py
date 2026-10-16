#!/usr/bin/env python3
"""Unit tests for the non-measuring mixed graph stress evidence validator."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from benchmarks import stress_evidence as harness


class MixedStressEvidenceTests(unittest.TestCase):
    def test_mixed_window_cli_bounds_include_five_minutes_only(self):
        for value in (100, 1_000, 10_000, 300_000):
            with self.subTest(value=value):
                harness._validate_mixed_window_ms(value)
        for value in (99, 300_001, True, "300000"):
            with self.subTest(value=value), self.assertRaisesRegex(
                ValueError, "--window-ms must be"
            ):
                harness._validate_mixed_window_ms(value)

    def test_full_mixed_report_validator_accepts_bounded_five_minute_window(self):
        args = argparse.Namespace(nodes=32, window_ms=300_000)
        report = self.valid_report(args.nodes, args.window_ms)
        summary = harness._validate_mixed(report, args)
        self.assertEqual(summary["active_window_ms"], 300_000)
        self.assertEqual(summary["active_window_observed_us"], 300_000_000)

    def test_workflow_rejects_three_five_minute_trials_before_checkout_or_smoke(self):
        workflow_path = ROOT / ".github/workflows/mixed-runtime-probe.yml"
        workflow = workflow_path.read_text()
        validation_name = "Validate requested 1,000-actor trial bounds"
        self.assertIn(f"      - name: {validation_name}\n", workflow)
        validation_step = workflow.split(validation_name, 1)[1].split("\n      - ", 1)[0]
        self.assertIn("        if: inputs.run_1000\n", validation_step)
        self.assertLess(workflow.index(validation_name), workflow.index("uses: actions/checkout@"))
        self.assertLess(workflow.index(validation_name), workflow.index("Run the 32-actor correctness smoke"))
        self.assertIn('          - "300000"', workflow)
        input_definitions = workflow.split("jobs:", 1)[0]
        window_input = input_definitions.split("      window_ms:", 1)[1].split(
            "      replications:", 1
        )[0]
        replication_input = input_definitions.split("      replications:", 1)[1].split(
            "\n\npermissions:", 1
        )[0]
        self.assertIn('default: "1000"', window_input)
        self.assertIn('default: "1"', replication_input)

        step = workflow.split(f"      - name: {validation_name}\n", 1)[1].split("\n      - ", 1)[0]
        run_marker = "        run: |\n"
        self.assertIn(run_marker, step)
        run_body = step.split(run_marker, 1)[1]
        script_lines = []
        for line in run_body.splitlines():
            if line.startswith("          "):
                script_lines.append(line[10:])
            elif not line.strip():
                script_lines.append("")
            else:
                break
        script = "\n".join(script_lines) + "\n"
        for window_ms, replications, expected_returncode in (
            ("1000", "1", 0),
            ("10000", "3", 0),
            ("300000", "1", 0),
            ("300000", "3", 2),
            ("300001", "1", 2),
        ):
            with self.subTest(window_ms=window_ms, replications=replications):
                result = subprocess.run(
                    ["bash", "-c", script],
                    env={
                        **os.environ,
                        "MIXED_WINDOW_MS": window_ms,
                        "MIXED_REPLICATIONS": replications,
                    },
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, expected_returncode, result.stderr)

    def ready_receipt(self, timer_count: int) -> dict[str, object]:
        return {
            "status": "ready",
            "timeout_ms": 10_000,
            "warmup_elapsed_us": 42,
            "watchdog_status": "stopped_after_ready",
            "watchdog_joined": True,
            "active_window_started_after_readiness": True,
            "timer_events_consumed_at_barrier": 2,
            "observed_timer_source_ids": [
                f"timer_{index:04d}" for index in range(timer_count)
            ],
            "missing_actor_sources": [],
        }

    def valid_report(self, nodes: int, window_ms: int) -> dict[str, object]:
        layout = harness._mixed_actor_layout(nodes)
        timer_ids = [f"timer_{index:04d}" for index in range(layout["timers"])]
        return {
            "schema": "execute.graph-mixed-lifecycle.raw/1",
            "trial_type": "measurement",
            "status": "passed",
            "parameters": {
                "total_graph_actors": nodes,
                "active_window_ms": window_ms,
            },
            "actor_layout": layout,
            "readiness": self.ready_receipt(layout["timers"]),
            "timings_us": {
                "actor_readiness_warmup": 42,
                "active_window_observed": window_ms * 1_000,
            },
            "graph": {
                "node_count": nodes,
                "edge_count": layout["timers"],
                "max_parallel_admission": nodes,
                "max_node_admission": nodes,
                "lossless_timer_edge_capacity": 1,
                "listener_reader_admission": layout["loopback_remote_listeners"],
                "graph_deadline_budget_ms": window_ms + 20_000,
                "active_window_observation_tolerance_ms": 1_000,
            },
            "correctness": {
                "expected_graph_actor_count": nodes,
                "all_actor_sources_ready_before_active_window": True,
                "all_native_finite_nodes_emitted": True,
                "all_worker_nodes_emitted": True,
                "all_in_process_nodes_emitted": True,
                "all_program_services_emitted": True,
                "all_remote_listeners_emitted": True,
                "all_timer_nodes_emitted": True,
                "readiness_watchdog_joined": True,
                "active_window_duration_at_least_requested": True,
                "active_window_duration_within_graph_deadline": True,
                "timer_event_counter_monotonic": True,
                "active_timer_counter_matches_window_delta": True,
                "positive_active_timer_progress": True,
                "all_expected_timer_sources_progressed_during_active_window": True,
                "active_window_watchdog_fired": True,
                "active_window_watchdog_joined": True,
                "timer_events_consumed": True,
                "remote_requests_complete": True,
                "worker_pool_capacity_respected": True,
                "workers_alive_before_registry_drop": True,
                "workers_reaped_after_registry_drop": True,
                "graph_metrics_match_native_plus_worker_starts": True,
                "threads_returned_to_baseline_plus_two": True,
                "cancel_join_and_helper_threads_succeeded": True,
                "unexpected_output_reader_error": None,
            },
            "outputs": {
                "distinct_nodes_with_terminal_output": {
                    "native": layout["native_finite_applets"],
                    "worker": layout["distinct_registry_workers"],
                    "in_process": layout["in_process_applets"],
                    "service_event": layout["scheduled_program_services"],
                    "remote_event": layout["loopback_remote_listeners"],
                },
                "distinct_timer_sources_with_output": layout["timers"],
                "timer_source_ids_with_output": timer_ids,
                "missing_timer_source_ids": [],
                "unexpected_timer_source_ids": [],
                "timer_events_consumed_by_capacity_one_lossless_collector": 10,
                "timer_events_consumed_total_including_warmup": 10,
                "timer_events_consumed_at_readiness_barrier": 2,
                "timer_events_consumed_before_active_window": 3,
                "timer_events_consumed_at_active_window_end": 8,
                "timer_events_consumed_during_active_window": 5,
                "timer_events_recorded_during_active_window": 5,
                "timer_events_consumed_after_active_window_end": 2,
                "active_timer_source_ids": list(timer_ids),
                "missing_active_timer_source_ids": [],
                "unexpected_active_timer_source_id": None,
                "active_window_watchdog_fired": True,
                "active_window_watchdog_joined": True,
            },
            "remote_fixture": {"requests": 2 * layout["loopback_remote_listeners"]},
            "workers": {
                "worker_pids": list(range(2000, 2000 + layout["distinct_registry_workers"])),
                "all_reaped_after_registry_drop": True,
                "ownership": "synthetic validator receipt",
            },
            "resources": {
                "peak_sampled": {},
                "launched_processes_reported_by_graph_metrics": (
                    layout["native_finite_applets"] + layout["distinct_registry_workers"]
                ),
            },
        }

    def test_readiness_requires_all_exact_timer_ids_and_joined_watchdog(self):
        receipt = self.ready_receipt(timer_count=4)
        self.assertEqual(harness._validate_mixed_readiness(receipt, 4), 42)

        receipt["observed_timer_source_ids"] = ["timer_0000", "timer_0001", "timer_0003"]
        with self.assertRaisesRegex(ValueError, "every expected actor source"):
            harness._validate_mixed_readiness(receipt, 4)

        receipt = self.ready_receipt(timer_count=4)
        receipt["watchdog_joined"] = False
        with self.assertRaisesRegex(ValueError, "watchdog did not stop and join"):
            harness._validate_mixed_readiness(receipt, 4)

    def test_readiness_cannot_claim_success_after_watchdog_timeout(self):
        receipt = self.ready_receipt(timer_count=1)
        receipt["watchdog_status"] = "deadline_cancelled_graph"
        with self.assertRaisesRegex(ValueError, "watchdog did not stop and join"):
            harness._validate_mixed_readiness(receipt, 1)

    def test_full_mixed_report_accepts_exact_ids_and_rejects_malformed_timer_ids(self):
        args = argparse.Namespace(nodes=32, window_ms=1_000)
        report = self.valid_report(args.nodes, args.window_ms)
        summary = harness._validate_mixed(report, args)
        self.assertEqual(summary["actor_count"], args.nodes)

        report = self.valid_report(args.nodes, args.window_ms)
        report["outputs"]["timer_source_ids_with_output"].pop()
        with self.assertRaisesRegex(ValueError, "unexpected timer source identity set"):
            harness._validate_mixed(report, args)

    def test_full_mixed_report_rejects_zero_or_short_active_duration(self):
        args = argparse.Namespace(nodes=32, window_ms=1_000)
        for elapsed in (0, 999_999):
            with self.subTest(elapsed=elapsed):
                report = self.valid_report(args.nodes, args.window_ms)
                report["timings_us"]["active_window_observed"] = elapsed
                with self.assertRaisesRegex(ValueError, "shorter than requested"):
                    harness._validate_mixed(report, args)

        report = self.valid_report(args.nodes, args.window_ms)
        report["timings_us"]["active_window_observed"] = 22_000_001
        with self.assertRaisesRegex(ValueError, "exceeded its graph-deadline observation bound"):
            harness._validate_mixed(report, args)

    def test_full_mixed_report_rejects_ready_only_active_window(self):
        args = argparse.Namespace(nodes=32, window_ms=1_000)
        report = self.valid_report(args.nodes, args.window_ms)
        outputs = report["outputs"]
        outputs["timer_events_consumed_at_active_window_end"] = 3
        outputs["timer_events_consumed_during_active_window"] = 0
        outputs["timer_events_recorded_during_active_window"] = 0
        outputs["timer_events_consumed_after_active_window_end"] = 7
        with self.assertRaisesRegex(ValueError, "no timer progress"):
            harness._validate_mixed(report, args)

    def test_full_mixed_report_requires_active_watchdog_join_receipt(self):
        args = argparse.Namespace(nodes=32, window_ms=1_000)
        report = self.valid_report(args.nodes, args.window_ms)
        report["outputs"]["active_window_watchdog_joined"] = False
        report["correctness"]["active_window_watchdog_joined"] = False
        with self.assertRaisesRegex(ValueError, "failed active_window_watchdog_joined"):
            harness._validate_mixed(report, args)

    def test_full_mixed_report_rejects_invalid_or_non_monotonic_counters(self):
        args = argparse.Namespace(nodes=32, window_ms=1_000)
        report = self.valid_report(args.nodes, args.window_ms)
        report["outputs"]["timer_events_consumed_before_active_window"] = -1
        with self.assertRaisesRegex(ValueError, "invalid nonnegative counter"):
            harness._validate_mixed(report, args)

        report = self.valid_report(args.nodes, args.window_ms)
        report["outputs"]["timer_events_consumed_before_active_window"] = 9
        with self.assertRaisesRegex(ValueError, "non-monotonic"):
            harness._validate_mixed(report, args)

        report = self.valid_report(args.nodes, args.window_ms)
        report["outputs"]["timer_events_recorded_during_active_window"] = 4
        with self.assertRaisesRegex(ValueError, "exact counter delta"):
            harness._validate_mixed(report, args)

    def test_full_mixed_report_rejects_active_window_without_every_timer(self):
        args = argparse.Namespace(nodes=32, window_ms=1_000)
        report = self.valid_report(args.nodes, args.window_ms)
        report["outputs"]["active_timer_source_ids"].pop()
        with self.assertRaisesRegex(ValueError, "every expected timer source"):
            harness._validate_mixed(report, args)


if __name__ == "__main__":
    unittest.main(verbosity=2)
