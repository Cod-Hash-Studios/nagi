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
    parser.add_argument(
        "--export-media",
        type=Path,
        metavar="DIRECTORY",
        help="export the styled Ratatui buffers as deterministic SVG product media",
    )
    args = parser.parse_args()

    env = os.environ.copy()
    if args.update:
        env["NAGI_UPDATE_GOLDENS"] = "1"
    else:
        env.pop("NAGI_UPDATE_GOLDENS", None)
    if args.export_media:
        export_directory = args.export_media.expanduser().resolve()
        export_directory.mkdir(parents=True, exist_ok=True)
        env["NAGI_EXPORT_GOLDEN_MEDIA_DIR"] = str(export_directory)
    else:
        env.pop("NAGI_EXPORT_GOLDEN_MEDIA_DIR", None)
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
    if args.export_media:
        print(f"Exported styled UI media to {export_directory}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
