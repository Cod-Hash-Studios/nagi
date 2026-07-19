#!/usr/bin/env python3
"""Fail-closed verification for a locally assembled Nagi release directory."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path
from typing import Any, Callable

TARGETS = {
    "nagi-linux-x86_64": "x86_64-unknown-linux-musl",
    "nagi-linux-aarch64": "aarch64-unknown-linux-musl",
    "nagi-macos-x86_64": "x86_64-apple-darwin",
    "nagi-macos-aarch64": "aarch64-apple-darwin",
    "nagi-windows-x86_64.exe": "x86_64-pc-windows-msvc",
}
SHA256 = re.compile(r"^[0-9a-f]{64}$")
COMMIT = re.compile(r"^[0-9a-f]{40}$")


class ReleaseVerificationError(ValueError):
    pass


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path: Path, limit: int = 4 * 1024 * 1024) -> Any:
    if not path.is_file() or path.stat().st_size > limit:
        raise ReleaseVerificationError(f"{path.name} is missing or oversized")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ReleaseVerificationError(f"{path.name} is invalid JSON: {error}") from error


def read_checksums(path: Path) -> dict[str, str]:
    if not path.is_file() or path.stat().st_size > 64 * 1024:
        raise ReleaseVerificationError("checksums.sha256 is missing or oversized")
    checksums: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+)", line)
        if not match:
            raise ReleaseVerificationError("checksums.sha256 contains an unsafe or malformed entry")
        digest, name = match.groups()
        if name in checksums:
            raise ReleaseVerificationError(f"duplicate checksum entry: {name}")
        checksums[name] = digest
    if not checksums:
        raise ReleaseVerificationError("checksums.sha256 contains no artifacts")
    return checksums


def verify_release_directory(root: Path, *, version: str, commit: str) -> dict[str, Any]:
    root = root.resolve(strict=True)
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?", version):
        raise ReleaseVerificationError("expected version is invalid")
    if not COMMIT.fullmatch(commit):
        raise ReleaseVerificationError("expected commit is invalid")
    checksums = read_checksums(root / "checksums.sha256")
    provenance = read_json(root / "provenance.json")
    sbom = read_json(root / "sbom.spdx.json")
    if not isinstance(provenance, dict) or provenance.get("schema_version") != 1:
        raise ReleaseVerificationError("provenance schema version is invalid")
    if provenance.get("version") != version or provenance.get("tag") != f"v{version}":
        raise ReleaseVerificationError("provenance version or tag does not match")
    if provenance.get("commit") != commit:
        raise ReleaseVerificationError("provenance commit does not match")
    artifacts = provenance.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        raise ReleaseVerificationError("provenance artifacts are missing")

    seen: set[str] = set()
    for item in artifacts:
        if not isinstance(item, dict):
            raise ReleaseVerificationError("provenance artifact is invalid")
        name, target, claimed = item.get("name"), item.get("target"), item.get("sha256")
        if not isinstance(name, str) or name not in TARGETS or name in seen:
            raise ReleaseVerificationError("provenance artifact name is invalid or duplicated")
        if target != TARGETS[name]:
            raise ReleaseVerificationError(f"provenance target does not match {name}")
        if not isinstance(claimed, str) or not SHA256.fullmatch(claimed):
            raise ReleaseVerificationError(f"provenance checksum is invalid for {name}")
        artifact = root / name
        if not artifact.is_file() or artifact.is_symlink():
            raise ReleaseVerificationError(f"release artifact is missing or unsafe: {name}")
        actual = sha256(artifact)
        if checksums.get(name) != actual or claimed != actual:
            raise ReleaseVerificationError(f"checksum mismatch for {name}")
        seen.add(name)
    if set(checksums) != seen:
        raise ReleaseVerificationError("checksum artifact set does not match provenance")

    if not isinstance(sbom, dict) or sbom.get("spdxVersion") != "SPDX-2.3":
        raise ReleaseVerificationError("SBOM must use SPDX-2.3")
    packages = sbom.get("packages")
    if not isinstance(packages, list) or not packages:
        raise ReleaseVerificationError("SBOM packages are missing")
    nagi = next((package for package in packages if isinstance(package, dict) and package.get("name") == "nagi"), None)
    if nagi is None or nagi.get("versionInfo") != version:
        raise ReleaseVerificationError("SBOM Nagi package version does not match")
    return {"version": version, "commit": commit, "artifacts": sorted(seen)}


def verify_release_signatures(
    root: Path,
    names: list[str],
    *,
    identity: str,
    issuer: str,
    runner: Callable[[list[str]], int] | None = None,
) -> None:
    root = root.resolve(strict=True)
    if not identity.strip() or not issuer.strip():
        raise ReleaseVerificationError("signature identity and issuer are required")
    if not names:
        raise ReleaseVerificationError("no signed release files were provided")
    if len(names) != len(set(names)):
        raise ReleaseVerificationError("signed release files contain duplicates")

    if runner is None:
        def run_cosign(command: list[str]) -> int:
            completed = subprocess.run(
                command,
                check=False,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            return completed.returncode

        runner = run_cosign

    for name in names:
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", name):
            raise ReleaseVerificationError(f"unsafe signed release filename: {name}")
        artifact = root / name
        bundle = root / f"{name}.sigstore.json"
        for path, label in (
            (artifact, "release file"),
            (bundle, "signature bundle"),
        ):
            if not path.is_file() or path.is_symlink():
                raise ReleaseVerificationError(f"{label} is missing or unsafe for {name}")
        command = [
            "cosign",
            "verify-blob",
            "--bundle",
            str(bundle),
            "--certificate-identity-regexp",
            identity,
            "--certificate-oidc-issuer",
            issuer,
            str(artifact),
        ]
        if runner(command) != 0:
            raise ReleaseVerificationError(f"signature verification failed for {name}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--require-signatures", action="store_true")
    parser.add_argument("--certificate-identity-regexp")
    parser.add_argument(
        "--certificate-oidc-issuer",
        default="https://token.actions.githubusercontent.com",
    )
    args = parser.parse_args()
    try:
        report = verify_release_directory(args.directory, version=args.version, commit=args.commit)
        if args.require_signatures:
            if not args.certificate_identity_regexp:
                raise ReleaseVerificationError(
                    "--certificate-identity-regexp is required with --require-signatures"
                )
            signed_files = report["artifacts"] + [
                "checksums.sha256",
                "provenance.json",
                "sbom.spdx.json",
            ]
            verify_release_signatures(
                args.directory,
                signed_files,
                identity=args.certificate_identity_regexp,
                issuer=args.certificate_oidc_issuer,
            )
            report["signatures"] = sorted(signed_files)
    except (OSError, ReleaseVerificationError) as error:
        parser.error(str(error))
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
