"""The runtime ships inside Poundlock releases, so it reports Poundlock's version."""

import re
import unittest
from pathlib import Path

import poundlock_llamacpp

ROOT = Path(__file__).resolve().parents[3]


class VersionTests(unittest.TestCase):
    def test_runtime_version_is_poundlocks(self) -> None:
        cargo = (ROOT / "Cargo.toml").read_text()
        poundlock = re.search(r'^version = "([^"]+)"', cargo, re.M).group(1)
        self.assertEqual(poundlock, poundlock_llamacpp.__version__)
        pyproject = (ROOT / "node-runtimes/llamacpp/pyproject.toml").read_text()
        self.assertIn(f'version = "{poundlock}"', pyproject)


if __name__ == "__main__":
    unittest.main()
