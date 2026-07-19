#!/usr/bin/env python3
"""Exercise Nagi's clean first-run flow through a real pseudo-terminal."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import time


ANSI_ESCAPE = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")


def run(command: list[str], *, cwd: Path, env: dict[str, str], timeout: float = 20) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )


def checked(command: list[str], *, cwd: Path, env: dict[str, str], timeout: float = 20) -> subprocess.CompletedProcess[str]:
    result = run(command, cwd=cwd, env=env, timeout=timeout)
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


class TerminalSession:
    def __init__(self, command: list[str], *, cwd: Path, env: dict[str, str]) -> None:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        self.master = master
        self.output = bytearray()
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)

    def send(self, value: str | bytes) -> None:
        os.write(self.master, value.encode() if isinstance(value, str) else value)

    def wait_for(self, needle: str, *, timeout: float = 20) -> str:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.2)
            if ready:
                try:
                    self.output.extend(os.read(self.master, 65_536))
                except OSError:
                    pass
            rendered = self.rendered_output()
            if needle.casefold() in rendered.casefold():
                return rendered
            if self.process.poll() is not None:
                break
        raise RuntimeError(
            f"timed out waiting for {needle!r}; exit={self.process.poll()}\n"
            f"terminal:\n{self.rendered_output()[-8_000:]}"
        )

    def rendered_output(self) -> str:
        return ANSI_ESCAPE.sub("", self.output.decode("utf-8", errors="ignore"))

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        os.close(self.master)


def write_fake_codex(directory: Path) -> None:
    executable = directory / "codex"
    executable.write_text(
        "#!/bin/sh\n"
        "if [ \"${1:-}\" = \"--version\" ]; then\n"
        "  printf '%s\\n' 'codex-cli 1.0.0'\n"
        "  exit 0\n"
        "fi\n"
        "printf '%s\\n' 'fixture provider running'\n"
        "while :; do sleep 1; done\n",
        encoding="utf-8",
    )
    executable.chmod(0o755)


def initialize_repository(repository: Path, env: dict[str, str]) -> None:
    repository.mkdir(parents=True)
    checked(["git", "init", "--quiet"], cwd=repository, env=env)
    checked(["git", "config", "user.name", "Nagi Smoke"], cwd=repository, env=env)
    checked(["git", "config", "user.email", "smoke@nagi.invalid"], cwd=repository, env=env)
    (repository / "README.md").write_text("# clean first mission\n", encoding="utf-8")
    checked(["git", "add", "README.md"], cwd=repository, env=env)
    checked(["git", "commit", "--quiet", "-m", "test: seed first mission"], cwd=repository, env=env)


def wait_for_mission(binary: Path, *, cwd: Path, env: dict[str, str], title: str) -> str:
    deadline = time.monotonic() + 30
    last = ""
    while time.monotonic() < deadline:
        result = run([str(binary), "mission", "list"], cwd=cwd, env=env, timeout=5)
        last = f"{result.stdout}\n{result.stderr}"
        if result.returncode == 0 and title in last:
            return last
        time.sleep(0.25)
    raise RuntimeError(f"mission did not become visible through the CLI:\n{last}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.expanduser().resolve()
    if not binary.is_file():
        parser.error(f"binary does not exist: {binary}")

    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="nagi-first-", dir="/tmp") as temporary:
        root = Path(temporary)
        config = root / "config"
        fake_bin = root / "bin"
        repository = root / "repo"
        config.mkdir()
        fake_bin.mkdir()
        write_fake_codex(fake_bin)

        env = os.environ.copy()
        for key in tuple(env):
            if key.startswith("NAGI_") or key.startswith("XDG_"):
                env.pop(key)
        env.update(
            {
                "HOME": str(root / "home"),
                "PATH": os.pathsep.join(
                    [str(fake_bin), "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
                ),
                "SHELL": "/bin/sh",
                "TERM": "xterm-256color",
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_CONFIG_HOME": str(config),
                "XDG_STATE_HOME": str(root / "state"),
            }
        )
        (root / "home").mkdir()
        initialize_repository(repository, env)

        version = checked([str(binary), "--version"], cwd=repository, env=env)
        if not version.stdout.startswith("nagi "):
            raise RuntimeError(f"unexpected version output: {version.stdout!r}")
        help_output = checked([str(binary), "--help"], cwd=repository, env=env).stdout
        for command in ("nagi doctor", "nagi mission <subcommand>"):
            if command not in help_output:
                raise RuntimeError(f"root help is missing {command!r}")

        doctor = checked([str(binary), "doctor", "--json"], cwd=repository, env=env)
        report = json.loads(doctor.stdout)
        if not report["ready"] or report["provider_count"] != 1:
            raise RuntimeError(f"unexpected clean doctor report: {doctor.stdout}")
        config_check = next(check for check in report["checks"] if check["id"] == "config")
        if not Path(config_check["detail"]).is_relative_to(config):
            raise RuntimeError("doctor escaped the isolated config directory")

        session = TerminalSession([str(binary)], cwd=repository, env=env)
        title = "Keep the clean install usable"
        try:
            session.wait_for("opens your first mission")
            session.send("\r")
            session.wait_for("What should be true")
            session.send(f"{title}\r")
            session.wait_for("Define acceptance")
            session.send("README remains present\r")
            session.wait_for("Git baseline")
            session.send("\r")
            session.wait_for("Codex")
            session.send("\r")
            session.wait_for("Allow provider writes")
            session.send(" ")
            session.wait_for("[x]")
            session.send("\r")
            wait_for_mission(binary, cwd=repository, env=env, title=title)
        except Exception as error:
            diagnostics = [f"\n--- terminal ---\n{session.rendered_output()[-8_000:]}"]
            for log in sorted(config.rglob("*.log")):
                diagnostics.append(f"\n--- {log.relative_to(config)} ---\n{log.read_text(errors='replace')}")
            raise RuntimeError(f"{error}{''.join(diagnostics)}") from error
        finally:
            session.close()
            run([str(binary), "server", "stop"], cwd=repository, env=env, timeout=10)

    elapsed = time.monotonic() - started
    if elapsed >= 180:
        raise RuntimeError(f"first mission exceeded the 180 second budget: {elapsed:.3f}s")
    print(f"clean first mission passed in {elapsed:.3f}s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
