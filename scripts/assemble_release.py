#!/usr/bin/env python3
"""Assemble deterministic checksum, provenance, and SPDX metadata for Nagi releases."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import tomllib
from pathlib import Path

TARGETS = {
    "nagi-linux-x86_64": "x86_64-unknown-linux-musl",
    "nagi-linux-aarch64": "aarch64-unknown-linux-musl",
    "nagi-macos-x86_64": "x86_64-apple-darwin",
    "nagi-macos-aarch64": "aarch64-apple-darwin",
    "nagi-windows-x86_64.exe": "x86_64-pc-windows-msvc",
}
ROOT = Path(__file__).resolve().parents[1]


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def assemble(directory: Path, version: str, commit: str) -> None:
    directory = directory.resolve(strict=True)
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?", version):
        raise ValueError("invalid release version")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("invalid release commit")
    artifacts = []
    checksum_lines = []
    for name, target in TARGETS.items():
        path = directory / name
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"missing release artifact: {name}")
        sha256 = digest(path)
        checksum_lines.append(f"{sha256}  {name}\n")
        artifacts.append({"name": name, "target": target, "sha256": sha256, "bytes": path.stat().st_size})
    (directory / "checksums.sha256").write_text("".join(checksum_lines), encoding="utf-8")
    provenance = {
        "schema_version": 1,
        "version": version,
        "tag": f"v{version}",
        "commit": commit,
        "repository": "https://github.com/Cod-Hash-Studios/nagi",
        "artifacts": artifacts,
    }
    (directory / "provenance.json").write_text(
        json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    packages = [
        {"SPDXID": f"SPDXRef-Package-{index}", "name": package["name"], "versionInfo": package["version"]}
        for index, package in enumerate(lock.get("package", []), start=1)
        if isinstance(package, dict) and isinstance(package.get("name"), str) and isinstance(package.get("version"), str)
    ]
    packages.insert(0, {"SPDXID": "SPDXRef-Package-Nagi", "name": "nagi", "versionInfo": version})
    sbom = {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": f"nagi-{version}",
        "documentNamespace": f"https://github.com/Cod-Hash-Studios/nagi/releases/tag/v{version}",
        "creationInfo": {"creators": ["Tool: scripts/assemble_release.py"]},
        "packages": packages,
    }
    (directory / "sbom.spdx.json").write_text(
        json.dumps(sbom, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    try:
        assemble(args.directory, args.version, args.commit)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
