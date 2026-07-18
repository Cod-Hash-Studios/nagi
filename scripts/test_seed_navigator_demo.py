import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "seed_navigator_demo.sh"


class SeedNavigatorDemoTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        self.config_home = self.root / "config"
        self.main_socket = self.config_home / "nagi" / "nagi.sock"
        self.main_socket.parent.mkdir(parents=True)
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(str(self.main_socket))

        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        dirname = shutil.which("dirname")
        if dirname is None:
            self.fail("dirname is required to run the demo script tests")
        (self.bin_dir / "dirname").symlink_to(dirname)

    def tearDown(self) -> None:
        self.server.close()
        self.temp_dir.cleanup()

    def run_script(self, socket_path: Path) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(self.root / "home"),
                "PATH": str(self.bin_dir),
                "XDG_CONFIG_HOME": str(self.config_home),
                "NAGI_NAV_SOCKET_PATH": str(socket_path),
            }
        )
        return subprocess.run(
            ["/bin/bash", str(SCRIPT)],
            cwd=ROOT,
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

    def install_fake_jq(self) -> None:
        jq = self.bin_dir / "jq"
        jq.write_text("#!/bin/sh\nexit 0\n")
        jq.chmod(0o755)

    def test_rejects_aliases_of_the_main_socket_without_opt_in(self) -> None:
        self.install_fake_jq()
        alias = self.config_home / "nagi-dev" / "nagi.sock"
        alias.parent.mkdir(parents=True)
        alias.symlink_to(self.main_socket)

        for socket_path in (alias, self.main_socket.parent / ".." / "nagi" / "nagi.sock"):
            with self.subTest(socket_path=socket_path):
                result = self.run_script(socket_path)
                self.assertEqual(1, result.returncode)
                self.assertIn("refusing to seed main nagi session", result.stderr)
                self.assertIn("--allow-main", result.stderr)

    def test_requires_jq_before_calling_the_nagi_binary(self) -> None:
        dev_socket = self.config_home / "nagi-dev" / "nagi.sock"
        dev_socket.parent.mkdir(parents=True)
        dev_server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(dev_server.close)
        dev_server.bind(str(dev_socket))

        result = self.run_script(dev_socket)

        self.assertEqual(1, result.returncode)
        self.assertIn("required command not found: jq", result.stderr)


if __name__ == "__main__":
    unittest.main()
