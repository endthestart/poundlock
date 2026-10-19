"""Put the package under test on the path: `python3 -m unittest discover` needs no install."""

import os
import sys
import unittest
from pathlib import Path

if os.name != "posix":
    raise unittest.SkipTest("the runtime manages POSIX processes (Linux and macOS)")

SOURCE = Path(__file__).resolve().parents[1] / "src"
if str(SOURCE) not in sys.path:
    sys.path.insert(0, str(SOURCE))
