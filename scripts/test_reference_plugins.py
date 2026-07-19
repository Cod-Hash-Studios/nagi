from pathlib import Path
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
REFERENCE_PLUGINS = (
    "github-lifecycle",
    "dev-services",
    "evidence-exporter",
)


class ReferencePluginContracts(unittest.TestCase):
    def test_manifest_entrypoint_matches_the_cargo_binary_name(self) -> None:
        for plugin_name in REFERENCE_PLUGINS:
            with self.subTest(plugin=plugin_name):
                root = ROOT / "examples" / "plugins" / plugin_name
                cargo = tomllib.loads((root / "Cargo.toml").read_text())
                manifest = tomllib.loads((root / "nagi-plugin.toml").read_text())

                expected = f"{cargo['package']['name']}.wasm"
                actual = Path(manifest["entrypoint"]).name

                self.assertEqual(actual, expected)


if __name__ == "__main__":
    unittest.main()
