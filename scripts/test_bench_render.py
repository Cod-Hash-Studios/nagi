from __future__ import annotations

import importlib.util
import contextlib
import io
import json
import os
import socket
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/bench_render.py"


def load_benchmark_module():
    spec = importlib.util.spec_from_file_location("bench_render", SCRIPT)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class BenchmarkEntrypointTests(unittest.TestCase):
    def test_benchmark_entrypoints_exist(self) -> None:
        self.assertTrue((ROOT / "scripts/bench_startup.sh").is_file())
        self.assertTrue((ROOT / "scripts/bench_render.py").is_file())

    def test_shell_entrypoint_never_builds_inside_the_measurement(self) -> None:
        wrapper = (ROOT / "scripts/bench_startup.sh").read_text()
        self.assertNotIn("cargo build", wrapper)
        self.assertIn("bench_render.py", wrapper)

    def test_justfile_has_separate_build_baseline_json_and_smoke_recipes(self) -> None:
        justfile = (ROOT / "justfile").read_text()

        self.assertIn("bench-build:", justfile)
        self.assertIn("bench: bench-build", justfile)
        self.assertIn("bench-json: bench-build", justfile)
        self.assertIn("bench-smoke: bench-build", justfile)
        self.assertIn("NAGI_BENCH_STARTUP_CEILING_MS", justfile)


class StatisticsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.bench = load_benchmark_module()

    def test_summary_uses_nearest_rank_p95_and_keeps_raw_samples(self) -> None:
        summary = self.bench.summarize_samples([1.0, 2.0, 3.0, 100.0])

        self.assertEqual(summary["samples"], [1.0, 2.0, 3.0, 100.0])
        self.assertEqual(summary["count"], 4)
        self.assertEqual(summary["p50"], 2.0)
        self.assertEqual(summary["p95"], 100.0)
        self.assertEqual(summary["max"], 100.0)

    def test_ceiling_failure_names_metric_measurement_and_limit(self) -> None:
        results = {
            "startup_ms": {"p95": 12.5},
            "render_latency_ms": {"p95": 4.0},
            "reattach_ms": {"p95": 8.0},
            "idle_cpu_percent": 0.2,
            "process_tree_rss_mib": 20.0,
        }
        ceilings = {
            "startup_p95_ms": 0.001,
            "render_p95_ms": 10.0,
            "reattach_p95_ms": None,
            "idle_cpu_percent": None,
            "process_tree_rss_mib": None,
        }

        failures = self.bench.evaluate_ceilings(results, ceilings)

        self.assertEqual(len(failures), 1)
        self.assertEqual(failures[0]["metric"], "startup_p95_ms")
        self.assertEqual(failures[0]["measured"], 12.5)
        self.assertEqual(failures[0]["ceiling"], 0.001)


class ProtocolTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.bench = load_benchmark_module()

    def test_hello_requests_terminal_ansi_with_explicit_geometry(self) -> None:
        framed = self.bench.encode_client_hello(protocol=16, cols=120, rows=40)
        payload_size = struct.unpack("<I", framed[:4])[0]
        payload = framed[4:]

        self.assertEqual(payload_size, len(payload))
        values = []
        offset = 0
        for _ in range(8):
            value, offset = self.bench.decode_varint(payload, offset)
            values.append(value)
        self.assertEqual(values, [0, 16, 120, 40, 0, 0, 1, 0])
        launch_mode, offset = self.bench.decode_varint(payload, offset)
        self.assertEqual(launch_mode, 0)
        self.assertEqual(offset, len(payload))

    def test_terminal_message_decoder_returns_real_ansi_bytes(self) -> None:
        ansi = b"\x1b[2Jrender-marker"
        payload = b"".join(
            [
                self.bench.encode_varint(2),
                self.bench.encode_varint(7),
                self.bench.encode_varint(120),
                self.bench.encode_varint(40),
                b"\x01",
                self.bench.encode_varint(len(ansi)),
                ansi,
            ]
        )

        message = self.bench.decode_server_message(payload)

        self.assertEqual(message["kind"], "terminal")
        self.assertEqual(message["seq"], 7)
        self.assertEqual(message["width"], 120)
        self.assertEqual(message["height"], 40)
        self.assertTrue(message["full"])
        self.assertEqual(message["bytes"], ansi)

    def test_terminal_text_removes_cursor_sequences_between_each_character(self) -> None:
        ansi = b"\x1b[2;43HN\x1b[2;44HA\x1b[2;45HG\x1b[2;46HI"

        self.assertEqual(self.bench.terminal_text(ansi), b"NAGI")

    def test_render_probe_resets_the_viewport_before_printing_marker(self) -> None:
        command = self.bench.render_probe_command("NAGI_BENCH_MARKER")

        self.assertIn(b"\\033[2J\\033[H", command)
        self.assertIn(b"NAGI_BENCH_MARKER", command)
        self.assertTrue(command.endswith(b"\n"))

    def test_render_probe_markers_change_every_cell_between_samples(self) -> None:
        first = self.bench.render_probe_marker(0)
        second = self.bench.render_probe_marker(1)

        self.assertEqual(len(first), len(second))
        self.assertTrue(all(left != right for left, right in zip(first, second)))

    def test_framed_reader_reassembles_a_real_socket_message(self) -> None:
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        payload = b"terminal-frame"
        framed = struct.pack("<I", len(payload)) + payload
        right.sendall(framed[:3])
        right.sendall(framed[3:])

        self.assertEqual(self.bench.read_framed(left, timeout=0.5), payload)


class EnvironmentTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.bench = load_benchmark_module()

    def test_isolated_environment_removes_session_and_socket_inheritance(self) -> None:
        inherited = {
            "PATH": os.environ.get("PATH", ""),
            "NAGI_SESSION": "real-session",
            "NAGI_CLIENT_SOCKET_PATH": "/tmp/real-client.sock",
            "NAGI_ENV": "secret-state",
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            env = self.bench.isolated_environment(Path(temp_dir), inherited)

        self.assertNotIn("NAGI_SESSION", env)
        self.assertNotIn("NAGI_CLIENT_SOCKET_PATH", env)
        self.assertNotIn("NAGI_ENV", env)
        self.assertTrue(env["NAGI_SOCKET_PATH"].endswith("/runtime/nagi.sock"))
        self.assertEqual(env["NAGI_DISABLE_SOUND"], "1")

    def test_process_group_snapshot_reports_current_process(self) -> None:
        snapshot = self.bench.process_group_snapshot(os.getpgrp())

        self.assertIn(os.getpid(), snapshot["pids"])
        self.assertGreater(snapshot["rss_bytes"], 0)
        self.assertGreaterEqual(snapshot["cpu_seconds"], 0)

    def test_process_tree_snapshot_includes_a_child_with_its_own_process_group(self) -> None:
        child = subprocess.Popen(
            ["python3", "-c", "import time; time.sleep(5)"],
            start_new_session=True,
        )
        self.addCleanup(child.wait)
        self.addCleanup(child.terminate)

        snapshot = self.bench.process_tree_snapshot(os.getpid())

        self.assertIn(os.getpid(), snapshot["pids"])
        self.assertIn(child.pid, snapshot["pids"])


class CommandLineTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.bench = load_benchmark_module()

    def test_tiny_positive_ceiling_is_accepted(self) -> None:
        args = self.bench.parse_args(
            [
                "--binary",
                "/tmp/nagi",
                "--startup-ceiling-ms",
                "0.001",
                "--format",
                "json",
            ]
        )

        self.assertEqual(args.startup_ceiling_ms, 0.001)
        self.assertEqual(args.output_format, "json")

    def test_zero_samples_are_rejected(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                self.bench.parse_args(["--startup-samples", "0"])

    def test_human_report_exposes_scenario_metadata_and_gate_status(self) -> None:
        report = {
            "status": "pass",
            "metadata": {
                "repository": {"commit": "abc123", "dirty": False},
                "binary": {"path": "/tmp/nagi", "version": "nagi 1.0"},
                "system": {"os": "Linux", "machine": "x86_64", "cpu": "test cpu"},
            },
            "scenario": {
                "cols": 120,
                "rows": 40,
                "panes": 1,
                "startup_samples": 3,
                "render_samples": 3,
                "compilation_included": False,
            },
            "results": {
                "startup_ms": {"p50": 10.0, "p95": 12.0, "max": 12.0},
                "render_latency_ms": {"p50": 2.0, "p95": 3.0, "max": 3.0},
                "reattach_ms": {"p50": 4.0, "p95": 5.0, "max": 5.0},
                "idle_cpu_percent": 0.1,
                "server_rss_mib": 10.0,
                "process_tree_rss_mib": 12.0,
            },
            "ceilings": {},
            "failures": [],
        }

        rendered = self.bench.format_human(report)

        self.assertIn("PASS", rendered)
        self.assertIn("abc123", rendered)
        self.assertIn("120x40", rendered)
        self.assertIn("compilation excluded", rendered)

    def test_json_cli_error_is_machine_readable_when_binary_is_missing(self) -> None:
        completed = subprocess.run(
            [
                "python3",
                str(SCRIPT),
                "--binary",
                "/definitely/missing/nagi",
                "--format",
                "json",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 2)
        payload = json.loads(completed.stdout)
        self.assertEqual(payload["status"], "error")
        self.assertIn("prebuilt binary", payload["error"])
        self.assertEqual(completed.stderr, "")


if __name__ == "__main__":
    unittest.main()
