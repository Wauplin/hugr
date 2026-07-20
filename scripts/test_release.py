from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import release


class ReleaseTest(unittest.TestCase):
    def test_version_bumps(self) -> None:
        self.assertEqual(release.next_version("0.0.2", "none"), "0.0.2")
        self.assertEqual(release.next_version("0.0.2", "patch"), "0.0.3")
        self.assertEqual(release.next_version("0.0.2", "minor"), "0.1.0")
        self.assertEqual(release.next_version("0.0.2", "major"), "1.0.0")

    def test_set_updates_workspace_and_exact_dependencies(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = release.ROOT / "Cargo.toml"
            (root / "Cargo.toml").write_bytes(source.read_bytes())
            release.set_version("1.2.3", root)
            self.assertEqual(release.workspace_version(root), "1.2.3")
            rendered = (root / "Cargo.toml").read_text()
            for name in release.CRATES:
                self.assertIn(f'{name} = {{ version = "=1.2.3",', rendered)

    def test_rejects_non_release_versions(self) -> None:
        with self.assertRaises(ValueError):
            release.next_version("1.2.3-beta.1", "patch")
        with self.assertRaises(ValueError):
            release.set_version("v1.2.3")


if __name__ == "__main__":
    unittest.main()
