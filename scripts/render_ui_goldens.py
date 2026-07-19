#!/usr/bin/env python3
"""Check or intentionally regenerate Nagi's deterministic Ratatui goldens."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--update",
        action="store_true",
        help="replace committed goldens with the current intentional rendering",
    )
    args = parser.parse_args()

    env = os.environ.copy()
    if args.update:
        env["NAGI_UPDATE_GOLDENS"] = "1"
    else:
        env.pop("NAGI_UPDATE_GOLDENS", None)
    zig = Path("/opt/homebrew/opt/zig@0.15/bin/zig")
    if zig.exists():
        env.setdefault("ZIG", str(zig))

    command = [
        "cargo",
        "test",
        "--locked",
        "ui::golden::primary_ui_goldens_match_all_supported_sizes_and_themes",
        "--",
        "--exact",
        "--test-threads=1",
    ]
    result = subprocess.run(command, cwd=ROOT, env=env, check=False)
    if result.returncode != 0:
        return result.returncode
    if args.update:
        print("Updated UI goldens. Review tests/golden before committing.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
