#!/usr/bin/env python3

from __future__ import annotations

import argparse
import ctypes
import datetime as dt
import hashlib
import json
import math
import os
import platform
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Mapping, Sequence


ROOT = Path(__file__).resolve().parents[1]


class BenchmarkError(RuntimeError):
    pass


MAX_WIRE_FRAME_BYTES = 32 * 1024 * 1024
OSC_SEQUENCE = re.compile(rb"\x1b\].*?(?:\x07|\x1b\\)", re.DOTALL)
CSI_SEQUENCE = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
SHORT_ESCAPE = re.compile(rb"\x1b[@-_]")


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def nonnegative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be zero or greater")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("must be a finite number greater than zero")
    return parsed


def nonnegative_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < 0:
        raise argparse.ArgumentTypeError("must be a finite number zero or greater")
    return parsed


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Measure Nagi process startup, warm reattach, input-to-frame rendering, "
            "idle CPU, and resident memory. The binary must already be built; "
            "compilation is never included in timings."
        )
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=ROOT / "target/release/nagi",
        help="prebuilt Nagi executable (default: target/release/nagi)",
    )
    parser.add_argument("--repo-root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    parser.add_argument("--startup-samples", type=positive_int, default=10)
    parser.add_argument("--render-samples", type=positive_int, default=30)
    parser.add_argument("--warmups", type=nonnegative_int, default=2)
    parser.add_argument("--idle-settle-seconds", type=nonnegative_float, default=30.0)
    parser.add_argument("--idle-sample-seconds", type=positive_float, default=5.0)
    parser.add_argument("--timeout-seconds", type=positive_float, default=15.0)
    parser.add_argument("--cols", type=positive_int, default=120)
    parser.add_argument("--rows", type=positive_int, default=40)
    parser.add_argument("--panes", type=positive_int, default=1)
    parser.add_argument(
        "--format",
        dest="output_format",
        choices=("human", "json"),
        default="human",
    )
    parser.add_argument("--startup-ceiling-ms", type=positive_float)
    parser.add_argument("--render-ceiling-ms", type=positive_float)
    parser.add_argument("--reattach-ceiling-ms", type=positive_float)
    parser.add_argument("--idle-cpu-ceiling-percent", type=positive_float)
    parser.add_argument("--rss-ceiling-mib", type=positive_float)
    return parser.parse_args(argv)


def summarize_samples(samples: Sequence[float]) -> dict[str, object]:
    if not samples:
        raise ValueError("at least one sample is required")
    ordered = sorted(float(sample) for sample in samples)

    def nearest_rank(percentile: float) -> float:
        rank = max(1, math.ceil(percentile * len(ordered)))
        return ordered[rank - 1]

    return {
        "count": len(ordered),
        "samples": [round(sample, 3) for sample in samples],
        "min": round(ordered[0], 3),
        "p50": round(nearest_rank(0.50), 3),
        "p95": round(nearest_rank(0.95), 3),
        "max": round(ordered[-1], 3),
    }


def evaluate_ceilings(
    results: Mapping[str, object], ceilings: Mapping[str, float | None]
) -> list[dict[str, float | str]]:
    measurements = {
        "startup_p95_ms": float(results["startup_ms"]["p95"]),
        "render_p95_ms": float(results["render_latency_ms"]["p95"]),
        "reattach_p95_ms": float(results["reattach_ms"]["p95"]),
        "idle_cpu_percent": float(results["idle_cpu_percent"]),
        "process_tree_rss_mib": float(results["process_tree_rss_mib"]),
    }
    failures = []
    for metric, ceiling in ceilings.items():
        if ceiling is None:
            continue
        measured = measurements[metric]
        if measured > ceiling:
            failures.append(
                {
                    "metric": metric,
                    "measured": measured,
                    "ceiling": float(ceiling),
                }
            )
    return failures


def encode_varint(value: int) -> bytes:
    if value < 0:
        raise ValueError("varints cannot be negative")
    if value < 251:
        return bytes((value,))
    if value <= 0xFFFF:
        return b"\xfb" + struct.pack("<H", value)
    if value <= 0xFFFFFFFF:
        return b"\xfc" + struct.pack("<I", value)
    if value <= 0xFFFFFFFFFFFFFFFF:
        return b"\xfd" + struct.pack("<Q", value)
    if value <= (1 << 128) - 1:
        return b"\xfe" + value.to_bytes(16, "little")
    raise ValueError("varint exceeds u128")


def decode_varint(payload: bytes, offset: int = 0) -> tuple[int, int]:
    if offset >= len(payload):
        raise ValueError("payload is too short for a varint")
    tag = payload[offset]
    if tag <= 250:
        return tag, offset + 1
    widths = {251: 2, 252: 4, 253: 8, 254: 16}
    if tag not in widths:
        raise ValueError(f"unsupported varint tag {tag}")
    width = widths[tag]
    end = offset + 1 + width
    if end > len(payload):
        raise ValueError("payload is too short for the tagged varint")
    return int.from_bytes(payload[offset + 1 : end], "little"), end


def frame_payload(payload: bytes) -> bytes:
    return struct.pack("<I", len(payload)) + payload


def encode_client_hello(protocol: int, cols: int, rows: int) -> bytes:
    payload = b"".join(
        (
            encode_varint(0),  # ClientMessage::Hello
            encode_varint(protocol),
            encode_varint(cols),
            encode_varint(rows),
            encode_varint(0),  # cell width in pixels
            encode_varint(0),  # cell height in pixels
            encode_varint(1),  # RenderEncoding::TerminalAnsi
            encode_varint(0),  # ClientKeybindings::Server
            encode_varint(0),  # ClientLaunchMode::App
        )
    )
    return frame_payload(payload)


def decode_server_message(payload: bytes) -> dict[str, object]:
    variant, offset = decode_varint(payload)
    if variant == 0:
        version, offset = decode_varint(payload, offset)
        encoding, offset = decode_varint(payload, offset)
        if offset >= len(payload):
            raise ValueError("welcome message has no error option tag")
        option_tag = payload[offset]
        offset += 1
        error = None
        if option_tag == 1:
            length, offset = decode_varint(payload, offset)
            end = offset + length
            if end > len(payload):
                raise ValueError("welcome error string is truncated")
            error = payload[offset:end].decode("utf-8")
            offset = end
        elif option_tag != 0:
            raise ValueError(f"invalid welcome option tag {option_tag}")
        if offset != len(payload):
            raise ValueError("welcome message has trailing bytes")
        return {
            "kind": "welcome",
            "version": version,
            "encoding": encoding,
            "error": error,
        }
    if variant == 2:
        seq, offset = decode_varint(payload, offset)
        width, offset = decode_varint(payload, offset)
        height, offset = decode_varint(payload, offset)
        if offset >= len(payload) or payload[offset] not in (0, 1):
            raise ValueError("terminal message has an invalid full flag")
        full = bool(payload[offset])
        offset += 1
        length, offset = decode_varint(payload, offset)
        end = offset + length
        if end != len(payload):
            raise ValueError("terminal message bytes are truncated or have trailing data")
        return {
            "kind": "terminal",
            "seq": seq,
            "width": width,
            "height": height,
            "full": full,
            "bytes": payload[offset:end],
        }
    return {"kind": "other", "variant": variant, "payload": payload[offset:]}


def read_framed(stream: socket.socket, timeout: float) -> bytes:
    previous_timeout = stream.gettimeout()
    stream.settimeout(timeout)

    def read_exact(length: int) -> bytes:
        chunks = bytearray()
        while len(chunks) < length:
            chunk = stream.recv(length - len(chunks))
            if not chunk:
                raise BenchmarkError("client protocol socket closed mid-frame")
            chunks.extend(chunk)
        return bytes(chunks)

    try:
        length = struct.unpack("<I", read_exact(4))[0]
        if length == 0 or length > MAX_WIRE_FRAME_BYTES:
            raise BenchmarkError(f"invalid client protocol frame length: {length}")
        return read_exact(length)
    except TimeoutError as error:
        raise BenchmarkError("timed out waiting for a client protocol frame") from error
    finally:
        stream.settimeout(previous_timeout)


def encode_client_input(data: bytes) -> bytes:
    return frame_payload(encode_varint(1) + encode_varint(len(data)) + data)


def encode_client_detach() -> bytes:
    return frame_payload(encode_varint(4))


def terminal_text(ansi: bytes) -> bytes:
    without_osc = OSC_SEQUENCE.sub(b"", ansi)
    without_csi = CSI_SEQUENCE.sub(b"", without_osc)
    return SHORT_ESCAPE.sub(b"", without_csi)


def render_probe_command(marker: str) -> bytes:
    if re.fullmatch(r"[A-Z0-9_]+", marker) is None:
        raise ValueError("render marker must contain only uppercase ASCII, digits, and underscores")
    return f"printf '\\033[2J\\033[H{marker}\\n'\n".encode("ascii")


def render_probe_marker(sample_index: int) -> str:
    return ("A" if sample_index % 2 == 0 else "B") * 48


def isolated_environment(
    base: Path, inherited: Mapping[str, str] | None = None
) -> dict[str, str]:
    env = dict(os.environ if inherited is None else inherited)
    for name in ("NAGI_SESSION", "NAGI_CLIENT_SOCKET_PATH", "NAGI_ENV"):
        env.pop(name, None)
    env.update(
        {
            "XDG_CONFIG_HOME": str(base / "config"),
            "XDG_RUNTIME_DIR": str(base / "runtime"),
            "XDG_STATE_HOME": str(base / "state"),
            "NAGI_CONFIG_PATH": str(base / "config.toml"),
            "NAGI_SOCKET_PATH": str(base / "runtime/nagi.sock"),
            "NAGI_DISABLE_SOUND": "1",
        }
    )
    return env


def _process_group_pids(process_group: int) -> list[int]:
    completed = subprocess.run(
        ["ps", "-axo", "pid=,pgid="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        raise BenchmarkError(f"cannot enumerate process group: {completed.stderr.strip()}")
    pids = []
    for line in completed.stdout.splitlines():
        fields = line.split()
        if len(fields) != 2:
            continue
        try:
            pid, pgid = (int(field) for field in fields)
        except ValueError:
            continue
        if pgid == process_group:
            pids.append(pid)
    return sorted(pids)


def _process_tree_pids(root_pid: int) -> list[int]:
    completed = subprocess.run(
        ["ps", "-axo", "pid=,ppid="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        raise BenchmarkError(f"cannot enumerate process tree: {completed.stderr.strip()}")
    children: dict[int, list[int]] = {}
    for line in completed.stdout.splitlines():
        fields = line.split()
        if len(fields) != 2:
            continue
        try:
            pid, parent_pid = (int(field) for field in fields)
        except ValueError:
            continue
        children.setdefault(parent_pid, []).append(pid)
    found = []
    pending = [root_pid]
    while pending:
        pid = pending.pop()
        if pid in found:
            continue
        found.append(pid)
        pending.extend(children.get(pid, ()))
    return sorted(found)


def _linux_process_usage(pid: int) -> tuple[float, int]:
    stat = Path(f"/proc/{pid}/stat").read_text()
    close_paren = stat.rfind(")")
    if close_paren < 0:
        raise OSError(f"cannot parse /proc/{pid}/stat")
    fields = stat[close_paren + 2 :].split()
    clock_ticks = os.sysconf("SC_CLK_TCK")
    cpu_seconds = (int(fields[11]) + int(fields[12])) / clock_ticks
    resident_pages = int(fields[21])
    rss_bytes = resident_pages * os.sysconf("SC_PAGE_SIZE")
    return cpu_seconds, rss_bytes


def _macos_process_usage(pid: int) -> tuple[float, int]:
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    buffer = ctypes.create_string_buffer(256)
    # RUSAGE_INFO_V2 has nanosecond user/system times and current RSS.
    if libproc.proc_pid_rusage(pid, 2, ctypes.byref(buffer)) != 0:
        errno = ctypes.get_errno()
        raise OSError(errno, os.strerror(errno))
    raw = buffer.raw
    user_ns = struct.unpack_from("=Q", raw, 16)[0]
    system_ns = struct.unpack_from("=Q", raw, 24)[0]
    resident_bytes = struct.unpack_from("=Q", raw, 64)[0]
    return (user_ns + system_ns) / 1_000_000_000, resident_bytes


def _parse_ps_cpu_time(value: str) -> float:
    day_split = value.strip().split("-", 1)
    days = int(day_split[0]) if len(day_split) == 2 else 0
    clock = day_split[-1].split(":")
    if len(clock) == 3:
        hours, minutes, seconds = clock
    elif len(clock) == 2:
        hours = "0"
        minutes, seconds = clock
    else:
        hours = minutes = "0"
        seconds = clock[0]
    return days * 86400 + int(hours) * 3600 + int(minutes) * 60 + float(seconds)


def _fallback_process_usage(pid: int) -> tuple[float, int]:
    completed = subprocess.run(
        ["ps", "-o", "time=", "-o", "rss=", "-p", str(pid)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    fields = completed.stdout.split()
    if completed.returncode != 0 or len(fields) != 2:
        raise OSError(f"cannot inspect process {pid}")
    return _parse_ps_cpu_time(fields[0]), int(fields[1]) * 1024


def _process_usage(pid: int) -> tuple[float, int]:
    system = platform.system()
    if system == "Linux":
        return _linux_process_usage(pid)
    if system == "Darwin":
        return _macos_process_usage(pid)
    return _fallback_process_usage(pid)


def _snapshot_pids(pids: Sequence[int], context: str) -> dict[str, object]:
    usages: dict[int, tuple[float, int]] = {}
    for pid in pids:
        try:
            usages[pid] = _process_usage(pid)
        except (OSError, ProcessLookupError, FileNotFoundError):
            continue
    if not usages:
        raise BenchmarkError(f"{context} has no inspectable processes")
    return {
        "pids": sorted(usages),
        "cpu_seconds": sum(cpu for cpu, _rss in usages.values()),
        "rss_bytes": sum(rss for _cpu, rss in usages.values()),
        "rss_by_pid": {pid: rss for pid, (_cpu, rss) in usages.items()},
    }


def process_group_snapshot(process_group: int) -> dict[str, object]:
    return _snapshot_pids(
        _process_group_pids(process_group), f"process group {process_group}"
    )


def process_tree_snapshot(root_pid: int) -> dict[str, object]:
    return _snapshot_pids(_process_tree_pids(root_pid), f"process tree {root_pid}")


def _socket_ping(path: Path, timeout: float) -> bool:
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(timeout)
            stream.connect(str(path))
            stream.sendall(b'{"id":"bench-ready","method":"ping","params":{}}\n')
            response = bytearray()
            while not response.endswith(b"\n"):
                chunk = stream.recv(4096)
                if not chunk:
                    return False
                response.extend(chunk)
            payload = json.loads(response)
            return payload.get("result", {}).get("type") == "pong"
    except (OSError, TimeoutError, json.JSONDecodeError):
        return False


class NagiRuntime:
    def __init__(self, binary: Path, timeout: float, repo_root: Path):
        temp_parent = Path("/tmp") if Path("/tmp").is_dir() else None
        self._temporary = tempfile.TemporaryDirectory(prefix="nagi-bench-", dir=temp_parent)
        self.base = Path(self._temporary.name)
        self.binary = binary
        self.timeout = timeout
        self.repo_root = repo_root
        self.workspace = self.base / "workspace"
        for directory in (
            self.base / "config",
            self.base / "runtime",
            self.base / "state",
            self.workspace,
        ):
            directory.mkdir(parents=True, exist_ok=True)
        (self.base / "config.toml").write_text(
            "onboarding = false\n"
            "[update]\n"
            "version_check = false\n"
            "manifest_check = false\n"
            "[terminal]\n"
            "shell_mode = \"non_login\"\n"
        )
        self.env = isolated_environment(self.base)
        self.env["SHELL"] = "/bin/sh"
        self.api_socket = Path(self.env["NAGI_SOCKET_PATH"])
        self.client_socket = self.api_socket.with_name(f"{self.api_socket.stem}-client.sock")
        self.process: subprocess.Popen[bytes] | None = None
        self._log = None

    def __enter__(self) -> "NagiRuntime":
        return self

    def __exit__(self, _type, _value, _traceback) -> None:
        self.stop()
        self._temporary.cleanup()

    def start(self) -> float:
        if self.process is not None:
            raise BenchmarkError("runtime was already started")
        self._log = (self.base / "server.log").open("wb")
        started_ns = time.perf_counter_ns()
        self.process = subprocess.Popen(
            [str(self.binary), "server"],
            cwd=self.workspace,
            env=self.env,
            stdin=subprocess.DEVNULL,
            stdout=self._log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise BenchmarkError(
                    f"server exited before readiness with code {self.process.returncode}: "
                    f"{self._log_tail()}"
                )
            api_ready = _socket_ping(self.api_socket, min(0.2, self.timeout))
            if api_ready and self._client_accepts_connection():
                return (time.perf_counter_ns() - started_ns) / 1_000_000
            time.sleep(0.005)
        raise BenchmarkError(
            f"server did not become ready within {self.timeout:.3f}s: {self._log_tail()}"
        )

    @property
    def pid(self) -> int:
        if self.process is None:
            raise BenchmarkError("runtime is not started")
        return self.process.pid

    def _client_accepts_connection(self) -> bool:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                stream.settimeout(min(0.2, self.timeout))
                stream.connect(str(self.client_socket))
            return True
        except OSError:
            return False

    def _log_tail(self) -> str:
        if self._log is not None:
            self._log.flush()
        path = self.base / "server.log"
        if not path.exists():
            return "no server log"
        return path.read_text(errors="replace")[-2000:].strip() or "empty server log"

    def run_cli(self, *arguments: str) -> dict[str, object]:
        try:
            completed = subprocess.run(
                [str(self.binary), *arguments],
                cwd=self.workspace,
                env=self.env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=self.timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise BenchmarkError(f"`nagi {' '.join(arguments)}` timed out") from error
        if completed.returncode != 0:
            detail = completed.stderr.strip() or completed.stdout.strip()
            raise BenchmarkError(
                f"`nagi {' '.join(arguments)}` failed with code {completed.returncode}: {detail}"
            )
        try:
            return json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise BenchmarkError(
                f"`nagi {' '.join(arguments)}` returned invalid JSON: {completed.stdout!r}"
            ) from error

    def pane_count(self) -> int:
        response = self.run_cli("pane", "list")
        panes = response.get("result", {}).get("panes")
        if not isinstance(panes, list):
            raise BenchmarkError(f"pane.list returned an unexpected payload: {response}")
        return len(panes)

    def configure_panes(self, pane_count: int) -> None:
        current = self.pane_count()
        if current == 0:
            self.run_cli(
                "workspace",
                "create",
                "--cwd",
                str(self.workspace),
                "--label",
                "benchmark",
                "--focus",
            )
            current = self.pane_count()
        if current > pane_count:
            raise BenchmarkError(
                f"isolated runtime unexpectedly started with {current} panes, requested {pane_count}"
            )
        while current < pane_count:
            direction = "right" if current % 2 else "down"
            self.run_cli(
                "pane",
                "split",
                "--current",
                "--direction",
                direction,
                "--ratio",
                "0.5",
                "--cwd",
                str(self.workspace),
                "--no-focus",
            )
            current = self.pane_count()
        if current != pane_count:
            raise BenchmarkError(f"requested {pane_count} panes, runtime has {current}")

    def stop(self) -> None:
        process = self.process
        if process is None:
            return
        if process.poll() is None:
            try:
                subprocess.run(
                    [str(self.binary), "server", "stop"],
                    cwd=self.workspace,
                    env=self.env,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=min(5.0, self.timeout),
                    check=False,
                )
            except subprocess.TimeoutExpired:
                pass
        try:
            process.wait(timeout=min(5.0, self.timeout))
        except subprocess.TimeoutExpired:
            self._signal_group(signal.SIGTERM)
            try:
                process.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                self._signal_group(signal.SIGKILL)
                process.wait(timeout=2.0)
        if self._log is not None:
            self._log.close()
            self._log = None
        self.process = None

    def _signal_group(self, signal_number: int) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        try:
            os.killpg(self.process.pid, signal_number)
        except ProcessLookupError:
            pass


def _read_until_terminal(stream: socket.socket, timeout: float) -> dict[str, object]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        message = decode_server_message(read_framed(stream, remaining))
        if message["kind"] == "terminal":
            return message
    raise BenchmarkError("timed out waiting for a rendered terminal frame")


def _connect_render_client(
    runtime: NagiRuntime, protocol: int, cols: int, rows: int
) -> tuple[socket.socket, float]:
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.settimeout(runtime.timeout)
    started_ns = time.perf_counter_ns()
    try:
        stream.connect(str(runtime.client_socket))
        stream.sendall(encode_client_hello(protocol, cols, rows))
        welcome = decode_server_message(read_framed(stream, runtime.timeout))
        if welcome["kind"] != "welcome":
            raise BenchmarkError(f"expected Welcome, received {welcome['kind']}")
        if welcome["error"] is not None:
            raise BenchmarkError(f"client handshake was rejected: {welcome['error']}")
        if welcome["version"] != protocol or welcome["encoding"] != 1:
            raise BenchmarkError(f"unexpected client negotiation: {welcome}")
        _read_until_terminal(stream, runtime.timeout)
        reattach_ms = (time.perf_counter_ns() - started_ns) / 1_000_000
        return stream, reattach_ms
    except Exception:
        stream.close()
        raise


def _drain_render_frames(stream: socket.socket) -> None:
    while True:
        readable, _, _ = select.select([stream], [], [], 0.02)
        if not readable:
            return
        read_framed(stream, 0.2)


def _detach_render_client(stream: socket.socket) -> None:
    try:
        stream.sendall(encode_client_detach())
    except OSError:
        pass
    finally:
        stream.close()


def measure_reattach_samples(
    runtime: NagiRuntime,
    protocol: int,
    cols: int,
    rows: int,
    warmups: int,
    sample_count: int,
) -> list[float]:
    samples = []
    for index in range(warmups + sample_count):
        try:
            stream, reattach_ms = _connect_render_client(runtime, protocol, cols, rows)
        except BenchmarkError as error:
            raise BenchmarkError(f"reattach sample {index + 1} failed: {error}") from error
        _detach_render_client(stream)
        if index >= warmups:
            samples.append(reattach_ms)
        # Let the server reconcile foreground ownership before the next attach.
        time.sleep(0.05)
    return samples


def measure_render_samples(
    runtime: NagiRuntime,
    protocol: int,
    cols: int,
    rows: int,
    warmups: int,
    sample_count: int,
) -> list[float]:
    stream, _reattach_ms = _connect_render_client(runtime, protocol, cols, rows)
    samples = []
    try:
        _drain_render_frames(stream)
        for index in range(warmups + sample_count):
            marker = render_probe_marker(index)
            command = render_probe_command(marker)
            started_ns = time.perf_counter_ns()
            stream.sendall(encode_client_input(command))
            deadline = time.monotonic() + runtime.timeout
            output = bytearray()
            marker_bytes = marker.encode()
            while time.monotonic() < deadline:
                remaining = deadline - time.monotonic()
                try:
                    message = decode_server_message(read_framed(stream, remaining))
                except BenchmarkError as error:
                    raise BenchmarkError(
                        f"render sample {index + 1} failed: {error}"
                    ) from error
                if message["kind"] != "terminal":
                    continue
                output.extend(message["bytes"])
                if marker_bytes in terminal_text(output):
                    render_ms = (time.perf_counter_ns() - started_ns) / 1_000_000
                    if index >= warmups:
                        samples.append(render_ms)
                    # The marker can first appear in shell echo. Drain the command's
                    # remaining output outside the timed interval before the next input.
                    _drain_render_frames(stream)
                    break
            else:
                raise BenchmarkError(
                    f"render sample {index + 1} did not contain its causality marker "
                    f"within {runtime.timeout:.3f}s"
                )
    finally:
        _detach_render_client(stream)
    return samples


def measure_startup_samples(
    binary: Path,
    repo_root: Path,
    timeout: float,
    warmups: int,
    sample_count: int,
) -> list[float]:
    samples = []
    for index in range(warmups + sample_count):
        with NagiRuntime(binary, timeout, repo_root) as runtime:
            startup_ms = runtime.start()
        if index >= warmups:
            samples.append(startup_ms)
    return samples


def measure_idle(
    root_pid: int,
    server_pid: int,
    settle_seconds: float,
    sample_seconds: float,
) -> dict[str, float]:
    time.sleep(settle_seconds)
    before = process_tree_snapshot(root_pid)
    started = time.perf_counter()
    time.sleep(sample_seconds)
    elapsed = time.perf_counter() - started
    after = process_tree_snapshot(root_pid)
    cpu_delta = max(0.0, float(after["cpu_seconds"]) - float(before["cpu_seconds"]))
    rss_by_pid = after["rss_by_pid"]
    server_rss = int(rss_by_pid.get(server_pid, 0))
    return {
        "idle_cpu_percent": round(cpu_delta / elapsed * 100, 3),
        "server_rss_mib": round(server_rss / (1024 * 1024), 3),
        "process_tree_rss_mib": round(int(after["rss_bytes"]) / (1024 * 1024), 3),
        "observed_seconds": round(elapsed, 3),
    }


def _command_text(arguments: Sequence[str], cwd: Path | None = None) -> str:
    try:
        completed = subprocess.run(
            list(arguments),
            cwd=cwd,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=5.0,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unavailable"
    return completed.stdout.strip() if completed.returncode == 0 else "unavailable"


def _cpu_model() -> str:
    if platform.system() == "Darwin":
        model = _command_text(("sysctl", "-n", "machdep.cpu.brand_string"))
        if model != "unavailable":
            return model
    if platform.system() == "Linux":
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.lower().startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
    return platform.processor() or "unknown"


def _physical_memory_bytes() -> int | None:
    if platform.system() == "Darwin":
        value = _command_text(("sysctl", "-n", "hw.memsize"))
        return int(value) if value.isdigit() else None
    try:
        return os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")
    except (ValueError, OSError):
        return None


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _git_metadata(repo_root: Path) -> dict[str, object]:
    commit = _command_text(("git", "rev-parse", "HEAD"), cwd=repo_root)
    dirty_status = _command_text(
        ("git", "status", "--porcelain", "--untracked-files=normal"), cwd=repo_root
    )
    return {
        "root": str(repo_root.resolve()),
        "commit": commit,
        "dirty": dirty_status not in ("", "unavailable"),
    }


def binary_metadata(binary: Path) -> dict[str, object]:
    clean_env = dict(os.environ)
    for name in ("NAGI_SESSION", "NAGI_SOCKET_PATH", "NAGI_CLIENT_SOCKET_PATH"):
        clean_env.pop(name, None)
    version = subprocess.run(
        [str(binary), "--version"],
        env=clean_env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=5.0,
        check=False,
    )
    status = subprocess.run(
        [str(binary), "status", "client", "--json"],
        env=clean_env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=5.0,
        check=False,
    )
    if version.returncode != 0 or status.returncode != 0:
        detail = version.stderr.strip() or status.stderr.strip()
        raise BenchmarkError(f"prebuilt binary identity check failed: {detail}")
    try:
        status_json = json.loads(status.stdout)
        protocol = int(status_json["protocol"])
    except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        raise BenchmarkError(f"cannot read protocol from `{binary} status client --json`") from error
    return {
        "path": str(binary),
        "version": version.stdout.strip(),
        "protocol": protocol,
        "size_bytes": binary.stat().st_size,
        "sha256": _sha256(binary),
    }


def collect_metadata(binary: Path, repo_root: Path) -> dict[str, object]:
    return {
        "recorded_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "repository": _git_metadata(repo_root),
        "binary": binary_metadata(binary),
        "system": {
            "os": platform.system(),
            "os_release": platform.release(),
            "machine": platform.machine(),
            "cpu": _cpu_model(),
            "logical_cpus": os.cpu_count(),
            "physical_memory_bytes": _physical_memory_bytes(),
        },
        "toolchain": {
            "python": platform.python_version(),
            "rustc": _command_text(("rustc", "--version")),
            "cargo": _command_text(("cargo", "--version")),
            "zig": _command_text((os.environ.get("ZIG", "zig"), "version")),
        },
    }


def format_human(report: Mapping[str, object]) -> str:
    metadata = report["metadata"]
    repository = metadata["repository"]
    binary = metadata["binary"]
    system = metadata["system"]
    scenario = report["scenario"]
    results = report["results"]

    def latency_line(label: str, key: str) -> str:
        stats = results[key]
        return (
            f"{label:<18} p50 {stats['p50']:.3f} ms  "
            f"p95 {stats['p95']:.3f} ms  max {stats['max']:.3f} ms"
        )

    dirty = " dirty" if repository["dirty"] else ""
    lines = [
        f"Nagi benchmark: {str(report['status']).upper()}",
        f"commit: {repository['commit']}{dirty}",
        f"binary: {binary['path']} ({binary['version']})",
        f"host: {system['os']} {system['machine']}, {system['cpu']}",
        (
            f"scenario: {scenario['cols']}x{scenario['rows']}, {scenario['panes']} pane(s), "
            f"{scenario['startup_samples']} startup + {scenario['render_samples']} render samples, "
            "compilation excluded"
        ),
        "",
        latency_line("startup", "startup_ms"),
        latency_line("render", "render_latency_ms"),
        latency_line("warm reattach", "reattach_ms"),
        f"idle CPU          {results['idle_cpu_percent']:.3f}%",
        (
            f"resident memory    server {results['server_rss_mib']:.3f} MiB  "
            f"process tree {results['process_tree_rss_mib']:.3f} MiB"
        ),
    ]
    failures = report.get("failures", [])
    if failures:
        lines.extend(("", "ceiling failures:"))
        lines.extend(
            f"  {failure['metric']}: {failure['measured']} > {failure['ceiling']}"
            for failure in failures
        )
    return "\n".join(lines) + "\n"


def run_benchmark(args: argparse.Namespace, binary: Path) -> dict[str, object]:
    repo_root = args.repo_root.expanduser().resolve()
    metadata = collect_metadata(binary, repo_root)
    protocol = int(metadata["binary"]["protocol"])
    startup_samples = measure_startup_samples(
        binary,
        repo_root,
        args.timeout_seconds,
        args.warmups,
        args.startup_samples,
    )

    with NagiRuntime(binary, args.timeout_seconds, repo_root) as runtime:
        runtime.start()
        runtime.configure_panes(args.panes)
        reattach_samples = measure_reattach_samples(
            runtime,
            protocol,
            args.cols,
            args.rows,
            args.warmups,
            args.render_samples,
        )
        render_samples = measure_render_samples(
            runtime,
            protocol,
            args.cols,
            args.rows,
            args.warmups,
            args.render_samples,
        )
        idle = measure_idle(
            runtime.pid,
            runtime.pid,
            args.idle_settle_seconds,
            args.idle_sample_seconds,
        )

    results = {
        "startup_ms": summarize_samples(startup_samples),
        "render_latency_ms": summarize_samples(render_samples),
        "reattach_ms": summarize_samples(reattach_samples),
        "idle_cpu_percent": idle["idle_cpu_percent"],
        "server_rss_mib": idle["server_rss_mib"],
        "process_tree_rss_mib": idle["process_tree_rss_mib"],
        "idle_observed_seconds": idle["observed_seconds"],
    }
    ceilings = {
        "startup_p95_ms": args.startup_ceiling_ms,
        "render_p95_ms": args.render_ceiling_ms,
        "reattach_p95_ms": args.reattach_ceiling_ms,
        "idle_cpu_percent": args.idle_cpu_ceiling_percent,
        "process_tree_rss_mib": args.rss_ceiling_mib,
    }
    failures = evaluate_ceilings(results, ceilings)
    return {
        "schema": 1,
        "status": "fail" if failures else "pass",
        "metadata": metadata,
        "scenario": {
            "startup_definition": (
                "process spawn to responsive JSON API and accepting client socket, "
                "fresh isolated XDG directories and empty project"
            ),
            "render_definition": (
                "client input write to terminal-ANSI frame containing a unique causality marker"
            ),
            "reattach_definition": (
                "Unix client socket connect through handshake to first terminal-ANSI frame"
            ),
            "compilation_included": False,
            "render_encoding": "terminal-ansi",
            "cols": args.cols,
            "rows": args.rows,
            "panes": args.panes,
            "startup_samples": args.startup_samples,
            "render_samples": args.render_samples,
            "warmups": args.warmups,
            "idle_settle_seconds": args.idle_settle_seconds,
            "idle_sample_seconds": args.idle_sample_seconds,
            "timeout_seconds": args.timeout_seconds,
        },
        "results": results,
        "ceilings": ceilings,
        "failures": failures,
    }


def _emit_error(message: str, output_format: str) -> None:
    if output_format == "json":
        print(json.dumps({"schema": 1, "status": "error", "error": message}))
    else:
        print(f"error: {message}", file=sys.stderr)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    binary = args.binary.expanduser().resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        message = (
            f"prebuilt binary is missing or not executable: {binary}; "
            "run `just bench-build` before measuring"
        )
        _emit_error(message, args.output_format)
        return 2
    try:
        report = run_benchmark(args, binary)
    except (BenchmarkError, OSError, subprocess.SubprocessError) as error:
        _emit_error(str(error), args.output_format)
        return 2
    if args.output_format == "json":
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(format_human(report), end="")
    return 1 if report["failures"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
