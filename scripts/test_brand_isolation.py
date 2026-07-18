from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOTS = (ROOT / "src", ROOT / "tests")
LEGACY_RUNTIME_MARKERS = (
    "HERDR_",
    "herdr.sock",
    "herdr-client.sock",
    ".config/herdr",
    "CARGO_BIN_EXE_herdr",
)


class BrandIsolationTests(unittest.TestCase):
    def test_package_and_runtime_use_the_nagi_namespace(self) -> None:
        cargo = (ROOT / "Cargo.toml").read_text()
        self.assertIn('name = "nagi"', cargo)

        main = (ROOT / "src" / "main.rs").read_text()
        sockets = (ROOT / "src" / "server" / "socket_paths.rs").read_text()
        self.assertIn('"NAGI_ENV"', main)
        self.assertIn('"nagi.log"', main)
        self.assertIn('"NAGI_CLIENT_SOCKET_PATH"', sockets)
        self.assertIn('"nagi-client.sock"', sockets)

    def test_legacy_runtime_namespace_is_absent_from_code_and_tests(self) -> None:
        for root in SOURCE_ROOTS:
            for path in root.rglob("*"):
                if not path.is_file():
                    continue
                text = path.read_text(errors="ignore")
                for marker in LEGACY_RUNTIME_MARKERS:
                    with self.subTest(path=path.relative_to(ROOT), marker=marker):
                        self.assertNotIn(marker, text)

    def test_integration_asset_names_use_the_nagi_namespace(self) -> None:
        assets = ROOT / "src" / "integration" / "assets"
        legacy_names = [path for path in assets.rglob("*") if "herdr" in path.name.lower()]
        self.assertEqual([], legacy_names)


if __name__ == "__main__":
    unittest.main()
