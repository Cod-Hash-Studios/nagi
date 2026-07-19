import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RELEASE = ROOT / ".github" / "workflows" / "release.yml"
SECURITY = ROOT / ".github" / "workflows" / "security.yml"
CI = ROOT / ".github" / "workflows" / "ci.yml"
AUDIT_POLICY = ROOT / ".cargo" / "audit.toml"


class ReleaseWorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = RELEASE.read_text(encoding="utf-8")
        self.security_workflow = SECURITY.read_text(encoding="utf-8")
        self.ci_workflow = CI.read_text(encoding="utf-8")

    def test_supported_unix_platforms_run_the_complete_nextest_suite(self) -> None:
        self.assertGreaterEqual(self.ci_workflow.count("nextest_filter: all()"), 2)
        self.assertNotIn("not binary(live_handoff)", self.ci_workflow)

    def test_cross_builds_install_the_required_platform_toolchains(self) -> None:
        self.assertIn("Install Linux build tools", self.workflow)
        self.assertIn("musl-tools", self.workflow)
        self.assertIn("gcc-aarch64-linux-gnu", self.workflow)
        self.assertIn("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", self.workflow)
        self.assertIn("Install macOS build tools", self.workflow)
        self.assertIn("brew install cmake ninja", self.workflow)

    def test_release_uses_current_bundle_signing_and_verifies_before_publish(self) -> None:
        self.assertIn(
            "sigstore/cosign-installer@6f9f17788090df1f26f669e9d70d6ae9567deba6",
            self.workflow,
        )
        self.assertIn("cosign-release: v3.0.6", self.workflow)
        self.assertIn('cosign sign-blob --yes --bundle "$file.sigstore.json" "$file"', self.workflow)
        self.assertIn("--require-signatures", self.workflow)
        self.assertIn("--certificate-identity-regexp", self.workflow)
        self.assertLess(
            self.workflow.index("Sign every release payload"),
            self.workflow.index("Verify signed release payloads"),
        )
        self.assertLess(
            self.workflow.index("Verify signed release payloads"),
            self.workflow.index("Publish GitHub release"),
        )

    def test_release_attests_only_the_unsigned_binary_subjects(self) -> None:
        self.assertIn(
            "actions/attest@a1948c3f048ba23858d222213b7c278aabede763",
            self.workflow,
        )
        self.assertIn("subject-path: |", self.workflow)
        self.assertNotIn('subject-path: "release/nagi-*"', self.workflow)

    def test_security_workflow_uses_pinned_tools_and_repo_audit_policy(self) -> None:
        self.assertIn("cargo-deny@0.20.2,cargo-audit@0.22.2", self.security_workflow)
        self.assertIn("fallback: none", self.security_workflow)
        self.assertIn("cargo deny check bans licenses sources", self.security_workflow)
        self.assertIn("cargo audit --deny warnings", self.security_workflow)
        audit_policy = AUDIT_POLICY.read_text(encoding="utf-8")
        self.assertIn('"RUSTSEC-2025-0141"', audit_policy)
        self.assertIn('"RUSTSEC-2026-0097"', audit_policy)


if __name__ == "__main__":
    unittest.main()
