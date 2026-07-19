#!/usr/bin/env python3
"""Run Nagi's process-boundary crash, provider, and plugin chaos gates."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
CASES = ("mission_recovery", "provider_recovery", "plugin_isolation")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", choices=CASES, action="append")
    parser.add_argument("--repetitions", type=int, default=1)
    args = parser.parse_args()
    if args.repetitions < 1 or args.repetitions > 20:
        parser.error("--repetitions must be between 1 and 20")

    env = os.environ.copy()
    zig = Path("/opt/homebrew/opt/zig@0.15/bin/zig")
    if zig.exists():
        env.setdefault("ZIG", str(zig))
    cases = args.case or list(CASES)
    for repetition in range(1, args.repetitions + 1):
        print(f"chaos repetition {repetition}/{args.repetitions}: {', '.join(cases)}")
        command = ["cargo", "test", "--locked"]
        for case in cases:
            command.extend(["--test", case])
        command.extend(["--", "--test-threads=1"])
        result = subprocess.run(command, cwd=ROOT, env=env, check=False)
        if result.returncode != 0:
            return result.returncode
    return 0


if __name__ == "__main__":
    sys.exit(main())
