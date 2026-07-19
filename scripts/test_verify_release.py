import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from scripts.assemble_release import TARGETS, assemble
from scripts.verify_release import (
    ReleaseVerificationError,
    verify_release_directory,
    verify_release_signatures,
)


class VerifyReleaseTests(unittest.TestCase):
    def fixture(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        artifact = root / "nagi-macos-aarch64"
        artifact.write_bytes(b"nagi release fixture")
        digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
        (root / "checksums.sha256").write_text(f"{digest}  {artifact.name}\n", encoding="utf-8")
        (root / "provenance.json").write_text(
            json.dumps({
                "schema_version": 1,
                "version": "1.0.0",
                "tag": "v1.0.0",
                "commit": "a" * 40,
                "artifacts": [{"name": artifact.name, "target": "aarch64-apple-darwin", "sha256": digest}],
            }),
            encoding="utf-8",
        )
        (root / "sbom.spdx.json").write_text(
            json.dumps({"spdxVersion": "SPDX-2.3", "packages": [{"name": "nagi", "versionInfo": "1.0.0"}]}),
            encoding="utf-8",
        )
        return temporary, root

    def test_accepts_exact_checksums_target_provenance_and_sbom(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        report = verify_release_directory(root, version="1.0.0", commit="a" * 40)
        self.assertEqual(report["artifacts"], ["nagi-macos-aarch64"])

    def test_rejects_checksum_mismatch(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        (root / "nagi-macos-aarch64").write_bytes(b"tampered")
        with self.assertRaisesRegex(ReleaseVerificationError, "checksum mismatch"):
            verify_release_directory(root, version="1.0.0", commit="a" * 40)

    def test_rejects_wrong_architecture_binding_and_path_escape(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        provenance = json.loads((root / "provenance.json").read_text())
        provenance["artifacts"][0]["target"] = "x86_64-unknown-linux-musl"
        (root / "provenance.json").write_text(json.dumps(provenance), encoding="utf-8")
        with self.assertRaisesRegex(ReleaseVerificationError, "target"):
            verify_release_directory(root, version="1.0.0", commit="a" * 40)

        (root / "checksums.sha256").write_text(f"{'0' * 64}  ../escape\n", encoding="utf-8")
        with self.assertRaisesRegex(ReleaseVerificationError, "unsafe"):
            verify_release_directory(root, version="1.0.0", commit="a" * 40)

    def test_rejects_stale_version_commit_and_incomplete_sbom(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        with self.assertRaisesRegex(ReleaseVerificationError, "version"):
            verify_release_directory(root, version="2.0.0", commit="a" * 40)
        with self.assertRaisesRegex(ReleaseVerificationError, "commit"):
            verify_release_directory(root, version="1.0.0", commit="b" * 40)
        (root / "sbom.spdx.json").write_text('{"spdxVersion":"SPDX-2.3","packages":[]}', encoding="utf-8")
        with self.assertRaisesRegex(ReleaseVerificationError, "packages"):
            verify_release_directory(root, version="1.0.0", commit="a" * 40)

    def test_assembler_produces_a_verifiable_complete_release(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in TARGETS:
                (root / name).write_bytes(name.encode())
            assemble(root, "1.0.0", "a" * 40)

            report = verify_release_directory(root, version="1.0.0", commit="a" * 40)

            self.assertEqual(report["artifacts"], sorted(TARGETS))

    def test_signature_verification_requires_every_bundle_and_exact_identity(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        artifact = root / "nagi-macos-aarch64"
        bundle = artifact.with_name(f"{artifact.name}.sigstore.json")
        bundle.write_text("bundle", encoding="utf-8")
        resolved_artifact = artifact.resolve()
        resolved_bundle = bundle.resolve()
        calls: list[list[str]] = []

        def runner(command: list[str]) -> int:
            calls.append(command)
            return 0

        verify_release_signatures(
            root,
            [artifact.name],
            identity=r"https://github.com/Cod-Hash-Studios/nagi/.github/workflows/release.yml@refs/tags/v.*",
            issuer="https://token.actions.githubusercontent.com",
            runner=runner,
        )
        self.assertEqual(
            calls,
            [[
                "cosign",
                "verify-blob",
                "--bundle",
                str(resolved_bundle),
                "--certificate-identity-regexp",
                r"https://github.com/Cod-Hash-Studios/nagi/.github/workflows/release.yml@refs/tags/v.*",
                "--certificate-oidc-issuer",
                "https://token.actions.githubusercontent.com",
                str(resolved_artifact),
            ]],
        )
        bundle.unlink()
        with self.assertRaisesRegex(ReleaseVerificationError, "signature"):
            verify_release_signatures(root, [artifact.name], identity="id", issuer="issuer", runner=runner)

    def test_signature_verification_rejects_cosign_failure(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        artifact = root / "nagi-macos-aarch64"
        artifact.with_name(f"{artifact.name}.sigstore.json").write_text("bundle", encoding="utf-8")

        with self.assertRaisesRegex(ReleaseVerificationError, "failed"):
            verify_release_signatures(
                root,
                [artifact.name],
                identity="id",
                issuer="issuer",
                runner=lambda _command: 1,
            )


if __name__ == "__main__":
    unittest.main()
