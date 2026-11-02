"""Tests for scripts/dev_version.py.

    python3 -m unittest scripts/test_dev_version.py
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from dev_version import dev_version, is_dev_of  # noqa: E402


def key(version: str):
    """semver's order for the forms used here: a release sorts after its prereleases."""
    release, _, pre = version.partition("-")
    numbers = tuple(int(part) for part in release.split("."))
    return (numbers, 1, 0) if not pre else (numbers, 0, int(pre.split(".")[1]))


class DevVersion(unittest.TestCase):
    def test_a_main_build_sorts_between_the_last_release_and_the_next(self):
        earlier = dev_version("0.2.1", 1_793_221_000)
        later = dev_version("0.2.1", 1_793_224_600)
        self.assertEqual(later, "0.3.0-dev.20261028215640")
        self.assertLess(key("0.2.1"), key(earlier))
        self.assertLess(key(earlier), key(later))
        self.assertLess(key(later), key("0.3.0"))

    def test_only_the_next_minor_dev_form_counts_as_a_dev_build(self):
        self.assertTrue(is_dev_of("0.3.0-dev.20261029001640", "0.2.1"))
        self.assertFalse(is_dev_of("0.2.2-dev.20261029001640", "0.2.1"))
        self.assertFalse(is_dev_of("0.3.0-rc.1", "0.2.1"))


if __name__ == "__main__":
    unittest.main()
