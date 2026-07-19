from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
DISABLED_WORKFLOWS = (
    "approve-contributor.yml",
    "approve-merged-contributor.yml",
    "build-artifacts-manual.yml",
    "issue-gate.yml",
    "label-next-release-issues.yml",
    "pr-gate.yml",
    "preview.yml",
)


class ForkSafetyTests(unittest.TestCase):
    def test_upstream_automation_cannot_publish_or_mutate_the_fork(self) -> None:
        for name in DISABLED_WORKFLOWS:
            workflow = ROOT / ".github" / "workflows" / name
            with self.subTest(workflow=name):
                if workflow.exists():
                    self.assertIn("\non: []\n", workflow.read_text())

    def test_upstream_update_channels_are_disabled_in_code(self) -> None:
        self.assertIn(
            "const FORK_RELEASE_CHANNELS_CONFIGURED: bool = false;",
            (ROOT / "src" / "update.rs").read_text(),
        )
        self.assertIn(
            "const FORK_REMOTE_RELEASE_CHANNEL_CONFIGURED: bool = false;",
            (ROOT / "src" / "remote" / "unix.rs").read_text(),
        )

    def test_nagi_release_is_repository_scoped_and_requires_signed_payloads(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "release.yml").read_text()
        self.assertEqual(
            workflow.count("if: github.repository == 'Cod-Hash-Studios/nagi'"),
            2,
        )
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("cosign sign-blob --yes --bundle", workflow)
        self.assertIn("--require-signatures", workflow)
        self.assertIn("actions/attest@", workflow)
        self.assertNotIn("ogulcancelik", workflow.lower())
        self.assertNotIn("rm -rf", workflow)

    def test_fork_attribution_pins_the_exact_upstream_base(self) -> None:
        notice = (ROOT / "FORK.md").read_text()
        self.assertIn("AGPL-3.0-or-later", notice)
        self.assertIn("50aaa2ec046ee26ff407c20f49de496f522512a8", notice)
        self.assertIn("https://github.com/ogulcancelik/herdr", notice)


if __name__ == "__main__":
    unittest.main()
